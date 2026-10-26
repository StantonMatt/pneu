//! Win32 clipboard adapter. The watcher owns actor observations; provider reads
//! run in a killable child and publication is one OpenClipboard transaction.
#[path = "clipboard_content.rs"]
mod content;
use super::{
    pasteboard_reader::{Reader, ticket},
    windows_clipboard_content::*,
};
use anyhow::{Context, Result, ensure};
pub use content::{Content, MAX_IMAGE};
use sha2::{Digest, Sha256};
use std::{
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{
        DataExchange::*, LibraryLoader::GetModuleHandleW, Memory::*,
        RemoteDesktop::ProcessIdToSessionId, StationsAndDesktops::*, Threading::*,
    },
    UI::WindowsAndMessaging::*,
};

const TEXT: u32 = 13;
const DIB: u32 = 8;
const DIBV5: u32 = 17;
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn format(name: &str) -> Result<u32> {
    let id = unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) };
    ensure!(
        id != 0,
        "register clipboard format: {}",
        std::io::Error::last_os_error()
    );
    Ok(id)
}
fn interactive() -> Result<()> {
    let mut session = 0;
    ensure!(
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } != 0 && session != 0,
        "clipboard requires an interactive session"
    );
    let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    ensure!(!desktop.is_null(), "clipboard requires an input desktop");
    unsafe {
        CloseDesktop(desktop);
    }
    Ok(())
}
struct Window(HWND);
impl Window {
    fn new() -> Result<Self> {
        interactive()?;
        unsafe {
            let name = wide("PneuClipboard");
            let module = GetModuleHandleW(null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: module,
                lpszClassName: name.as_ptr(),
                ..std::mem::zeroed()
            };
            let atom = RegisterClassW(&class);
            ensure!(
                atom != 0 || GetLastError() == ERROR_CLASS_ALREADY_EXISTS,
                "register clipboard window"
            );
            let hwnd = CreateWindowExW(
                0,
                name.as_ptr(),
                name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                module,
                null(),
            );
            ensure!(
                !hwnd.is_null(),
                "create clipboard window: {}",
                std::io::Error::last_os_error()
            );
            Ok(Self(hwnd))
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}
struct Clipboard;
impl Clipboard {
    fn open(hwnd: HWND) -> Result<Self> {
        Self::open_until(hwnd, Instant::now() + Duration::from_millis(250), || false)
    }
    fn open_until(hwnd: HWND, deadline: Instant, canceled: impl Fn() -> bool) -> Result<Self> {
        loop {
            ensure!(!canceled(), "clipboard installation canceled");
            if unsafe { OpenClipboard(hwnd) } != 0 {
                return Ok(Self);
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::last_os_error()).context("open clipboard");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}
// Every allocation is prepared before EmptyClipboard; only a successful Set
// transfers ownership. A failed transaction clears everything it published.
struct Allocation(HGLOBAL);
impl Allocation {
    fn new(bytes: &[u8]) -> Result<Self> {
        write_step()?;
        let memory = Self(unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) });
        ensure!(!memory.0.is_null(), "allocate clipboard data");
        write_step()?;
        let pointer = unsafe { GlobalLock(memory.0) };
        ensure!(!pointer.is_null(), "lock clipboard data");
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
            GlobalUnlock(memory.0);
        }
        Ok(memory)
    }
    fn publish(&mut self, id: u32) -> Result<()> {
        write_step()?;
        ensure!(
            !unsafe { SetClipboardData(id, self.0) }.is_null(),
            "publish clipboard data: {}",
            std::io::Error::last_os_error()
        );
        self.0 = null_mut();
        Ok(())
    }
}
impl Drop for Allocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                GlobalFree(self.0);
            }
        }
    }
}
#[cfg(test)]
thread_local! {
    static WRITE_FAULT: std::cell::Cell<(Option<usize>, usize)> = const { std::cell::Cell::new((None, 0)) };
    static BEFORE_REAP: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
fn write_step() -> Result<()> {
    #[cfg(test)]
    WRITE_FAULT.with(|fault| {
        let (at, step) = fault.get();
        fault.set((at, step + 1));
        ensure!(
            at != Some(step),
            "injected clipboard write failure at {step}"
        );
        Ok(())
    })?;
    Ok(())
}
#[cfg(test)]
fn put(id: u32, bytes: &[u8]) -> Result<()> {
    Allocation::new(bytes)?.publish(id)
}
// This function is reachable only in the helper (or native test fixtures).
// Check authority after the provider returns, before even sizing/locking data.
fn get_checked(id: u32, cap: usize, guard: &ReadGuard) -> Result<Vec<u8>> {
    let memory = unsafe { GetClipboardData(id) };
    guard.check()?;
    copy_memory(memory, cap)
}
fn copy_memory(memory: HANDLE, cap: usize) -> Result<Vec<u8>> {
    unsafe {
        ensure!(!memory.is_null(), "clipboard format unavailable");
        let size = GlobalSize(memory);
        ensure!(size <= cap, "clipboard format limit");
        let pointer = GlobalLock(memory);
        ensure!(!pointer.is_null(), "lock clipboard format");
        let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
        GlobalUnlock(memory);
        Ok(bytes)
    }
}
#[cfg(test)]
fn get(id: u32, cap: usize) -> Result<Vec<u8>> {
    copy_memory(unsafe { GetClipboardData(id) }, cap)
}
struct Metadata {
    formats: Vec<u32>,
    class: Classification,
    history: Option<u32>,
}
fn metadata() -> Result<Metadata> {
    let mut formats = Vec::new();
    let mut names = Vec::new();
    let mut id = 0;
    loop {
        unsafe {
            SetLastError(0);
        }
        id = unsafe { EnumClipboardFormats(id) };
        if id == 0 {
            ensure!(
                unsafe { GetLastError() } == 0,
                "enumerate clipboard formats"
            );
            break;
        }
        ensure!(formats.len() < 4096, "clipboard format count limit");
        formats.push(id);
        let mut name = [0u16; 256];
        let len = unsafe { GetClipboardFormatNameW(id, name.as_mut_ptr(), 256) };
        if id >= 0xc000 {
            ensure!(len > 0, "clipboard format name unavailable");
            names.push((String::from_utf16_lossy(&name[..len as usize]), id));
        }
    }
    // History's presence is metadata; its value is read only in the helper.
    let history = names
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(HISTORY))
        .map(|(_, id)| *id);
    let values: Vec<_> = names
        .into_iter()
        .map(|(n, _)| {
            let value = n.eq_ignore_ascii_case(HISTORY).then_some(1);
            (n, value)
        })
        .collect();
    Ok(Metadata {
        formats,
        class: classify(&values),
        history,
    })
}

#[derive(Debug)]
struct ReadAbort(SnapshotStatus);
impl std::fmt::Display for ReadAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "clipboard {:?}", self.0)
    }
}
impl std::error::Error for ReadAbort {}
struct ReadGuard {
    sequence: u32,
    history: Option<u32>,
}
impl ReadGuard {
    fn check(&self) -> Result<()> {
        if unsafe { GetClipboardSequenceNumber() } != self.sequence {
            return Err(ReadAbort(SnapshotStatus::Changed).into());
        }
        let meta = metadata()?;
        if meta.class == Classification::Sensitive || meta.history != self.history {
            return Err(ReadAbort(SnapshotStatus::Sensitive).into());
        }
        Ok(())
    }
    fn optional(&self, id: u32, cap: usize) -> Result<Option<Vec<u8>>> {
        match get_checked(id, cap, self) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.downcast_ref::<ReadAbort>().is_some() => Err(e),
            Err(_) => {
                self.check()?;
                Ok(None)
            }
        }
    }
}

/// All preparation is inert. Hash only the representations a watcher publishes.
pub(crate) struct Offer {
    payload: Arc<Vec<(u32, Vec<u8>)>>,
    hashes: Vec<[u8; 32]>,
    ticket: u128,
    canceled: Arc<AtomicBool>,
    authority: Arc<dyn Fn() -> bool + Send + Sync>,
    worker: Option<tokio::task::JoinHandle<()>>,
    cleanup: Option<Arc<crate::work::Work>>,
    #[cfg(test)]
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl Offer {
    pub(crate) fn prepare(input: &crate::actions::ClipboardInput, ticket: u128) -> Result<Self> {
        use crate::actions::ClipboardInput;
        let mut offer = Self {
            payload: Arc::new(Vec::new()),
            hashes: Vec::new(),
            ticket,
            canceled: Arc::new(AtomicBool::new(false)),
            authority: Arc::new(|| true),
            worker: None,
            cleanup: None,
            #[cfg(test)]
            thread: std::sync::Mutex::new(None),
        };
        match input {
            ClipboardInput::Text(t) => offer.text(t)?,
            ClipboardInput::Formats { text, png, .. } => {
                offer.text(text)?;
                offer.image(png)?;
            }
            ClipboardInput::Image(file, _) => offer.image(&file.read_bounded(16 * 1024 * 1024)?)?,
            ClipboardInput::Bytes { bytes, .. } => offer.image(bytes)?,
        }
        Ok(offer)
    }
    fn text(&mut self, text: &str) -> Result<()> {
        ensure!(valid_text(text), "invalid clipboard text");
        let bytes = wide(text).iter().flat_map(|c| c.to_le_bytes()).collect();
        Arc::get_mut(&mut self.payload).unwrap().push((TEXT, bytes));
        self.hashes.push(Sha256::digest(text.as_bytes()).into());
        Ok(())
    }
    fn image(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(bytes.len() <= 16 * 1024 * 1024, "clipboard image limit");
        let decoded = crate::actions::decode_image(bytes)?;
        let png = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            ensure!(bytes.len() <= MAX_IMAGE, "clipboard PNG limit");
            bytes.to_vec()
        } else {
            png(&decoded)?
        };
        self.hashes.push(Sha256::digest(&png).into());
        Arc::get_mut(&mut self.payload)
            .unwrap()
            .push((format("PNG")?, png));
        let dib = image_to_dib(&decoded)?;
        self.hashes.push(Sha256::digest(dib_to_png(&dib)?).into());
        Arc::get_mut(&mut self.payload).unwrap().push((DIBV5, dib));
        Ok(())
    }
    pub(crate) fn hashes(&self) -> Vec<[u8; 32]> {
        self.hashes.clone()
    }
    pub(crate) fn set_authority(&mut self, authority: impl Fn() -> bool + Send + Sync + 'static) {
        self.authority = Arc::new(authority);
    }
    pub(crate) async fn install_tracked(
        &mut self,
        writers: Arc<crate::work::Work>,
        cleanup: Arc<crate::work::Work>,
    ) -> Result<()> {
        let payload = self.payload.clone();
        let authority = self.authority.clone();
        let ticket = self.ticket;
        self.install_worker(writers, cleanup, move |canceled, tx| {
            writer(payload, ticket, canceled, authority, |result| {
                let _ = tx.send(result);
            })
        })
        .await
    }
    // The worker/reply boundary also allows lifecycle tests without Win32 or
    // an interactive desktop. Success replies before the owner window exits.
    pub(crate) async fn install_worker(
        &mut self,
        writers: Arc<crate::work::Work>,
        cleanup: Arc<crate::work::Work>,
        run: impl FnOnce(Arc<AtomicBool>, tokio::sync::oneshot::Sender<Result<()>>) + Send + 'static,
    ) -> Result<()> {
        ensure!(self.worker.is_none(), "clipboard offer already installed");
        // The driver's select drops this future on supersession/revocation.
        let mut guard = InstallCancellation(Some(self.canceled.clone()));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let reservation = writers.reserve().await;
        self.cleanup = Some(cleanup);
        let canceled = self.canceled.clone();
        self.worker = Some(reservation.submit(move || run(canceled, tx)));
        let result = rx.await.context("clipboard writer exited")?;
        if result.is_ok() {
            guard.0.take();
        }
        result
    }
    pub(crate) async fn stop(&mut self) {
        self.canceled.store(true, Ordering::SeqCst);
        // Await by reference so cancellation/unwinding still leaves Drop able
        // to schedule reaping. Only this offer's worker belongs to this stop.
        if let Some(worker) = &mut self.worker {
            let _ = worker.await;
        }
        self.worker.take();
    }
    #[cfg(test)]
    fn install(&self) -> Result<()> {
        let payload = self.payload.clone();
        let canceled = self.canceled.clone();
        let authority = self.authority.clone();
        let ticket = self.ticket;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        *self.thread.lock().unwrap() = Some(std::thread::spawn(move || {
            writer(payload, ticket, canceled, authority, |result| {
                let _ = tx.send(result);
            });
        }));
        rx.recv()?
    }
}
impl Drop for Offer {
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            // Panic/canceled preparation paths can drop an offer before stop.
            // Register cleanup synchronously so the driver's fallback drain
            // waits for this retired worker, never an installed predecessor.
            let owned = crate::work::track(vec![self.cleanup.take().unwrap()]);
            tokio::spawn(async move {
                let _owned = owned;
                let _ = worker.await;
            });
        }
        #[cfg(test)]
        if let Some(thread) = self.thread.get_mut().unwrap().take() {
            let _ = thread.join();
        }
    }
}
struct InstallCancellation(Option<Arc<AtomicBool>>);
impl Drop for InstallCancellation {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(true, Ordering::SeqCst);
        }
    }
}
static WRITER: AtomicIsize = AtomicIsize::new(0);
struct WriterWindow(Window);
impl Drop for WriterWindow {
    fn drop(&mut self) {
        let _ = WRITER.compare_exchange(self.0.0 as isize, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}
fn writer(
    payload: Arc<Vec<(u32, Vec<u8>)>>,
    ticket: u128,
    canceled: Arc<AtomicBool>,
    authority: Arc<dyn Fn() -> bool + Send + Sync>,
    reply: impl FnOnce(Result<()>),
) {
    let result = Window::new().map(WriterWindow).and_then(|window| {
        install_payload(window.0.0, &payload, ticket, &canceled, &*authority)?;
        Ok(window)
    });
    match result {
        Ok(window) => {
            reply(Ok(()));
            // Keep EmptyClipboard's owner alive through marker confirmation.
            while !canceled.load(Ordering::SeqCst) {
                let mut message = unsafe { std::mem::zeroed() };
                while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } != 0 {
                    unsafe {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            drop(window);
        }
        Err(error) => reply(Err(error)),
    }
}
fn install_payload(
    hwnd: HWND,
    payload: &[(u32, Vec<u8>)],
    ticket: u128,
    canceled: &AtomicBool,
    authority: &dyn Fn() -> bool,
) -> Result<()> {
    let install = format(INSTALL)?;
    let cloud = format(CLOUD)?;
    let mut allocations = payload
        .iter()
        .map(|(id, b)| Ok((*id, Allocation::new(b)?)))
        .collect::<Result<Vec<_>>>()?;
    allocations.push((cloud, Allocation::new(&0u32.to_le_bytes())?));
    allocations.push((
        install,
        Allocation::new(format!("{ticket:032x}").as_bytes())?,
    ));
    let _clipboard = Clipboard::open_until(hwnd, Instant::now() + Duration::from_secs(5), || {
        canceled.load(Ordering::SeqCst) || !authority()
    })?;
    write_step()?;
    // Last authority check after acquisition/allocation, immediately before the
    // first destructive effect. Cancellation never leaves a marker behind.
    ensure!(
        !canceled.load(Ordering::SeqCst) && authority(),
        "clipboard installation canceled"
    );
    ensure!(unsafe { EmptyClipboard() } != 0, "empty clipboard");
    WRITER.store(hwnd as isize, Ordering::SeqCst);
    for (id, memory) in &mut allocations {
        if let Err(error) = memory.publish(*id) {
            ensure!(
                unsafe { EmptyClipboard() } != 0,
                "clear failed clipboard transaction: {error}"
            );
            return Err(error);
        }
    }
    Ok(())
}
fn writer_owns_clipboard() -> bool {
    let writer = WRITER.load(Ordering::SeqCst);
    writer != 0 && unsafe { GetClipboardOwner() } as isize == writer
}

pub fn read_helper(sequence: u32, marker: bool, images: bool) -> Result<()> {
    use std::io::Read;
    let mut go = [0];
    std::io::stdin().read_exact(&mut go)?;
    ensure!(go == *b"g", "clipboard helper not authorized");
    let result = read_snapshot(sequence, marker, images)?;
    serde_json::to_writer(std::io::stdout().lock(), &result)?;
    Ok(())
}
fn read_snapshot(sequence: u32, marker: bool, images: bool) -> Result<Snapshot> {
    // Parent enforces the original observation deadline across helper phases.
    // Acquisition contention is retried for all of that remaining lifetime.
    let _clipboard =
        Clipboard::open_until(null_mut(), Instant::now() + Duration::from_secs(5), || {
            false
        })?;
    let result: Result<Snapshot> = (|| {
        let meta = metadata()?;
        let guard = ReadGuard {
            sequence,
            history: meta.history,
        };
        guard.check()?;
        if let Some(id) = meta.history {
            let value = guard.optional(id, 4)?;
            if value.as_deref() != Some(&1u32.to_le_bytes()) {
                return Err(ReadAbort(SnapshotStatus::Sensitive).into());
            }
        }
        let mut result = Snapshot::empty();
        if marker {
            result.marker = guard
                .optional(format(INSTALL)?, 32)?
                .and_then(|b| ticket(&b))
                .map(|t| format!("{t:032x}"));
        } else {
            let text = if meta.formats.contains(&TEXT) {
                guard
                    .optional(TEXT, 2 * (pneu_proto::MAX_CLIPBOARD_BYTES + 1))?
                    .and_then(|bytes| {
                        if bytes.len() % 2 != 0 {
                            return None;
                        }
                        let chars: Vec<_> = bytes
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]]))
                            .collect();
                        let end = chars.iter().position(|c| *c == 0)?;
                        String::from_utf16(&chars[..end])
                            .ok()
                            .filter(|s| valid_text(s))
                    })
            } else {
                None
            };
            let mut image = None;
            if images {
                let png_format = format("PNG")?;
                if meta.formats.contains(&png_format) {
                    image = guard.optional(png_format, MAX_IMAGE)?.filter(|b| {
                        b.starts_with(b"\x89PNG\r\n\x1a\n")
                            && crate::actions::decode_image(b).is_ok()
                    });
                }
                if image.is_none() {
                    for id in [DIBV5, DIB] {
                        if meta.formats.contains(&id) {
                            image = guard
                                .optional(id, MAX_DIB)?
                                .and_then(|b| dib_to_png(&b).ok());
                            if image.is_some() {
                                break;
                            }
                        }
                    }
                }
            }
            result.content = combine(text, image);
        }
        guard.check()?;
        Ok(result)
    })();
    match result {
        Err(error) if error.downcast_ref::<ReadAbort>().is_some() => Ok(Snapshot {
            status: error.downcast_ref::<ReadAbort>().unwrap().0,
            ..Snapshot::empty()
        }),
        other => other,
    }
}

struct Pending {
    reader: Option<Reader<Snapshot>>,
    core: std::sync::Weak<crate::Core>,
    token: u64,
    sequence: u32,
    canceled: Arc<AtomicBool>,
    marker: bool,
    initial: bool,
    deadline: Instant,
    completed: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(hook) = BEFORE_REAP.with(|hook| hook.borrow_mut().take()) {
            hook();
        }
        self.reader.take();
        if !self.completed
            && let Some(core) = self.core.upgrade()
        {
            core.retire_clipboard_observation(self.token);
        }
    }
}
fn command(sequence: u32, marker: bool, images: bool) -> Result<std::process::Command> {
    #[cfg(not(test))]
    let executable = std::env::current_exe()?;
    #[cfg(test)]
    let executable = std::env::var_os("PNEU_CLIPBOARD_TEST_EXE")
        .context("set PNEU_CLIPBOARD_TEST_EXE to the built pneud.exe")?;
    let mut command = std::process::Command::new(executable);
    command.args([
        "clipboard-read",
        &sequence.to_string(),
        if marker { "marker" } else { "payload" },
        &images.to_string(),
    ]);
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    Ok(command)
}
fn invalidate(core: &crate::Core) {
    let (_, token) = core.observe_watched_clipboard_kind(false, false, false);
    core.clipboard_observed(token, None, false);
}
impl Pending {
    fn start(&mut self, images: bool) -> Result<()> {
        ensure!(
            !self.canceled.load(Ordering::SeqCst),
            "clipboard read canceled"
        );
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "clipboard reader timed out");
        self.reader = Some(Reader::spawn(
            command(self.sequence, self.marker, images)?,
            remaining,
            OUTPUT_CAP,
            parse_output,
        )?);
        Ok(())
    }
    fn finish(mut self, core: &crate::Core, content: Option<Content>, debounce: bool) {
        self.reader.take();
        self.completed = true;
        core.clipboard_observed(self.token, content, debounce);
    }
}
fn unavailable(core: &crate::Core, pending: &mut Option<Pending>) {
    // Queue the new authority boundary before old observation retirement.
    invalidate(core);
    pending.take();
}
fn poll(
    core: &Arc<crate::Core>,
    baseline: &mut Option<u32>,
    pending: &mut Option<Pending>,
) -> Result<()> {
    let sequence = unsafe { GetClipboardSequenceNumber() };
    if *baseline != Some(sequence) {
        let snapshot = (|| {
            let _clipboard = Clipboard::open(null_mut())?;
            if unsafe { GetClipboardSequenceNumber() } != sequence {
                return Ok(None);
            }
            Ok(Some((metadata()?, writer_owns_clipboard())))
        })();
        let (meta, owned) = match snapshot {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return service_pending(core, sequence, pending),
            Err(error) => {
                unavailable(core, pending);
                return Err(error);
            }
        };
        let initial = baseline.replace(sequence).is_none();
        if meta.class == Classification::Sensitive {
            unavailable(core, pending);
            return Ok(());
        }
        let marker = meta.class == Classification::Origin && owned;
        if initial && !marker {
            pending.take();
            return Ok(());
        }
        // Freeze recipients and register cancellation before reaping the old
        // provider, and reap it before spawning the replacement provider.
        let (_, token) =
            core.observe_watched_clipboard_kind(false, meta.formats.is_empty(), initial);
        let canceled = Arc::new(AtomicBool::new(false));
        core.register_clipboard_read(token, canceled.clone());
        let mut next = Pending {
            reader: None,
            core: Arc::downgrade(core),
            token,
            sequence,
            canceled,
            marker,
            initial,
            deadline: Instant::now() + Duration::from_secs(5),
            completed: false,
        };
        pending.take();
        if let Err(error) = next.start(core.config().clipboard_include_images) {
            next.finish(core, None, false);
            return Err(error);
        }
        *pending = Some(next);
        return Ok(());
    }
    service_pending(core, sequence, pending)
}
fn service_pending(
    core: &Arc<crate::Core>,
    sequence: u32,
    pending: &mut Option<Pending>,
) -> Result<()> {
    let Some(read) = pending.as_mut() else {
        return Ok(());
    };
    if read.sequence != sequence || read.canceled.load(Ordering::SeqCst) {
        pending.take();
        return Ok(());
    }
    let result = if Instant::now() >= read.deadline {
        Some(Err(anyhow::anyhow!("clipboard reader timed out")))
    } else {
        read.reader.as_mut().and_then(Reader::poll)
    };
    let Some(result) = result else { return Ok(()) };
    let mut read = pending.take().unwrap();
    read.reader.take(); // reap before handshake, next phase, or actor completion
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            read.finish(core, None, false);
            return Err(error);
        }
    };
    if unsafe { GetClipboardSequenceNumber() } != sequence || read.canceled.load(Ordering::SeqCst) {
        return Ok(()); // Drop retires the obsolete observation.
    }
    if result.status != SnapshotStatus::Ready {
        read.finish(core, None, false);
    } else if read.marker {
        let owned = writer_owns_clipboard();
        let ticket = result.marker.as_deref().and_then(|s| ticket(s.as_bytes()));
        if owned && ticket.is_some_and(|ticket| core.clipboard_install_marker(ticket)) {
            return Ok(()); // Drop retires the confirmed observation.
        }
        if owned && ticket.is_none() {
            read.finish(core, None, false);
        } else if read.initial {
            // Unknown startup contents remain a baseline, even with a marker.
            return Ok(());
        } else {
            // An unknown/retired or foreign marker does not hide local content.
            // Keep the original token, frozen policy, cancellation and deadline.
            read.marker = false;
            if let Err(error) = read.start(core.config().clipboard_include_images) {
                read.finish(core, None, false);
                return Err(error);
            }
            *pending = Some(read);
        }
    } else {
        if result.content.is_some() {
            eprintln!("pneud: clipboard observed sequence={sequence}");
        }
        read.finish(core, result.content, true);
    }
    Ok(())
}

// Also runs on unwinding, so every unexpected exit revokes cached manual
// shares before a runtime restart establishes its metadata-only baseline.
struct WatchAuthority {
    core: std::sync::Weak<crate::Core>,
    stopping: Arc<AtomicBool>,
}
impl Drop for WatchAuthority {
    fn drop(&mut self) {
        if !self.stopping.swap(true, Ordering::SeqCst)
            && let Some(core) = self.core.upgrade()
        {
            let (_, token) = core.observe_watched_clipboard_kind(false, true, true);
            core.clipboard_observed(token, None, false);
        }
    }
}

pub struct Watch {
    id: u32,
    stopping: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watch {
    pub fn start(core: Arc<crate::Core>) -> Result<Self> {
        interactive()?;
        let (ready, rx) = std::sync::mpsc::sync_channel(1);
        let weak = Arc::downgrade(&core);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = std::thread::Builder::new()
            .name("pneu-clipboard".into())
            .spawn(move || {
                let window = match Window::new() {
                    Ok(w) => w,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if unsafe { AddClipboardFormatListener(window.0) } == 0 {
                    let _ = ready.send(Err(std::io::Error::last_os_error().into()));
                    return;
                }
                if unsafe { SetTimer(window.0, 1, 25, None) } == 0 {
                    unsafe {
                        RemoveClipboardFormatListener(window.0);
                    }
                    let _ = ready.send(Err(anyhow::anyhow!("clipboard timer")));
                    return;
                }
                let authority = WatchAuthority {
                    core: weak.clone(),
                    stopping: stop,
                };
                let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
                let mut baseline = None;
                let mut pending = None;
                let mut logged = false;
                loop {
                    let Some(core) = weak.upgrade() else { break };
                    match poll(&core, &mut baseline, &mut pending) {
                        Ok(()) => logged = false,
                        Err(error) => {
                            if !logged {
                                eprintln!("pneud: clipboard read unavailable: {error}");
                                logged = true;
                            }
                        }
                    }
                    drop(core);
                    let mut message = unsafe { std::mem::zeroed() };
                    if unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } <= 0 {
                        break;
                    }
                    unsafe {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                drop(authority);
                drop(pending);
                unsafe {
                    KillTimer(window.0, 1);
                    RemoveClipboardFormatListener(window.0);
                }
            })?;
        match rx.recv() {
            Ok(Ok(id)) => Ok(Self {
                id,
                stopping,
                thread: Some(thread),
            }),
            result => {
                let _ = thread.join();
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => anyhow::bail!("clipboard thread exited during startup"),
                }
            }
        }
    }
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
    pub async fn shutdown(mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        if !self.is_finished() {
            unsafe {
                PostThreadMessageW(self.id, WM_QUIT, 0, 0);
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        if !self.is_finished() {
            unsafe {
                PostThreadMessageW(self.id, WM_QUIT, 0, 0);
            }
        }
    }
}

#[cfg(test)]
#[path = "windows_clipboard_tests.rs"]
mod tests;
