//! Pure control state and killable readers shared with native adapter tests.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use anyhow::{Context, Result, ensure};
use std::{
    io::Read,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

// JSON encodes each image byte as up to three digits and a comma, and
// each text byte as up to six bytes (\u0000), plus result metadata.
const OUTPUT_CAP: usize =
    super::clipboard::MAX_IMAGE * 4 + crate::store::history::MAX_TEXT * 6 + 4096;

// Held only for metadata snapshots and publication, never provider reads.
pub(super) static PUBLICATION: Mutex<()> = Mutex::new(());

pub(super) fn ticket(bytes: &[u8]) -> Option<u128> {
    (bytes.len() == 32 && bytes.iter().all(u8::is_ascii_hexdigit))
        .then(|| {
            std::str::from_utf8(bytes)
                .ok()
                .and_then(|s| u128::from_str_radix(s, 16).ok())
        })
        .flatten()
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MarkerAction {
    Confirmed,
    ReadPayload,
    Invalidate,
}
pub(super) fn marker_action(
    bytes: Option<&[u8]>,
    confirm: impl FnOnce(u128) -> bool,
) -> MarkerAction {
    match bytes.and_then(ticket) {
        Some(ticket) if confirm(ticket) => MarkerAction::Confirmed,
        Some(_) => MarkerAction::ReadPayload,
        None => MarkerAction::Invalidate,
    }
}

#[derive(Default)]
pub(super) struct Policy {
    denied: bool,
    logged: bool,
}
impl Policy {
    // Policy transitions are independent of payload failures/log suppression.
    pub(super) fn update(&mut self, access: isize) -> bool {
        let denied = access == 3;
        let invalidate = denied && !self.denied;
        self.denied = denied;
        if !denied {
            self.logged = false;
        }
        invalidate
    }
    pub(super) fn log_failure(&mut self) -> bool {
        !std::mem::replace(&mut self.logged, true)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct ReadResult<C> {
    pub marker: Option<String>,
    pub content: Option<C>,
    pub fingerprints: Vec<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// Startup/recovery may confirm an active installation, but must never
    /// read an unknown ticket's pre-existing payload as a new local copy.
    InitialMarker,
    Marker,
    Payload,
}

/// The first allowed snapshot is metadata-only, except for install tickets.
/// Forget denied snapshots so recovery can confirm a ticket even when its
/// changeCount was already seen while access was denied.
#[derive(Default)]
pub(super) struct Baseline(Option<isize>);
impl Baseline {
    pub(super) fn snapshot(&mut self, count: isize, denied: bool) -> Option<bool> {
        if denied {
            self.0 = None;
            return None;
        }
        let previous = self.0.replace(count);
        (previous != Some(count)).then_some(previous.is_none())
    }
    pub(super) fn changed(&self, count: isize) -> bool {
        self.0 != Some(count)
    }
}

pub(super) enum Completion<C> {
    Read(ReadResult<C>),
    Unavailable,
    Retired,
}
type Complete<C> = Box<dyn FnOnce(u64, Completion<C>) + Send>;

/// One changeCount observation, including the actor token whose policy was
/// frozen before either provider read. The actor owns that policy; advancing
/// from marker to payload must never observe again or extend the read deadline.
/// Every exit reaps the reader before issuing exactly one actor completion.
pub(super) struct Change<C> {
    count: isize,
    token: u64,
    canceled: Arc<AtomicBool>,
    phase: Phase,
    deadline: Instant,
    reader: Option<Reader<ReadResult<C>>>,
    complete: Option<Complete<C>>,
}
impl<C: serde::de::DeserializeOwned + Send + 'static> Change<C> {
    pub(super) fn new(
        count: isize,
        token: u64,
        canceled: Arc<AtomicBool>,
        phase: Phase,
        timeout: Duration,
        complete: impl FnOnce(u64, Completion<C>) + Send + 'static,
    ) -> Self {
        Self {
            count,
            token,
            canceled,
            phase,
            deadline: Instant::now() + timeout,
            reader: None,
            complete: Some(Box::new(complete)),
        }
    }

    pub(super) fn start(
        mut self,
        command: impl FnOnce(isize, Phase) -> Result<Command>,
    ) -> Result<Self> {
        if let Err(error) = self.begin_read(command) {
            self.finish(Completion::Unavailable);
            return Err(error);
        }
        Ok(self)
    }

    fn begin_read(&mut self, command: impl FnOnce(isize, Phase) -> Result<Command>) -> Result<()> {
        let timeout = self.deadline.saturating_duration_since(Instant::now());
        ensure!(!timeout.is_zero(), "pasteboard reader timed out");
        self.reader = Some(Reader::spawn(
            command(self.count, self.phase)?,
            timeout,
            OUTPUT_CAP,
            |bytes| Ok(serde_json::from_slice(&bytes)?),
        )?);
        Ok(())
    }

    pub(super) fn poll(
        mut self,
        count: isize,
        confirm: impl FnOnce(u128) -> bool,
        command: impl FnOnce(isize, Phase) -> Result<Command>,
    ) -> Result<Option<Self>> {
        if self.count != count || self.canceled.load(Ordering::SeqCst) {
            // Drop retires replaced/canceled observations, including held ones.
            return Ok(None);
        }
        let result = if Instant::now() >= self.deadline {
            Err(anyhow::anyhow!("pasteboard reader timed out"))
        } else if let Some(result) = self.reader.as_mut().and_then(Reader::poll) {
            result
        } else {
            return Ok(Some(self));
        };
        self.reader.take(); // reap before confirming, replacing or completing
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.finish(Completion::Unavailable);
                return Err(error);
            }
        };
        if self.phase == Phase::Payload {
            self.finish(Completion::Read(result));
            return Ok(None);
        }
        match marker_action(result.marker.as_deref().map(str::as_bytes), confirm) {
            MarkerAction::Confirmed => self.finish(Completion::Retired),
            MarkerAction::Invalidate => self.finish(Completion::Unavailable),
            MarkerAction::ReadPayload if self.phase == Phase::InitialMarker => {
                self.finish(Completion::Retired);
            }
            MarkerAction::ReadPayload => {
                self.phase = Phase::Payload;
                if let Err(error) = self.begin_read(command) {
                    self.finish(Completion::Unavailable);
                    return Err(error);
                }
                return Ok(Some(self));
            }
        }
        Ok(None)
    }
}
impl<C> Change<C> {
    fn finish(mut self, result: Completion<C>) {
        self.reader.take();
        if let Some(complete) = self.complete.take() {
            complete(self.token, result);
        }
    }
}
impl<C> Drop for Change<C> {
    fn drop(&mut self) {
        self.reader.take();
        if let Some(complete) = self.complete.take() {
            complete(self.token, Completion::Retired);
        }
    }
}

/// Exactly one child per watcher. Cancellation kills the provider call, rather
/// than abandoning an unkillable Cocoa thread. No payload work runs on watcher.
pub(super) struct Reader<T> {
    child: Child,
    #[cfg(windows)]
    job: ReaderJob,
    result: mpsc::Receiver<Result<T>>,
    canceled: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    deadline: Instant,
}
impl<T: Send + 'static> Reader<T> {
    pub(super) fn spawn(
        mut command: Command,
        timeout: Duration,
        cap: usize,
        decode: impl FnOnce(Vec<u8>) -> Result<T> + Send + 'static,
    ) -> Result<Self> {
        #[cfg(unix)]
        command.stdin(Stdio::null());
        #[cfg(windows)]
        command.stdin(Stdio::piped());
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        #[cfg(test)]
        eprintln!("clipboard reader PID={}", child.id());
        #[cfg(windows)]
        let job = match ReaderJob::assign(&child).and_then(|job| {
            use std::io::Write;
            child
                .stdin
                .take()
                .context("reader stdin")?
                .write_all(b"g")?;
            Ok(job)
        }) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let mut stdout = child.stdout.take().context("reader stdout")?;
        #[cfg(unix)]
        use std::os::fd::AsRawFd;
        // The decoding thread polls a nonblocking pipe so its cancellation does
        // not depend on EOF or a provider/descendant releasing the write end.
        #[cfg(unix)]
        let fd = stdout.as_raw_fd();
        #[cfg(unix)]
        if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::last_os_error().into());
        }
        let canceled = Arc::new(AtomicBool::new(false));
        let stop = canceled.clone();
        let (tx, result) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("pneu-pasteboard-result".into())
            .spawn(move || {
                let result = (|| {
                    let mut bytes = Vec::new();
                    let mut chunk = [0; 16384];
                    loop {
                        ensure!(!stop.load(Ordering::SeqCst), "pasteboard read canceled");
                        // Peek before a Windows anonymous-pipe read: ReadFile
                        // itself is synchronous and must never hold up cleanup.
                        #[cfg(windows)]
                        let available = {
                            use std::os::windows::io::AsRawHandle;
                            let mut available = 0;
                            let ok = unsafe {
                                windows_sys::Win32::System::Pipes::PeekNamedPipe(
                                    stdout.as_raw_handle(),
                                    std::ptr::null_mut(),
                                    0,
                                    std::ptr::null_mut(),
                                    &mut available,
                                    std::ptr::null_mut(),
                                )
                            };
                            if ok == 0 {
                                let error = std::io::Error::last_os_error();
                                if error.raw_os_error() == Some(109) {
                                    break;
                                }
                                return Err(error.into());
                            }
                            if available == 0 {
                                std::thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            available as usize
                        };
                        #[cfg(windows)]
                        let chunk = &mut chunk[..available.min(16384)];
                        #[cfg(unix)]
                        let chunk = &mut chunk[..];
                        match stdout.read(chunk) {
                            Ok(0) => break,
                            Ok(n) => {
                                ensure!(bytes.len() + n <= cap, "pasteboard reader output limit");
                                bytes.extend_from_slice(&chunk[..n]);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5))
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) => return Err(e.into()),
                        }
                    }
                    decode(bytes)
                })();
                let _ = tx.send(result);
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.into());
            }
        };
        Ok(Self {
            child,
            #[cfg(windows)]
            job,
            result,
            canceled,
            thread: Some(thread),
            deadline: Instant::now() + timeout,
        })
    }
    pub(super) fn poll(&mut self) -> Option<Result<T>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(anyhow::anyhow!("pasteboard reader exited")))
            }
            Err(mpsc::TryRecvError::Empty) if Instant::now() >= self.deadline => {
                Some(Err(anyhow::anyhow!("pasteboard reader timed out")))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}
impl<T> Drop for Reader<T> {
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::SeqCst);
        #[cfg(windows)]
        self.job.terminate();
        let _ = self.child.kill();
        let _ = self.child.wait(); // reaps only our SIGKILLed process, never a read
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The helper waits on stdin until assignment. Provider allocation and decoding
/// are bounded by committed process memory, including allocations inside Win32.
#[cfg(windows)]
struct ReaderJob(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl ReaderJob {
    const MEMORY_LIMIT: usize = 384 * 1024 * 1024;
    fn assign(child: &Child) -> Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        let job = Self(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) });
        ensure!(!job.0.is_null(), "create clipboard reader job");
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        limits.ProcessMemoryLimit = Self::MEMORY_LIMIT;
        ensure!(
            unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            } != 0,
            "limit clipboard reader job"
        );
        ensure!(
            unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) } != 0,
            "assign clipboard reader job: {}",
            std::io::Error::last_os_error()
        );
        Ok(job)
    }
    fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}
#[cfg(windows)]
impl Drop for ReaderJob {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[derive(Debug, PartialEq, Eq)]
    enum Outcome {
        Read(Option<String>),
        Unavailable,
        Retired,
    }
    #[test]
    fn output_cap_accepts_maximum_image_and_escaped_accompanying_text() -> Result<()> {
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let result = ReadResult {
            marker: None,
            content: Some(super::super::clipboard::Content::Formats(
                "\u{1}".repeat(crate::store::history::MAX_TEXT),
                vec![255; super::super::clipboard::MAX_IMAGE],
            )),
            fingerprints: vec![[255; 32]; 6],
        };
        let mut count = Count(0);
        serde_json::to_writer(&mut count, &result)?;
        assert!(count.0 > 64 * 1024 * 1024);
        assert!(
            count.0 <= OUTPUT_CAP,
            "valid clipboard JSON exceeds helper cap"
        );
        Ok(())
    }
    #[derive(Default)]
    struct Observations(Arc<Mutex<Vec<(u64, Outcome)>>>);
    impl Observations {
        fn change(&self, token: u64, phase: Phase) -> Change<String> {
            let completions = self.0.clone();
            Change::new(
                1,
                token,
                Arc::new(AtomicBool::new(false)),
                phase,
                Duration::from_secs(5),
                move |token, completion| {
                    let outcome = match completion {
                        Completion::Read(result) => Outcome::Read(result.content),
                        Completion::Unavailable => Outcome::Unavailable,
                        Completion::Retired => Outcome::Retired,
                    };
                    completions.lock().unwrap().push((token, outcome));
                },
            )
        }
        fn assert_only(&self, token: u64, expected: Outcome) {
            assert_eq!(*self.0.lock().unwrap(), [(token, expected)]);
        }
    }
    fn output(bytes: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf '%s' \"$1\"", "fixture", bytes]);
        command
    }
    fn result_command(_: isize, phase: Phase) -> Result<Command> {
        let result = ReadResult {
            marker: (phase != Phase::Payload).then(|| format!("{:032x}", 42)),
            content: (phase == Phase::Payload).then(|| "human copy".to_owned()),
            fingerprints: Vec::new(),
        };
        Ok(output(&serde_json::to_string(&result)?))
    }
    fn blocked_command(_: isize, _: Phase) -> Result<Command> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        Ok(command)
    }
    fn until_finished(
        change: Change<String>,
        confirmed: bool,
        command: impl FnMut(isize, Phase) -> Result<Command>,
    ) -> Result<()> {
        until_finished_by(change, |_| confirmed, command)
    }
    fn until_finished_by(
        mut change: Change<String>,
        mut confirm: impl FnMut(u128) -> bool,
        mut command: impl FnMut(isize, Phase) -> Result<Command>,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(Instant::now() < deadline, "fixture reader did not finish");
            match change.poll(1, &mut confirm, &mut command)? {
                Some(next) => change = next,
                None => return Ok(()),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn until_payload(mut change: Change<String>) -> Result<Change<String>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while change.phase == Phase::Marker {
            assert!(Instant::now() < deadline, "fixture marker did not finish");
            change = change.poll(1, |_| false, blocked_command)?.unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(change)
    }
    fn assert_reaped(pid: u32) {
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }

    #[test]
    fn startup_restart_and_access_recovery_confirm_without_reading_initial_payloads() -> Result<()>
    {
        for confirmed in [true, false] {
            for recovery in [false, true] {
                let mut baseline = Baseline::default();
                if recovery {
                    assert_eq!(baseline.snapshot(0, false), Some(true));
                    // Installation finished while access was denied. Seeing
                    // its count must not hide the ticket on the first allowed
                    // poll, even when the count does not change again.
                    assert_eq!(baseline.snapshot(1, true), None);
                }
                // Installation before watcher initialization (or replacement
                // watcher startup): the installed count is the first snapshot.
                assert!(baseline.changed(1));
                assert_eq!(baseline.snapshot(1, false), Some(true));
                let observations = Observations::default();
                let change = observations
                    .change(17, Phase::InitialMarker)
                    .start(result_command)?;
                let mut confirmations = 0;
                until_finished_by(
                    change,
                    |ticket| {
                        assert_eq!(ticket, 42);
                        confirmations += 1;
                        confirmed
                    },
                    |_, _| panic!("initial content must never become a local publication"),
                )?;
                assert_eq!(confirmations, 1);
                observations.assert_only(17, Outcome::Retired);
                assert_eq!(baseline.snapshot(1, false), None);
                assert!(!baseline.changed(1));
                // A later human copy is still an ordinary observation.
                assert_eq!(baseline.snapshot(2, false), Some(false));
            }
        }
        Ok(())
    }

    #[test]
    fn installation_after_initial_snapshot_is_a_changed_selection() {
        let mut baseline = Baseline::default();
        assert_eq!(baseline.snapshot(0, false), Some(true));
        assert!(baseline.changed(1));
        assert_eq!(baseline.snapshot(1, false), Some(false));
        assert_eq!(baseline.snapshot(1, false), None);
    }

    #[test]
    fn local_and_retired_marker_payloads_complete_the_original_observation_once() -> Result<()> {
        for phase in [Phase::Marker, Phase::Payload] {
            let observations = Observations::default();
            let change = observations.change(17, phase).start(result_command)?;
            // A retired marker must start a payload read with the same token,
            // without completing or recapturing the actor's recipient policy.
            let mut marker_seen = false;
            until_finished(change, false, |count, phase| {
                assert_eq!(count, 1);
                assert_eq!(phase, Phase::Payload);
                assert!(observations.0.lock().unwrap().is_empty());
                marker_seen = true;
                result_command(count, phase)
            })?;
            assert_eq!(marker_seen, phase == Phase::Marker);
            observations.assert_only(17, Outcome::Read(Some("human copy".into())));
        }
        Ok(())
    }

    #[test]
    fn confirmed_marker_retires_once_without_reading_payload_or_stamping_local_content()
    -> Result<()> {
        let observations = Observations::default();
        let change = observations
            .change(17, Phase::Marker)
            .start(result_command)?;
        until_finished(change, true, |_, _| {
            panic!("confirmed marker must not read payload")
        })?;
        observations.assert_only(17, Outcome::Retired);
        Ok(())
    }

    #[test]
    fn malformed_and_unreadable_markers_complete_once_as_unavailable() -> Result<()> {
        for marker in [
            None,
            Some("short"),
            Some("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"),
        ] {
            let observations = Observations::default();
            let change = observations
                .change(17, Phase::InitialMarker)
                .start(|_, _| {
                    Ok(output(&serde_json::to_string(&ReadResult::<String> {
                        marker: marker.map(str::to_owned),
                        content: None,
                        fingerprints: Vec::new(),
                    })?))
                })?;
            until_finished(change, false, |_, _| {
                panic!("malformed marker must not read payload")
            })?;
            observations.assert_only(17, Outcome::Unavailable);
        }
        Ok(())
    }

    #[test]
    fn replacement_cancellation_and_shutdown_retire_and_reap_each_phase_once() -> Result<()> {
        for phase in [Phase::InitialMarker, Phase::Marker, Phase::Payload] {
            for exit in ["replacement", "cancellation", "shutdown"] {
                let observations = Observations::default();
                let change = observations.change(17, phase).start(blocked_command)?;
                let pid = change.reader.as_ref().unwrap().child.id();
                let start = Instant::now();
                match exit {
                    "replacement" => assert!(
                        change
                            .poll(2, |_| panic!("replaced"), result_command)?
                            .is_none()
                    ),
                    "cancellation" => {
                        change.canceled.store(true, Ordering::SeqCst);
                        assert!(
                            change
                                .poll(1, |_| panic!("canceled"), result_command)?
                                .is_none()
                        );
                    }
                    "shutdown" => drop(change),
                    _ => unreachable!(),
                }
                assert!(start.elapsed() < Duration::from_secs(1));
                assert_reaped(pid);
                observations.assert_only(17, Outcome::Retired);
            }
        }
        Ok(())
    }

    #[test]
    fn replacement_drop_completes_before_the_new_marker_echo() -> Result<()> {
        let observations = Observations::default();
        let old = observations
            .change(17, Phase::Payload)
            .start(blocked_command)?;
        // Freeze the new observation before discarding the old reader, as the
        // watcher does. A manager echo cannot leave the old token held forever.
        let new = observations.change(18, Phase::Marker);
        drop(old);
        observations.assert_only(17, Outcome::Retired);
        until_finished(new.start(result_command)?, true, result_command)?;
        assert_eq!(
            *observations.0.lock().unwrap(),
            [(17, Outcome::Retired), (18, Outcome::Retired)]
        );
        Ok(())
    }

    #[test]
    fn blocked_provider_never_blocks_poll_and_deadline_completes_and_reaps_once() -> Result<()> {
        for phase in [Phase::InitialMarker, Phase::Marker, Phase::Payload] {
            let observations = Observations::default();
            let change = observations.change(17, phase).start(blocked_command)?;
            let pid = change.reader.as_ref().unwrap().child.id();
            let start = Instant::now();
            let mut change = change
                .poll(1, |_| panic!("blocked"), result_command)?
                .unwrap();
            assert!(start.elapsed() < Duration::from_millis(100));
            assert!(observations.0.lock().unwrap().is_empty());
            change.deadline = Instant::now();
            assert!(
                change
                    .poll(1, |_| panic!("expired"), result_command)
                    .is_err()
            );
            assert_reaped(pid);
            observations.assert_only(17, Outcome::Unavailable);
            until_finished(
                observations
                    .change(18, Phase::Payload)
                    .start(result_command)?,
                false,
                result_command,
            )?;
            assert_eq!(observations.0.lock().unwrap().len(), 2);
        }
        Ok(())
    }

    #[test]
    fn payload_after_retired_marker_keeps_the_original_deadline_and_cancellation() -> Result<()> {
        for cancel in [false, true] {
            let observations = Observations::default();
            let change = observations
                .change(17, Phase::Marker)
                .start(result_command)?;
            let original_deadline = change.deadline;
            let cancellation = change.canceled.clone();
            let mut change = until_payload(change)?;
            assert_eq!(change.token, 17);
            assert_eq!(change.deadline, original_deadline);
            assert!(observations.0.lock().unwrap().is_empty());
            let pid = change.reader.as_ref().unwrap().child.id();
            if cancel {
                cancellation.store(true, Ordering::SeqCst);
                assert!(
                    change
                        .poll(1, |_| panic!("payload phase"), result_command)?
                        .is_none()
                );
                observations.assert_only(17, Outcome::Retired);
            } else {
                change.deadline = Instant::now();
                assert!(
                    change
                        .poll(1, |_| panic!("payload phase"), result_command)
                        .is_err()
                );
                observations.assert_only(17, Outcome::Unavailable);
            }
            assert_reaped(pid);
        }
        Ok(())
    }

    #[test]
    fn command_spawn_decode_and_transition_errors_complete_once() -> Result<()> {
        for phase in [Phase::InitialMarker, Phase::Marker, Phase::Payload] {
            for failure in ["command", "spawn", "decode"] {
                let observations = Observations::default();
                let result = observations.change(17, phase).start(|_, _| match failure {
                    "command" => anyhow::bail!("fixture command error"),
                    "spawn" => Ok(Command::new("/pneu-test-no-such-reader")),
                    "decode" => Ok(output("malformed json")),
                    _ => unreachable!(),
                });
                match result {
                    Ok(change) => assert!(until_finished(change, false, result_command).is_err()),
                    Err(_) => assert_ne!(failure, "decode"),
                }
                observations.assert_only(17, Outcome::Unavailable);
            }
        }
        let observations = Observations::default();
        let change = observations
            .change(17, Phase::Marker)
            .start(result_command)?;
        assert!(
            until_finished(change, false, |_, _| anyhow::bail!("payload spawn failed")).is_err()
        );
        observations.assert_only(17, Outcome::Unavailable);
        Ok(())
    }

    #[test]
    fn unreadable_or_malformed_markers_never_confirm() {
        for bytes in [
            None,
            Some(&b""[..]),
            Some(&b"short"[..]),
            Some(&b"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"[..]),
            Some(&b"+000000000000000000000000000002a"[..]),
        ] {
            assert_eq!(
                marker_action(bytes, |_| panic!("invalid marker cannot confirm")),
                MarkerAction::Invalidate
            );
        }
        let valid = Some(&b"0000000000000000000000000000002a"[..]);
        assert_eq!(
            marker_action(valid, |ticket| ticket == 42),
            MarkerAction::Confirmed
        );
        assert_eq!(marker_action(valid, |_| false), MarkerAction::ReadPayload);
    }
    #[test]
    fn deny_allow_marker_confirmation_deny_invalidates_again() {
        let mut policy = Policy::default();
        assert!(policy.update(3));
        assert!(policy.log_failure());
        assert!(!policy.update(2));
        assert_eq!(
            marker_action(Some(b"0000000000000000000000000000002a"), |_| true),
            MarkerAction::Confirmed
        );
        assert!(policy.update(3));
        assert!(policy.log_failure());
        assert!(!policy.update(3));
    }
    #[test]
    fn snapshot_waits_for_writer_paused_after_clear() {
        let data = Arc::new(Mutex::new("old"));
        let (cleared_tx, cleared_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let writer_data = data.clone();
        let writer = std::thread::spawn(move || {
            let _guard = PUBLICATION.lock().unwrap();
            *writer_data.lock().unwrap() = "";
            cleared_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            *writer_data.lock().unwrap() = "ticket";
        });
        cleared_rx.recv().unwrap();
        assert!(PUBLICATION.try_lock().is_err());
        resume_tx.send(()).unwrap();
        writer.join().unwrap();
        let _guard = PUBLICATION.lock().unwrap();
        assert_eq!(*data.lock().unwrap(), "ticket");
    }
    #[test]
    fn shutdown_cancels_a_provider_before_its_deadline() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let reader = Reader::spawn(command, Duration::from_secs(30), 1024, Ok)?;
        let pid = reader.child.id();
        let start = Instant::now();
        drop(reader);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        Ok(())
    }
    #[test]
    fn blocked_provider_is_canceled_reaped_and_does_not_block_next_ticket() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let mut reader = Reader::spawn(command, Duration::from_millis(20), 1024, Ok)?;
        let pid = reader.child.id();
        assert!(reader.poll().is_none());
        std::thread::sleep(Duration::from_millis(30));
        assert!(reader.poll().unwrap().is_err());
        let start = Instant::now();
        drop(reader);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf ticket"]);
        let mut reader = Reader::spawn(command, Duration::from_secs(1), 1024, Ok)?;
        loop {
            if let Some(result) = reader.poll() {
                assert_eq!(result?, b"ticket");
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}
