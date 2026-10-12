//! Event-driven data-control watcher, with bounded reads and a cancellable owner
//! thread. Selection data is read from the actual offer (never a later selection).
use crate::Core;
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    io::Read,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use wl_clipboard_rs::{
    paste::Seat,
    watch::{ClipboardEvent, ClipboardType, Watcher},
};

pub const ORIGIN_MIME: &str = "application/x-pneu-origin";
pub const MAX_IMAGE: usize = 10 * 1024 * 1024;
#[derive(Default)]
pub struct EchoGuard(Mutex<VecDeque<[u8; 32]>>);
impl EchoGuard {
    pub fn remember(&self, bytes: &[u8]) {
        let mut hashes = self.0.lock().expect("clipboard echo");
        hashes.clear(); // Only the most recent selection Pneu placed is an echo.
        hashes.push_back(Sha256::digest(bytes).into());
        while hashes.len() > 8 {
            hashes.pop_front();
        }
    }
    pub fn replace_hashes(&self, hashes: impl IntoIterator<Item = [u8; 32]>) {
        let mut current = self.0.lock().expect("clipboard echo");
        *current = hashes.into_iter().take(8).collect();
    }
    pub fn contains(&self, bytes: &[u8]) -> bool {
        self.0
            .lock()
            .expect("clipboard echo")
            .contains(&Sha256::digest(bytes).into())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Image(Vec<u8>),
}

#[derive(Default)]
pub struct Debounce {
    pending: Option<(Content, Instant)>,
}
impl Debounce {
    pub fn change(&mut self, content: Option<Content>, now: Instant) {
        self.pending = content.map(|c| (c, now + Duration::from_millis(300)));
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|(_, due)| *due)
    }
    pub fn ready(&mut self, now: Instant) -> Option<Content> {
        if self.deadline().is_some_and(|due| due <= now) {
            self.pending.take().map(|(c, _)| c)
        } else {
            None
        }
    }
}
pub fn sensitive_mime(mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    mime != "x-kde-passwordmanagerhint"
        && (mime.contains("password")
            || mime.contains("secret")
            || mime.contains("keepass")
            || mime.contains("concealed")
            || mime.contains("sensitive"))
}
pub fn filter(
    mime: &str,
    bytes: Vec<u8>,
    secret: bool,
    include_images: bool,
    echo: &EchoGuard,
) -> Option<Content> {
    if secret || echo.contains(&bytes) {
        return None;
    }
    if mime == "image/png" {
        if !include_images || bytes.len() > MAX_IMAGE || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return None;
        }
        Some(Content::Image(bytes))
    } else {
        if bytes.len() > pneu_proto::MAX_CLIPBOARD_BYTES {
            return None;
        }
        let text = String::from_utf8(bytes).ok()?;
        if text.contains('\0') {
            None
        } else {
            Some(Content::Text(text))
        }
    }
}
#[derive(Clone)]
struct Selection {
    target: Option<crate::core::ClipboardTarget>,
    content: Option<Content>,
    arrived: Instant,
}
impl Selection {
    fn apply(&self, debounce: &mut Debounce) -> Option<crate::core::ClipboardTarget> {
        debounce.change(self.content.clone(), self.arrived);
        self.target.clone()
    }
}
// One reader and one latest-only slot bound memory and descriptors even when
// sources never finish. Observation never waits for data, and every new event
// cancels the current read before capturing the next offer's policy.
struct ReadJob {
    selection: Selection,
    generation: u64,
    include_images: bool,
    mime: &'static str,
    reader: std::fs::File,
    marker: Option<std::fs::File>,
}
#[derive(Default)]
struct ReadQueue {
    pending: Mutex<Option<ReadJob>>,
    ready: Condvar,
    generation: AtomicU64,
    #[cfg(test)]
    started: tokio::sync::Notify,
}
impl ReadQueue {
    fn observe(&self, selection: Selection, tx: &tokio::sync::watch::Sender<Selection>) -> u64 {
        let mut pending = self.pending.lock().expect("clipboard reads");
        pending.take();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        tx.send_replace(selection);
        generation
    }
    fn submit(&self, job: ReadJob, stop: &AtomicBool) {
        let mut pending = self.pending.lock().expect("clipboard reads");
        if !stop.load(Ordering::SeqCst) && self.generation.load(Ordering::SeqCst) == job.generation
        {
            *pending = Some(job);
            self.ready.notify_one();
        }
    }
    fn take(&self, stop: &AtomicBool) -> Option<ReadJob> {
        let mut pending = self.pending.lock().expect("clipboard reads");
        while pending.is_none() && !stop.load(Ordering::SeqCst) {
            pending = self.ready.wait(pending).expect("clipboard reads");
        }
        if stop.load(Ordering::SeqCst) {
            None
        } else {
            pending.take()
        }
    }
    fn shutdown(&self) {
        self.pending.lock().expect("clipboard reads").take();
        self.ready.notify_all();
    }
}
fn observe_offer(
    core: &Core,
    reads: &ReadQueue,
    tx: &tokio::sync::watch::Sender<Selection>,
    arrived: Instant,
) -> (Selection, u64, bool) {
    let selection = Selection {
        target: core.observe_clipboard_offer(),
        content: None,
        arrived,
    };
    let include_images = core.config().clipboard_include_images && core.clipboard_image_capable();
    let generation = reads.observe(selection.clone(), tx);
    (selection, generation, include_images)
}
fn prepare_job(
    event: ClipboardEvent<'_>,
    selection: Selection,
    generation: u64,
    include_images: bool,
) -> Result<Option<ReadJob>> {
    let ClipboardEvent::Changed {
        mime_types,
        mut offer,
        ..
    } = event
    else {
        return Ok(None);
    };
    if selection.target.is_none()
        || mime_types
            .iter()
            .any(|m| m == ORIGIN_MIME || sensitive_mime(m))
    {
        return Ok(None);
    }
    let mime = if include_images && mime_types.iter().any(|m| m == "image/png") {
        Some("image/png")
    } else {
        ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"]
            .into_iter()
            .find(|m| mime_types.iter().any(|offered| offered == m))
    };
    let Some(mime) = mime else {
        return Ok(None);
    };
    // Issue both receive requests promptly, then drop the borrowed offer so the
    // watcher can dispatch later events. Only the reader owns these bounded pipes.
    let marker = mime_types
        .iter()
        .find(|m| m.eq_ignore_ascii_case("x-kde-passwordManagerHint"))
        .map(|m| request_pipe(&mut offer, m))
        .transpose()?;
    let reader = request_pipe(&mut offer, mime)?;
    Ok(Some(ReadJob {
        selection,
        generation,
        include_images,
        mime,
        reader,
        marker,
    }))
}
fn request_pipe(
    offer: &mut wl_clipboard_rs::watch::Offer<'_>,
    mime: &str,
) -> Result<std::fs::File> {
    Ok(std::fs::File::from(rustix::io::fcntl_dupfd_cloexec(
        &offer.receive(mime)?,
        0,
    )?))
}
fn read_jobs(
    reads: &ReadQueue,
    stop: &AtomicBool,
    echo: &EchoGuard,
    tx: &tokio::sync::watch::Sender<Selection>,
) {
    while let Some(mut job) = reads.take(stop) {
        let result: Result<Option<Content>> = (|| {
            let deadline = job.selection.arrived + Duration::from_secs(5);
            if let Some(marker) = job.marker.as_mut() {
                let bytes = read_pipe(marker, 1024, stop, reads, job.generation, deadline)?;
                if String::from_utf8_lossy(&bytes)
                    .trim_matches(|c: char| c.is_whitespace() || c == '\0')
                    .eq_ignore_ascii_case("secret")
                {
                    return Ok(None);
                }
            }
            let limit = if job.mime == "image/png" {
                MAX_IMAGE
            } else {
                pneu_proto::MAX_CLIPBOARD_BYTES
            };
            let bytes = read_pipe(
                &mut job.reader,
                limit,
                stop,
                reads,
                job.generation,
                deadline,
            )?;
            Ok(filter(job.mime, bytes, false, job.include_images, echo))
        })();
        job.selection.content = result.unwrap_or(None);
        // Serialize publication with observation: an old read cannot overwrite
        // the cancellation/new selection even if it completes at the same instant.
        let _pending = reads.pending.lock().expect("clipboard reads");
        if !stop.load(Ordering::SeqCst) && reads.generation.load(Ordering::SeqCst) == job.generation
        {
            tx.send_replace(job.selection);
        }
    }
}
pub struct Watch {
    cancel: wl_clipboard_rs::watch::CancelHandle,
    stopping: Arc<AtomicBool>,
    reads: Arc<ReadQueue>,
    thread: Option<std::thread::JoinHandle<()>>,
    reader: Option<std::thread::JoinHandle<()>>,
    sender: tokio::task::JoinHandle<()>,
}
impl Watch {
    pub fn start(core: Arc<Core>, echo: Arc<EchoGuard>) -> Result<Self> {
        let watcher = Watcher::new(ClipboardType::Regular, Seat::Unspecified)?;
        let cancel = watcher.cancel_handle();
        let stopping = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(ReadQueue::default());
        let (tx, mut rx) = tokio::sync::watch::channel(Selection {
            target: None,
            content: None,
            arrived: Instant::now(),
        });
        let reader = {
            let (reads, stop, tx) = (reads.clone(), stopping.clone(), tx.clone());
            std::thread::Builder::new()
                .name("pneu-clipboard-reader".into())
                .spawn(move || read_jobs(&reads, &stop, &echo, &tx))?
        };
        let stop = stopping.clone();
        let observed_reads = reads.clone();
        let weak = Arc::downgrade(&core);
        let thread = std::thread::Builder::new()
            .name("pneu-clipboard-watch".into())
            .spawn(move || {
                let mut watcher = watcher;
                let mut first = true;
                while !stop.load(Ordering::SeqCst) {
                    let event = match watcher.next_event() {
                        Ok(Some(event)) => event,
                        _ => break,
                    };
                    if first {
                        first = false;
                        continue;
                    }
                    let arrived = Instant::now();
                    let Some(core) = weak.upgrade() else {
                        break;
                    };
                    let (selection, generation, include_images) =
                        observe_offer(&core, &observed_reads, &tx, arrived);
                    if let Ok(Some(job)) = prepare_job(event, selection, generation, include_images)
                    {
                        observed_reads.submit(job, &stop);
                    }
                }
                stop.store(true, Ordering::SeqCst);
                observed_reads.shutdown();
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                // A second-thread failure must not leave the reader parked.
                stopping.store(true, Ordering::SeqCst);
                cancel.cancel();
                reads.shutdown();
                let _ = reader.join();
                return Err(error.into());
            }
        };
        let sender = tokio::spawn(Box::pin(async move {
            let mut debounce = Debounce::default();
            let mut target = None;
            loop {
                let due = debounce
                    .deadline()
                    .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));
                tokio::select! {
                    change=rx.changed()=>{
                        if change.is_err() {break;}
                        let selection=rx.borrow_and_update();target=selection.apply(&mut debounce);
                    }
                    _=tokio::time::sleep_until(tokio::time::Instant::from_std(due))=>{
                        let Some(content)=debounce.ready(Instant::now()) else {continue;};
                        if target.is_none() || target!=core.automatic_target() {continue;}
                        let target=target.as_ref().expect("clipboard target").clone();
                        match content {
                            Content::Text(text)=>{let _=core.send_observed_clipboard(target,text).await;}
                            Content::Image(bytes)=>{let _=core.send_observed_image(target,bytes).await;}
                        }
                    }
                }
            }
        }));
        Ok(Self {
            cancel,
            stopping,
            reads,
            thread: Some(thread),
            reader: Some(reader),
            sender,
        })
    }
    pub async fn shutdown(mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.cancel.cancel();
        self.reads.shutdown();
        self.sender.abort();
        let _ = (&mut self.sender).await;
        let threads = [self.thread.take(), self.reader.take()];
        let _ = tokio::task::spawn_blocking(move || {
            for thread in threads.into_iter().flatten() {
                let _ = thread.join();
            }
        })
        .await;
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.cancel.cancel();
        self.reads.shutdown();
        self.sender.abort();
    }
}
fn read_pipe(
    reader: &mut std::fs::File,
    limit: usize,
    stop: &AtomicBool,
    reads: &ReadQueue,
    generation: u64,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 32768];
    loop {
        ensure!(
            !stop.load(Ordering::SeqCst)
                && reads.generation.load(Ordering::SeqCst) == generation
                && Instant::now() < deadline,
            "clipboard read cancelled"
        );
        #[cfg(test)]
        reads.started.notify_one();
        let mut fd = [rustix::event::PollFd::new(
            &*reader,
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(
            &mut fd,
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 100_000_000,
            }),
        )?;
        if fd[0].revents().is_empty() {
            continue;
        }
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            return Ok(bytes);
        }
        ensure!(bytes.len() + n <= limit, "clipboard exceeds limit");
        bytes.extend_from_slice(&buffer[..n]);
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        io::Write,
        os::{fd::OwnedFd, unix::net::UnixStream},
    };
    fn pipe(bytes: &[u8]) -> Result<(std::fs::File, UnixStream)> {
        let (reader, mut writer) = UnixStream::pair()?;
        writer.write_all(bytes)?;
        Ok((std::fs::File::from(OwnedFd::from(reader)), writer))
    }
    struct ReaderGuard {
        reads: Arc<ReadQueue>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl ReaderGuard {
        fn start(
            reads: Arc<ReadQueue>,
            echo: Arc<EchoGuard>,
            tx: tokio::sync::watch::Sender<Selection>,
        ) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let (queue, stopping) = (reads.clone(), stop.clone());
            let thread = std::thread::spawn(move || read_jobs(&queue, &stopping, &echo, &tx));
            Self {
                reads,
                stop,
                thread: Some(thread),
            }
        }
    }
    impl Drop for ReaderGuard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.reads.shutdown();
            self.thread.take().unwrap().join().unwrap();
        }
    }
    pub(crate) async fn delayed_source_pause_resume(
        core: &Core,
        pause: impl Fn(bool),
    ) -> Result<()> {
        for slow_marker in [false, true] {
            let reads = Arc::new(ReadQueue::default());
            let (tx, mut rx) = tokio::sync::watch::channel(Selection {
                target: None,
                content: None,
                arrived: Instant::now(),
            });
            let worker =
                ReaderGuard::start(reads.clone(), Arc::new(EchoGuard::default()), tx.clone());
            let (selection, generation, _) = observe_offer(core, &reads, &tx, Instant::now());
            let old_target = selection.target.clone().expect("initial offer allowed");
            let (blocked, _unresponsive_source) = pipe(b"")?;
            let (reader, marker) = if slow_marker {
                let (reader, writer) = pipe(b"old text")?;
                drop(writer);
                (reader, Some(blocked))
            } else {
                (blocked, None)
            };
            reads.submit(
                ReadJob {
                    selection,
                    generation,
                    include_images: false,
                    mime: "text/plain",
                    reader,
                    marker,
                },
                &worker.stop,
            );
            tokio::time::timeout(Duration::from_secs(2), reads.started.notified()).await?;
            // Observation uses Core's actual pause policy while the old source
            // still holds its pipe open. Resume cannot confer authority on B.
            pause(true);
            let (paused, _, _) = observe_offer(core, &reads, &tx, Instant::now());
            assert!(paused.target.is_none());
            pause(false);
            assert!(rx.borrow().target.is_none());
            assert!(
                core.send_observed_clipboard(old_target, "old text".into())
                    .await
                    .is_err()
            );
            // A newly eligible offer proceeds promptly, without waiting for the
            // old marker/body's five-second read deadline.
            let (selection, generation, _) = observe_offer(core, &reads, &tx, Instant::now());
            let target = selection.target.clone().unwrap();
            rx.borrow_and_update();
            let (reader, writer) = pipe(b"latest")?;
            drop(writer);
            reads.submit(
                ReadJob {
                    selection,
                    generation,
                    include_images: false,
                    mime: "text/plain",
                    reader,
                    marker: None,
                },
                &worker.stop,
            );
            tokio::time::timeout(Duration::from_secs(1), rx.changed()).await??;
            assert_eq!(rx.borrow().content, Some(Content::Text("latest".into())));
            core.send_observed_clipboard(target, "latest".into())
                .await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn independent_reader_filters_markers_echo_and_bounds_content() -> Result<()> {
        let reads = Arc::new(ReadQueue::default());
        let echo = Arc::new(EchoGuard::default());
        echo.remember(b"echo");
        let (tx, mut rx) = tokio::sync::watch::channel(Selection {
            target: None,
            content: None,
            arrived: Instant::now(),
        });
        let worker = ReaderGuard::start(reads.clone(), echo, tx.clone());
        for (text, marker, expected) in [
            ("password", Some("secret"), None),
            ("echo", None, None),
            (
                "allowed",
                Some("public"),
                Some(Content::Text("allowed".into())),
            ),
        ] {
            let selection = Selection {
                target: Some(crate::core::ClipboardTarget {
                    device: pneu_proto::Id::from_bytes([1; 16]),
                    connection: 1,
                    generation: 1,
                }),
                content: None,
                arrived: Instant::now(),
            };
            let generation = reads.observe(selection.clone(), &tx);
            rx.borrow_and_update();
            let (reader, writer) = pipe(text.as_bytes())?;
            drop(writer);
            let marker = marker
                .map(|m| {
                    let (reader, writer) = pipe(m.as_bytes())?;
                    drop(writer);
                    Ok::<_, anyhow::Error>(reader)
                })
                .transpose()?;
            reads.submit(
                ReadJob {
                    selection,
                    generation,
                    include_images: false,
                    mime: "text/plain",
                    reader,
                    marker,
                },
                &worker.stop,
            );
            tokio::time::timeout(Duration::from_secs(2), rx.changed()).await??;
            assert_eq!(rx.borrow().content, expected);
        }
        let (mut reader, writer) = pipe(b"too large")?;
        drop(writer);
        assert!(
            read_pipe(
                &mut reader,
                2,
                &worker.stop,
                &reads,
                reads.generation.load(Ordering::SeqCst),
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn delayed_selection_delivery_keeps_offer_arrival_deadline() {
        let arrived = Instant::now() - Duration::from_secs(2);
        let mut debounce = Debounce::default();
        let mut selection = Selection {
            target: None,
            content: Some(Content::Text("first".into())),
            arrived,
        };
        selection.apply(&mut debounce);
        assert!(
            debounce
                .ready(arrived + Duration::from_millis(299))
                .is_none()
        );
        selection.arrived += Duration::from_millis(100);
        selection.content = Some(Content::Text("latest".into()));
        selection.apply(&mut debounce);
        assert_eq!(
            debounce.deadline(),
            Some(arrived + Duration::from_millis(400))
        );
        assert_eq!(
            debounce.ready(arrived + Duration::from_secs(2)),
            Some(Content::Text("latest".into()))
        );
        selection.apply(&mut debounce);
        selection.content = None;
        selection.apply(&mut debounce);
        assert!(debounce.ready(arrived + Duration::from_secs(3)).is_none());
    }
    #[test]
    fn filtering_and_echo() {
        let echo = EchoGuard::default();
        echo.remember(b"phone text");
        assert!(filter("text/plain", b"phone text".to_vec(), false, false, &echo).is_none());
        echo.remember(b"new phone text");
        assert_eq!(
            filter("text/plain", b"phone text".to_vec(), false, false, &echo),
            Some(Content::Text("phone text".into()))
        );
        assert!(filter("text/plain", b"password".to_vec(), true, false, &echo).is_none());
        assert!(sensitive_mime("application/x-keepassxc-secret"));
        assert!(!sensitive_mime("x-kde-passwordManagerHint"));
        assert!(filter("text/plain", vec![255], false, false, &echo).is_none());
        assert!(filter("text/plain", vec![0], false, false, &echo).is_none());
        let png = b"\x89PNG\r\n\x1a\n".to_vec();
        assert!(filter("image/png", png.clone(), false, false, &echo).is_none());
        assert_eq!(
            filter("image/png", png.clone(), false, true, &echo),
            Some(Content::Image(png))
        );
        assert!(filter("image/png", vec![1; MAX_IMAGE + 1], false, true, &echo).is_none());
        assert_eq!(
            filter("text/plain", b"text".to_vec(), false, false, &echo),
            Some(Content::Text("text".into()))
        );
    }
    #[test]
    fn debounce_keeps_latest_and_sensitive_change_cancels_pending() {
        let now = Instant::now();
        let mut d = Debounce::default();
        d.change(Some(Content::Text("a".into())), now);
        assert!(d.ready(now + Duration::from_millis(299)).is_none());
        d.change(
            Some(Content::Text("b".into())),
            now + Duration::from_millis(100),
        );
        assert!(d.ready(now + Duration::from_millis(300)).is_none());
        assert_eq!(
            d.ready(now + Duration::from_millis(400)),
            Some(Content::Text("b".into()))
        );
        d.change(Some(Content::Text("c".into())), now);
        d.change(None, now);
        assert!(d.ready(now + Duration::from_secs(1)).is_none());
    }
}
