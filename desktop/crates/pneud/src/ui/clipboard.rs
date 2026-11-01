//! Event-driven data-control watcher, with bounded reads and a cancellable owner
//! thread. Selection data is read from the actual offer (never a later selection).
use crate::Core;
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::{
    collections::VecDeque,
    io::Read,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wl_clipboard_rs::{
    paste::Seat,
    watch::{ClipboardEvent, ClipboardType, Watcher},
};

pub const ORIGIN_MIME: &str = "application/x-pneu-origin";
pub use super::clipboard_content::MAX_IMAGE;
const IMAGE_MIMES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
];
const TEXT_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];
#[cfg(test)]
#[derive(Default)]
pub struct EchoGuard(Mutex<EchoSelection>);
#[cfg(test)]
#[derive(Default)]
struct EchoSelection {
    generation: u64,
    hashes: Vec<[u8; 32]>,
}
#[cfg(test)]
impl EchoGuard {
    pub fn remember(&self, bytes: &[u8]) {
        self.replace_hashes([Sha256::digest(bytes).into()]);
    }
    pub fn replace_hashes(&self, hashes: impl IntoIterator<Item = [u8; 32]>) -> u64 {
        let mut current = self.0.lock().expect("clipboard echo");
        current.generation += 1;
        current.hashes = hashes.into_iter().take(8).collect();
        current.generation
    }
    pub fn contains(&self, bytes: &[u8]) -> bool {
        self.0
            .lock()
            .expect("clipboard echo")
            .hashes
            .contains(&Sha256::digest(bytes).into())
    }
}

pub(crate) fn install_marker(ticket: u128) -> String {
    format!("application/x-pneu-install-{ticket:032x}")
}
fn install_ticket(mimes: &[String]) -> Option<u128> {
    mimes.iter().find_map(|mime| {
        mime.strip_prefix("application/x-pneu-install-")
            .and_then(|ticket| u128::from_str_radix(ticket, 16).ok())
    })
}
pub use super::clipboard_content::Content;

#[cfg(test)]
#[derive(Default)]
pub struct Debounce {
    pending: Option<(Content, Instant)>,
}
#[cfg(test)]
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
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
enum Observation {
    Echo,
    UnshareableLocal,
    ShareableLocal(Content),
}
#[cfg(test)]
impl Observation {
    fn content(self) -> Option<Content> {
        match self {
            Self::ShareableLocal(content) => Some(content),
            _ => None,
        }
    }
}
// Classify independently of recipients and sharing policy. Unknown/failed reads
// are local observations; only positive origin/hash evidence identifies echoes.
#[cfg(test)]
fn classify_observation(
    origin: bool,
    unshareable: bool,
    content: Option<Content>,
    echo: &EchoGuard,
) -> Observation {
    if origin {
        return Observation::Echo;
    }
    if unshareable {
        return Observation::UnshareableLocal;
    }
    let (text, image) = match content {
        Some(Content::Text(text)) => (Some(text), None),
        Some(Content::Image(image)) => (None, Some(image)),
        Some(Content::Formats(text, image)) => (Some(text), Some(image)),
        None => return Observation::UnshareableLocal,
    };
    let text = text.filter(|text| !echo.contains(text.as_bytes()));
    let image = image.filter(|image| !echo.contains(image));
    match (text, image) {
        (Some(text), Some(image)) => Observation::ShareableLocal(Content::Formats(text, image)),
        (Some(text), None) => Observation::ShareableLocal(Content::Text(text)),
        (None, Some(image)) => Observation::ShareableLocal(Content::Image(image)),
        (None, None) => Observation::Echo,
    }
}
#[cfg(test)]
pub fn filter(
    mime: &str,
    bytes: Vec<u8>,
    secret: bool,
    include_images: bool,
    echo: &EchoGuard,
) -> Option<Content> {
    classify_observation(
        false,
        secret,
        decode_payload(mime, bytes, include_images),
        echo,
    )
    .content()
}
fn decode_payload(mime: &str, bytes: Vec<u8>, include_images: bool) -> Option<Content> {
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
#[cfg(test)]
async fn complete_observation(core: &Core, selection: &mut Selection, observation: Observation) {
    let token = selection.observation.expect("watched observation");
    let echo = matches!(observation, Observation::Echo);
    selection.content = observation.content();
    if echo {
        core.clipboard_test_echo(token);
        core.clipboard_test_idle().await;
    } else {
        core.clipboard_test_read(token, selection.content.clone())
            .await;
    }
}
#[derive(Clone)]
struct Selection {
    #[cfg(test)]
    target: Option<crate::core::ClipboardOffer>,
    #[cfg(test)]
    content: Option<Content>,
    arrived: Instant,
    observation: Option<u64>,
}
#[cfg(test)]
impl Selection {
    fn apply(&self, debounce: &mut Debounce) -> Option<crate::core::ClipboardOffer> {
        debounce.change(self.content.clone(), self.arrived);
        self.target.clone()
    }
}
// One reader and one latest-only slot bound memory and descriptors even when
// sources never finish. Observation never waits for data, and every new event
// cancels the current read before capturing the next offer's policy.
struct ReadJob {
    selection: Selection,
    #[cfg(test)]
    generation: u64,
    readers: Vec<PayloadReader>,
    marker: Option<std::fs::File>,
}
struct PayloadReader {
    mime: &'static str,
    reader: std::fs::File,
}
#[cfg(test)]
#[derive(Default)]
struct ReadQueue {
    pending: Mutex<Option<ReadJob>>,
    ready: Condvar,
    generation: AtomicU64,
    #[cfg(test)]
    started: tokio::sync::Notify,
}
#[cfg(test)]
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
#[cfg(test)]
fn observe_offer(
    core: &Core,
    reads: &ReadQueue,
    tx: &tokio::sync::watch::Sender<Selection>,
    arrived: Instant,
) -> (Selection, u64) {
    let selection = Selection {
        observation: None,
        target: core.observe_clipboard_offer(),
        content: None,
        arrived,
    };
    let generation = reads.observe(selection.clone(), tx);
    (selection, generation)
}
fn prepare_job(
    event: ClipboardEvent<'_>,
    selection: Selection,
    generation: u64,
) -> Result<Option<ReadJob>> {
    let ClipboardEvent::Changed {
        mime_types,
        mut offer,
        ..
    } = event
    else {
        return Ok(None);
    };
    prepare_read_job(&mime_types, selection, generation, |mime| {
        request_pipe(&mut offer, mime)
    })
}
fn prepare_read_job(
    mime_types: &[String],
    selection: Selection,
    _generation: u64,
    mut receive: impl FnMut(&str) -> Result<std::fs::File>,
) -> Result<Option<ReadJob>> {
    if mime_types
        .iter()
        .any(|m| sensitive_mime(m) || (m == ORIGIN_MIME && install_ticket(mime_types).is_none()))
    {
        return Ok(None);
    }
    // Read for echo classification even when sharing is disabled, paused or
    // there are no recipients. Publication still uses the frozen recipient policy.
    // Retain text aliases too, so a failed preferred alias cannot hide valid text.
    let formats: Vec<_> = TEXT_MIMES
        .into_iter()
        .chain(IMAGE_MIMES)
        .filter(|mime| mime_types.iter().any(|offered| offered == mime))
        .collect();
    if formats.is_empty() {
        return Ok(None);
    }
    // Request every needed representation before dropping the borrowed offer.
    // One bounded reader drains their pipes together so neither can block the other.
    let marker = mime_types
        .iter()
        .find(|m| m.eq_ignore_ascii_case("x-kde-passwordManagerHint"))
        .map(|m| receive(m))
        .transpose()?;
    let mut readers = Vec::new();
    for mime in formats {
        // A failed payload request must not discard another usable format.
        // Marker failures above still suppress the whole offer.
        if let Ok(reader) = receive(mime) {
            readers.push(PayloadReader { mime, reader });
        }
    }
    if readers.is_empty() {
        return Ok(None);
    }
    Ok(Some(ReadJob {
        selection,
        #[cfg(test)]
        generation: _generation,
        readers,
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
#[cfg(test)]
fn read_jobs(
    reads: &ReadQueue,
    stop: &AtomicBool,
    echo: &EchoGuard,
    tx: &tokio::sync::watch::Sender<Selection>,
    observer: Option<(&std::sync::Weak<Core>, &tokio::runtime::Handle)>,
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
            Ok(read_payloads(
                &mut job.readers,
                stop,
                reads,
                job.generation,
                deadline,
            ))
        })();
        let observation = classify_observation(false, false, result.unwrap_or(None), echo);
        // Serialize publication with observation: an old read cannot overwrite
        // cancellation/new selection even if it completes at the same instant.
        let _pending = reads.pending.lock().expect("clipboard reads");
        if !stop.load(Ordering::SeqCst) && reads.generation.load(Ordering::SeqCst) == job.generation
        {
            if let Some((weak, runtime)) = observer
                && let Some(core) = weak.upgrade()
            {
                runtime.block_on(complete_observation(&core, &mut job.selection, observation));
            } else {
                job.selection.content = observation.content();
            }
            tx.send_replace(job.selection);
        }
    }
}
#[derive(Default)]
struct ActorReadState {
    pending: VecDeque<(ReadJob, Arc<AtomicBool>)>,
    cancellations: Vec<std::sync::Weak<AtomicBool>>,
    stopped: bool,
}
#[derive(Default)]
struct ActorReads {
    state: Mutex<ActorReadState>,
    ready: Condvar,
}
impl ActorReads {
    fn submit(&self, job: ReadJob, cancel: Arc<AtomicBool>) -> Option<ReadJob> {
        let mut state = self.state.lock().expect("clipboard readers");
        if state.stopped {
            return Some(job);
        }
        state
            .pending
            .retain(|(_, cancel)| !cancel.load(Ordering::SeqCst));
        state
            .cancellations
            .retain(|cancel| cancel.strong_count() > 0);
        let dropped = if state.pending.len() == 8 {
            state.pending.pop_front().map(|(job, old)| {
                old.store(true, Ordering::SeqCst);
                job
            })
        } else {
            None
        };
        state.cancellations.push(Arc::downgrade(&cancel));
        state.pending.push_back((job, cancel));
        self.ready.notify_one();
        dropped
    }
    fn take(&self) -> Option<(ReadJob, Arc<AtomicBool>)> {
        let mut state = self.state.lock().expect("clipboard readers");
        loop {
            if state.stopped {
                return None;
            }
            if let Some(job) = state.pending.pop_front() {
                if job.1.load(Ordering::SeqCst) {
                    continue;
                }
                return Some(job);
            }
            state = self.ready.wait(state).expect("clipboard readers");
        }
    }
    fn shutdown(&self) {
        let mut state = self.state.lock().expect("clipboard readers");
        state.stopped = true;
        for cancel in state.cancellations.drain(..).filter_map(|c| c.upgrade()) {
            cancel.store(true, Ordering::SeqCst);
        }
        state.pending.clear();
        self.ready.notify_all();
    }
}
fn read_actor_jobs(reads: &ActorReads, core: &std::sync::Weak<Core>) {
    while let Some((mut job, cancel)) = reads.take() {
        let deadline = job.selection.arrived + Duration::from_secs(5);
        let (content, fingerprints) = (|| -> Result<(Option<Content>, Vec<[u8; 32]>)> {
            if let Some(marker) = job.marker.as_mut() {
                let bytes = read_pipe_raw(marker, 1024, &cancel, || true, deadline)?;
                if String::from_utf8_lossy(&bytes)
                    .trim_matches(|c: char| c.is_whitespace() || c == '\0')
                    .eq_ignore_ascii_case("secret")
                {
                    return Ok((None, Vec::new()));
                }
            }
            Ok(read_payloads_bounded(
                &mut job.readers,
                &cancel,
                || true,
                deadline,
                true,
            ))
        })()
        .unwrap_or((None, Vec::new()));
        if let Some(core) = core.upgrade() {
            core.clipboard_observed_hashes(
                job.selection.observation.expect("watcher token"),
                content,
                fingerprints,
                true,
            );
        } else {
            break;
        }
    }
}
#[cfg(test)]
fn read_payloads(
    readers: &mut Vec<PayloadReader>,
    stop: &AtomicBool,
    reads: &ReadQueue,
    generation: u64,
    deadline: Instant,
) -> Option<Content> {
    read_payloads_raw(
        readers,
        stop,
        || {
            reads.started.notify_one();
            reads.generation.load(Ordering::SeqCst) == generation
        },
        deadline,
    )
}
#[cfg(test)]
fn read_payloads_raw(
    readers: &mut Vec<PayloadReader>,
    stop: &AtomicBool,
    current: impl Fn() -> bool,
    deadline: Instant,
) -> Option<Content> {
    read_payloads_bounded(readers, stop, current, deadline, false).0
}
fn image_header(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        "image/bmp" => bytes.starts_with(b"BM"),
        _ => false,
    }
}
fn read_payloads_bounded(
    readers: &mut Vec<PayloadReader>,
    stop: &AtomicBool,
    current: impl Fn() -> bool,
    deadline: Instant,
    observe: bool,
) -> (Option<Content>, Vec<[u8; 32]>) {
    let mut pending: Vec<_> = readers
        .drain(..)
        .map(|reader| (reader, Vec::new()))
        .collect();
    let (mut text, mut image) = (None, None);
    let mut fingerprints = Vec::new();
    let mut total = 0usize;
    while !pending.is_empty()
        && !stop.load(Ordering::SeqCst)
        && current()
        && Instant::now() < deadline
    {
        let mut fds: Vec<_> = pending
            .iter()
            .map(|(reader, _)| {
                rustix::event::PollFd::new(&reader.reader, rustix::event::PollFlags::IN)
            })
            .collect();
        if rustix::event::poll(
            &mut fds,
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 100_000_000,
            }),
        )
        .is_err()
        {
            break;
        }
        let ready: Vec<_> = fds.iter().map(|fd| !fd.revents().is_empty()).collect();
        drop(fds);
        // Each pass reads at most one chunk per pipe. Failed/oversized formats
        // are discarded individually; the secret marker above gates the whole offer.
        for index in (0..pending.len()).rev().filter(|index| ready[*index]) {
            let (reader, bytes) = &mut pending[index];
            let mut buffer = [0; 32768];
            let limit = if observe && reader.mime == "image/png" {
                32 * 1024 * 1024
            } else if observe && IMAGE_MIMES.contains(&reader.mime) {
                crate::store::history::MAX_IMAGE
            } else if reader.mime == "image/png" {
                MAX_IMAGE
            } else if observe {
                crate::store::history::MAX_TEXT
            } else {
                pneu_proto::MAX_CLIPBOARD_BYTES
            };
            match reader.reader.read(&mut buffer) {
                Ok(0) => {
                    let (reader, bytes) = pending.remove(index);
                    let is_image = image_header(reader.mime, &bytes);
                    let content = if reader.mime == "image/png" || TEXT_MIMES.contains(&reader.mime)
                    {
                        if observe && TEXT_MIMES.contains(&reader.mime) {
                            String::from_utf8(bytes.clone())
                                .ok()
                                .filter(|s| !s.contains('\0'))
                                .map(Content::Text)
                        } else {
                            decode_payload(reader.mime, bytes.clone(), true)
                        }
                    } else if observe && is_image && bytes.len() <= crate::store::history::MAX_IMAGE
                    {
                        (|| -> Result<Content> {
                            let image = crate::actions::decode_image(&bytes)?;
                            let mut png = std::io::Cursor::new(Vec::new());
                            image.write_to(&mut png, image::ImageFormat::Png)?;
                            ensure!(
                                png.get_ref().len() <= crate::store::history::MAX_IMAGE,
                                "history image limit"
                            );
                            Ok(Content::Image(png.into_inner()))
                        })()
                        .ok()
                    } else {
                        None
                    };
                    if observe && (is_image || content.is_some()) {
                        let hash: [u8; 32] = Sha256::digest(&bytes).into();
                        if !fingerprints.contains(&hash) {
                            fingerprints.push(hash);
                        }
                    }
                    match content {
                        Some(Content::Text(value)) => {
                            let rank = TEXT_MIMES
                                .iter()
                                .position(|mime| *mime == reader.mime)
                                .unwrap_or(TEXT_MIMES.len());
                            if text.as_ref().is_none_or(|(current, _)| rank < *current) {
                                text = Some((rank, value));
                            }
                        }
                        Some(Content::Image(value)) => image = Some(value),
                        _ => {}
                    }
                }
                Ok(n) if bytes.len() + n <= limit && total + n <= 64 * 1024 * 1024 => {
                    total += n;
                    bytes.extend_from_slice(&buffer[..n]);
                }
                _ => {
                    pending.remove(index);
                }
            }
        }
    }
    let content = match (text, image) {
        (Some((_, text)), Some(image)) => Some(Content::Formats(text, image)),
        (Some((_, text)), None) => Some(Content::Text(text)),
        (None, Some(image)) => Some(Content::Image(image)),
        (None, None) => None,
    };
    (content, fingerprints)
}
pub struct Watch {
    cancel: wl_clipboard_rs::watch::CancelHandle,
    stopping: Arc<AtomicBool>,
    reads: Arc<ActorReads>,
    thread: Option<std::thread::JoinHandle<()>>,
    readers: Vec<std::thread::JoinHandle<()>>,
}
impl Watch {
    pub fn start(core: Arc<Core>) -> Result<Self> {
        let mut watcher = Watcher::new(ClipboardType::Regular, Seat::Unspecified)?;
        let cancel = watcher.cancel_handle();
        let stopping = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(ActorReads::default());
        let mut readers = Vec::new();
        for index in 0..2 {
            let (queue, weak) = (reads.clone(), Arc::downgrade(&core));
            match std::thread::Builder::new()
                .name(format!("pneu-clipboard-reader-{index}"))
                .spawn(move || read_actor_jobs(&queue, &weak))
            {
                Ok(thread) => readers.push(thread),
                Err(error) => {
                    reads.shutdown();
                    cancel.cancel();
                    for thread in readers {
                        let _ = thread.join();
                    }
                    return Err(error.into());
                }
            }
        }
        let (stop, observed_reads, weak) = (stopping.clone(), reads.clone(), Arc::downgrade(&core));
        let thread = std::thread::Builder::new()
            .name("pneu-clipboard-watch".into())
            .spawn(move || {
                let mut first = true;
                while !stop.load(Ordering::SeqCst) {
                    let event = match watcher.next_event() {
                        Ok(Some(event)) => event,
                        _ => break,
                    };
                    let mimes = match &event {
                        ClipboardEvent::Changed { mime_types, .. } => mime_types.as_slice(),
                        _ => &[],
                    };
                    let Some(core) = weak.upgrade() else {
                        break;
                    };
                    let marker = install_ticket(mimes);
                    if marker.is_some_and(|ticket| core.clipboard_install_marker(ticket)) {
                        first = false;
                        continue;
                    }
                    let initial = std::mem::replace(&mut first, false);
                    if marker.is_none() && mimes.iter().any(|mime| mime == ORIGIN_MIME) {
                        continue;
                    }
                    let (_target, token) = core.observe_watched_clipboard_metadata(
                        false,
                        !matches!(&event, ClipboardEvent::Changed { .. }),
                        initial,
                        marker.is_some(),
                    );
                    let selection = Selection {
                        observation: Some(token),
                        arrived: Instant::now(),
                        #[cfg(test)]
                        target: _target,
                        #[cfg(test)]
                        content: None,
                    };
                    if let Ok(Some(job)) = prepare_job(event, selection, 0) {
                        let canceled = Arc::new(AtomicBool::new(false));
                        core.register_clipboard_read(token, canceled.clone());
                        if let Some(dropped) = observed_reads.submit(job, canceled) {
                            core.clipboard_observed(
                                dropped.selection.observation.expect("read token"),
                                None,
                                false,
                            );
                        }
                    } else {
                        core.clipboard_observed(token, None, false);
                    }
                }
                if !stop.swap(true, Ordering::SeqCst)
                    && let Some(core) = weak.upgrade()
                {
                    // The selection is unknown while disconnected. Invalidate
                    // cached manual shares before the runtime reconnects us.
                    let (_, token) = core.observe_watched_clipboard_kind(false, true, true);
                    core.clipboard_observed(token, None, false);
                }
                observed_reads.shutdown();
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                stopping.store(true, Ordering::SeqCst);
                cancel.cancel();
                reads.shutdown();
                for thread in readers {
                    let _ = thread.join();
                }
                return Err(error.into());
            }
        };
        Ok(Self {
            cancel,
            stopping,
            reads,
            thread: Some(thread),
            readers,
        })
    }
    pub(super) fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }
    pub async fn shutdown(mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.cancel.cancel();
        self.reads.shutdown();
        let mut threads = std::mem::take(&mut self.readers);
        threads.extend(self.thread.take());
        let _ = tokio::task::spawn_blocking(move || {
            for thread in threads {
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
    }
}
#[cfg(test)]
fn read_pipe(
    reader: &mut std::fs::File,
    limit: usize,
    stop: &AtomicBool,
    reads: &ReadQueue,
    generation: u64,
    deadline: Instant,
) -> Result<Vec<u8>> {
    read_pipe_raw(
        reader,
        limit,
        stop,
        || {
            reads.started.notify_one();
            reads.generation.load(Ordering::SeqCst) == generation
        },
        deadline,
    )
}
fn read_pipe_raw(
    reader: &mut std::fs::File,
    limit: usize,
    stop: &AtomicBool,
    current: impl Fn() -> bool,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 32768];
    loop {
        ensure!(
            !stop.load(Ordering::SeqCst) && current() && Instant::now() < deadline,
            "clipboard read cancelled"
        );
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
    #[derive(Default)]
    struct NullActions;
    #[async_trait::async_trait]
    impl crate::actions::PcActions for NullActions {
        async fn clipboard_text(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn clipboard_image(&self, _: &std::path::Path, _: &str) -> Result<()> {
            Ok(())
        }
        async fn notify(&self, _: &std::path::Path, _: &str) -> Result<()> {
            Ok(())
        }
        async fn open(&self, _: &std::path::Path) -> Result<()> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn filtered_failed_and_echo_offers_classify_before_releasing_remote_effects() -> Result<()>
    {
        let dir = crate::test_support::scratch()?;
        let paths = crate::Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config.toml"),
            socket: dir.path().join("socket"),
        };
        let core = Core::new(
            paths.clone(),
            crate::Config {
                save_path: dir.path().join("saved"),
                ..Default::default()
            },
            Arc::new(NullActions),
        )
        .await?;
        let db = rusqlite::Connection::open(paths.data.join("state.sqlite3"))?;
        let echo = Arc::new(EchoGuard::default());
        echo.replace_hashes([
            Sha256::digest(b"remote text").into(),
            Sha256::digest(b"\x89PNG\r\n\x1a\nremote image").into(),
        ]);
        let oversized = vec![b't'; pneu_proto::MAX_CLIPBOARD_BYTES + 1];
        let oversized_image = vec![1; MAX_IMAGE + 1];
        type Case<'a> = (&'a str, Vec<(&'a str, &'a [u8])>, Option<&'a str>, bool);
        let cases: Vec<Case<'_>> = vec![
            (
                "text echo",
                vec![("text/plain", b"remote text")],
                None,
                false,
            ),
            (
                "image echo",
                vec![("image/png", b"\x89PNG\r\n\x1a\nremote image")],
                None,
                false,
            ),
            (
                "origin echo",
                vec![(ORIGIN_MIME, b"1"), ("text/plain", b"remote")],
                None,
                false,
            ),
            (
                "local text",
                vec![("text/plain", b"local text")],
                None,
                true,
            ),
            (
                "local image",
                vec![("image/png", b"\x89PNG\r\n\x1a\nlocal image")],
                None,
                true,
            ),
            (
                "secret marker",
                vec![
                    ("text/plain", b"secret"),
                    ("x-kde-passwordManagerHint", b" secret\0 "),
                ],
                None,
                true,
            ),
            (
                "sensitive MIME",
                vec![
                    ("text/plain", b"secret"),
                    ("application/x-keepass-secret", b""),
                ],
                None,
                true,
            ),
            (
                "oversized text",
                vec![("text/plain", &oversized)],
                None,
                true,
            ),
            (
                "oversized image",
                vec![("image/png", &oversized_image)],
                None,
                true,
            ),
            ("invalid text", vec![("text/plain", &[255])], None, true),
            ("NUL text", vec![("text/plain", b"local\0")], None, true),
            ("invalid image", vec![("image/png", b"bad PNG")], None, true),
            (
                "unsupported",
                vec![("application/octet-stream", b"local")],
                None,
                true,
            ),
            ("clear", vec![], None, true),
            (
                "request failure",
                vec![("text/plain", b"local")],
                Some("request"),
                true,
            ),
            (
                "read failure",
                vec![("text/plain", b"local")],
                Some("read"),
                true,
            ),
            (
                "payload timeout",
                vec![("text/plain", b"local")],
                Some("timeout"),
                true,
            ),
            (
                "marker request failure",
                vec![
                    ("x-kde-passwordManagerHint", b"public"),
                    ("text/plain", b"local"),
                ],
                Some("request"),
                true,
            ),
            (
                "marker read failure",
                vec![
                    ("x-kde-passwordManagerHint", b"public"),
                    ("text/plain", b"local"),
                ],
                Some("read"),
                true,
            ),
            (
                "marker timeout",
                vec![
                    ("x-kde-passwordManagerHint", b"public"),
                    ("text/plain", b"local"),
                ],
                Some("timeout"),
                true,
            ),
        ];
        for (name, formats, failure, local) in cases {
            let before: String =
                db.query_row("SELECT last_seen FROM mesh_clock", [], |row| row.get(0))?;
            deliver_fixture_offer(&core, &formats, echo.clone(), failure, || {}).await?;
            let after: String =
                db.query_row("SELECT last_seen FROM mesh_clock", [], |row| row.get(0))?;
            assert_eq!(
                after.parse::<u64>()? > before.parse::<u64>()?,
                local,
                "{name}"
            );
            if local {
                let writer: String =
                    db.query_row("SELECT source_device FROM clipboard_clock", [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(writer, core.identity.device_id.as_str(), "{name}");
            }
        }
        core.shutdown_actions().await
    }
    #[test]
    fn mixed_echo_formats_never_forward_received_representations() {
        let echo = EchoGuard::default();
        echo.remember(b"received text");
        let image = b"\x89PNG\r\n\x1a\nlocal image".to_vec();
        assert_eq!(
            classify_observation(
                false,
                false,
                Some(Content::Formats("received text".into(), image.clone())),
                &echo
            ),
            Observation::ShareableLocal(Content::Image(image))
        );
        echo.remember(b"\x89PNG\r\n\x1a\nreceived image");
        assert_eq!(
            classify_observation(
                false,
                false,
                Some(Content::Formats(
                    "local text".into(),
                    b"\x89PNG\r\n\x1a\nreceived image".to_vec()
                )),
                &echo
            ),
            Observation::ShareableLocal(Content::Text("local text".into()))
        );
    }
    #[test]
    fn retired_install_mime_with_origin_still_requests_payload_for_provenance() -> Result<()> {
        let mut requested = Vec::new();
        let job = prepare_read_job(
            &[install_marker(42), ORIGIN_MIME.into(), "text/plain".into()],
            Selection {
                observation: Some(1),
                target: None,
                content: None,
                arrived: Instant::now(),
            },
            0,
            |mime| {
                requested.push(mime.to_owned());
                let (reader, writer) = pipe(b"received bytes")?;
                drop(writer);
                Ok(reader)
            },
        )?;
        assert!(job.is_some());
        assert_eq!(requested, vec!["text/plain"]);
        Ok(())
    }
    #[test]
    fn targetless_offers_still_request_payloads_for_echo_classification() -> Result<()> {
        let mut requested = Vec::new();
        let job = prepare_read_job(
            &["text/plain".into(), "image/png".into()],
            Selection {
                observation: Some(1),
                target: None,
                content: None,
                arrived: Instant::now(),
            },
            0,
            |mime| {
                requested.push(mime.to_owned());
                let (reader, writer) = pipe(b"received bytes")?;
                drop(writer);
                Ok(reader)
            },
        )?;
        assert!(
            job.is_some(),
            "disabled sharing still needs echo classification"
        );
        assert_eq!(requested, ["text/plain", "image/png"]);
        Ok(())
    }
    #[tokio::test]
    async fn marker_stripped_echo_reader_does_not_commit_an_unpublished_stamp() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let paths = crate::Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config.toml"),
            socket: dir.path().join("socket"),
        };
        let core = Core::new(
            paths.clone(),
            crate::Config {
                save_path: dir.path().join("saved"),
                ..Default::default()
            },
            Arc::new(NullActions),
        )
        .await?;
        let db = rusqlite::Connection::open(paths.data.join("state.sqlite3"))?;
        let remote = crate::now_ms();
        let source = "11".repeat(16);
        db.execute("UPDATE mesh_clock SET last_seen=?", [remote.to_string()])?;
        db.execute(
            "INSERT INTO clipboard_clock VALUES(1,1,?,?)",
            rusqlite::params![source, remote.to_string()],
        )?;
        let reads = Arc::new(ReadQueue::default());
        let stop = Arc::new(AtomicBool::new(false));
        let echo = Arc::new(EchoGuard::default());
        echo.remember(b"remote bytes");
        let (tx, mut rx) = tokio::sync::watch::channel(Selection {
            observation: None,
            target: None,
            content: None,
            arrived: Instant::now(),
        });
        let observer = Arc::downgrade(&core);
        let runtime = tokio::runtime::Handle::current();
        let (queue, stopping, sender) = (reads.clone(), stop.clone(), tx.clone());
        let thread = std::thread::spawn(move || {
            read_jobs(
                &queue,
                &stopping,
                &echo,
                &sender,
                Some((&observer, &runtime)),
            )
        });
        let guard = ReaderGuard {
            reads: reads.clone(),
            stop: stop.clone(),
            thread: Some(thread),
        };
        for (bytes, is_echo) in [
            (b"remote bytes".as_slice(), true),
            (b"real local".as_slice(), false),
            (b"invalid\0local".as_slice(), false),
        ] {
            let before: String =
                db.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?;
            let (target, revision) = core.observe_watched_clipboard(false);
            let selection = Selection {
                observation: Some(revision),
                target,
                content: None,
                arrived: Instant::now(),
            };
            let generation = reads.observe(selection.clone(), &tx);
            rx.borrow_and_update();
            let (reader, writer) = pipe(bytes)?;
            drop(writer);
            reads.submit(
                ReadJob {
                    selection,
                    generation,
                    readers: vec![PayloadReader {
                        mime: "text/plain",
                        reader,
                    }],
                    marker: None,
                },
                &stop,
            );
            tokio::time::timeout(Duration::from_secs(3), rx.changed()).await??;
            let seen: String =
                db.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?;
            if is_echo {
                assert_eq!(seen, remote.to_string());
                assert!(rx.borrow().content.is_none());
            } else {
                assert!(
                    seen.parse::<u64>()? > before.parse::<u64>()?,
                    "filtered local observation must advance ordering"
                );
                assert_eq!(rx.borrow().content.is_some(), !bytes.contains(&0));
            }
        }
        tokio::task::spawn_blocking(move || drop(guard)).await?;
        core.shutdown_actions().await?;
        Ok(())
    }
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
        fn observe(
            core: &Arc<Core>,
            reads: Arc<ReadQueue>,
            echo: Arc<EchoGuard>,
            tx: tokio::sync::watch::Sender<Selection>,
        ) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let (queue, stopping) = (reads.clone(), stop.clone());
            let observer = Arc::downgrade(core);
            let runtime = tokio::runtime::Handle::current();
            let thread = std::thread::spawn(move || {
                read_jobs(&queue, &stopping, &echo, &tx, Some((&observer, &runtime)))
            });
            Self {
                reads,
                stop,
                thread: Some(thread),
            }
        }
        fn start(
            reads: Arc<ReadQueue>,
            echo: Arc<EchoGuard>,
            tx: tokio::sync::watch::Sender<Selection>,
        ) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let (queue, stopping) = (reads.clone(), stop.clone());
            let thread = std::thread::spawn(move || read_jobs(&queue, &stopping, &echo, &tx, None));
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
    /// Exercise production observation, MIME selection, bounded reads, debounce and publication
    /// without a Wayland compositor. The callback supplies the physical offer's pipes.
    pub(crate) async fn deliver_offer(
        core: &Arc<Core>,
        formats: &[(&str, &[u8])],
    ) -> Result<Vec<String>> {
        deliver_offer_after_observation(core, formats, || {}).await
    }
    pub(crate) async fn deliver_offer_after_observation(
        core: &Arc<Core>,
        formats: &[(&str, &[u8])],
        after_observation: impl FnOnce(),
    ) -> Result<Vec<String>> {
        deliver_fixture_offer(
            core,
            formats,
            Arc::new(EchoGuard::default()),
            None,
            after_observation,
        )
        .await
    }
    pub(crate) async fn deliver_fixture_offer(
        core: &Arc<Core>,
        formats: &[(&str, &[u8])],
        echo: Arc<EchoGuard>,
        failure: Option<&str>,
        after_observation: impl FnOnce(),
    ) -> Result<Vec<String>> {
        let reads = Arc::new(ReadQueue::default());
        let (tx, mut rx) = tokio::sync::watch::channel(Selection {
            observation: None,
            target: None,
            content: None,
            arrived: Instant::now(),
        });
        let worker = ReaderGuard::observe(core, reads.clone(), echo.clone(), tx.clone());
        let origin = formats.iter().any(|(mime, _)| *mime == ORIGIN_MIME);
        let (target, revision) = core.observe_watched_clipboard(origin);
        let arrived = Instant::now()
            - if failure == Some("timeout") {
                Duration::from_millis(4900)
            } else {
                Duration::ZERO
            };
        let selection = Selection {
            target,
            observation: Some(revision),
            content: None,
            arrived,
        };
        let generation = reads.observe(selection.clone(), &tx);
        after_observation();
        rx.borrow_and_update();
        let mut requested = Vec::new();
        let mimes: Vec<_> = formats.iter().map(|(mime, _)| (*mime).to_owned()).collect();
        let mut stalled = None;
        let mut writer_task = None;
        let prepared = prepare_read_job(&mimes, selection.clone(), generation, |mime| {
            requested.push(mime.to_owned());
            if failure == Some("request") {
                anyhow::bail!("injected request failure");
            }
            if failure == Some("read") {
                return Ok(std::fs::File::open(".")?);
            }
            let (_, bytes) = formats.iter().find(|(m, _)| *m == mime).unwrap();
            if bytes.len() > 100_000 {
                let (reader, mut writer) = UnixStream::pair()?;
                let bytes = bytes.to_vec();
                writer_task = Some(std::thread::spawn(move || {
                    let _ = writer.write_all(&bytes);
                }));
                return Ok(std::fs::File::from(OwnedFd::from(reader)));
            }
            let (reader, writer) = pipe(bytes)?;
            if failure == Some("timeout") {
                stalled = Some(writer);
            } else {
                drop(writer);
            }
            Ok(reader)
        });
        if let Ok(Some(job)) = prepared {
            reads.submit(job, &worker.stop);
            tokio::time::timeout(Duration::from_secs(2), rx.changed()).await??;
        } else {
            let mut selection = selection;
            complete_observation(
                core,
                &mut selection,
                classify_observation(origin, false, None, &echo),
            )
            .await;
        }
        if let Some(task) = writer_task {
            task.join().unwrap();
        }
        Ok(requested)
    }
    pub(crate) async fn delayed_source_pause_resume(
        core: &Core,
        pause: impl Fn(bool),
    ) -> Result<()> {
        for slow_marker in [false, true] {
            let reads = Arc::new(ReadQueue::default());
            let (tx, mut rx) = tokio::sync::watch::channel(Selection {
                observation: None,
                target: None,
                content: None,
                arrived: Instant::now(),
            });
            let worker =
                ReaderGuard::start(reads.clone(), Arc::new(EchoGuard::default()), tx.clone());
            let (selection, generation) = observe_offer(core, &reads, &tx, Instant::now());
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
                    readers: vec![PayloadReader {
                        mime: "text/plain",
                        reader,
                    }],
                    marker,
                },
                &worker.stop,
            );
            tokio::time::timeout(Duration::from_secs(2), reads.started.notified()).await?;
            // Observation uses Core's actual pause policy while the old source
            // still holds its pipe open. Resume cannot confer authority on B.
            pause(true);
            let (paused, _) = observe_offer(core, &reads, &tx, Instant::now());
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
            let (selection, generation) = observe_offer(core, &reads, &tx, Instant::now());
            let target = selection.target.clone().unwrap();
            rx.borrow_and_update();
            let (reader, writer) = pipe(b"latest")?;
            drop(writer);
            reads.submit(
                ReadJob {
                    selection,
                    generation,
                    readers: vec![PayloadReader {
                        mime: "text/plain",
                        reader,
                    }],
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
            observation: None,
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
                observation: None,
                target: Some(crate::core::ClipboardOffer::test_offer()),
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
                    readers: vec![PayloadReader {
                        mime: "text/plain",
                        reader,
                    }],
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
    #[tokio::test]
    async fn prepared_offer_timeout_preserves_payload_fallback_but_requires_marker() -> Result<()> {
        for blocked_marker in [false, true] {
            let reads = Arc::new(ReadQueue::default());
            let (tx, mut rx) = tokio::sync::watch::channel(Selection {
                observation: None,
                target: None,
                content: None,
                arrived: Instant::now(),
            });
            let worker =
                ReaderGuard::start(reads.clone(), Arc::new(EchoGuard::default()), tx.clone());
            let selection = Selection {
                observation: None,
                target: Some(crate::core::ClipboardOffer::test_offer()),
                content: None,
                arrived: Instant::now() - Duration::from_millis(4900),
            };
            let generation = reads.observe(selection.clone(), &tx);
            rx.borrow_and_update();
            let mut stalled = None;
            let job = prepare_read_job(
                &[
                    "text/plain".into(),
                    "image/png".into(),
                    "x-kde-passwordManagerHint".into(),
                ],
                selection,
                generation,
                |mime| {
                    let bytes: &[u8] = match mime {
                        "image/png" => b"\x89PNG\r\n\x1a\nvalid image",
                        "text/plain" => b"valid text",
                        _ => b"public",
                    };
                    let (reader, writer) = pipe(bytes)?;
                    if (blocked_marker && mime == "x-kde-passwordManagerHint")
                        || (!blocked_marker && mime == "image/png")
                    {
                        stalled = Some(writer);
                    } else {
                        drop(writer);
                    }
                    Ok(reader)
                },
            )?
            .expect("supported offer");
            reads.submit(job, &worker.stop);
            tokio::time::timeout(Duration::from_secs(2), rx.changed()).await??;
            assert_eq!(
                rx.borrow().content,
                (!blocked_marker).then(|| Content::Text("valid text".into()))
            );
        }
        Ok(())
    }
    #[test]
    fn prepared_offer_keeps_text_when_png_fails() -> Result<()> {
        for failure in ["invalid", "oversized", "stalled", "request", "read"] {
            let reads = ReadQueue::default();
            let mut requested = Vec::new();
            let mut stalled = None;
            let result = std::thread::scope(|scope| -> Result<_> {
                let mut job = prepare_read_job(
                    &["text/plain".into(), "image/png".into()],
                    Selection {
                        observation: None,
                        target: Some(crate::core::ClipboardOffer::test_offer()),
                        content: None,
                        arrived: Instant::now(),
                    },
                    0,
                    |mime| {
                        requested.push(mime.to_owned());
                        if mime == "text/plain" {
                            let (reader, writer) = pipe(b"valid text")?;
                            drop(writer);
                            return Ok(reader);
                        }
                        if failure == "request" {
                            anyhow::bail!("injected PNG request failure");
                        }
                        if failure == "read" {
                            return Ok(std::fs::File::open(".")?);
                        }
                        let (reader, mut writer) = UnixStream::pair()?;
                        if failure == "stalled" {
                            writer.write_all(b"\x89PNG\r\n\x1a\npartial")?;
                            stalled = Some(writer);
                        } else {
                            scope.spawn(move || {
                                if failure == "oversized" {
                                    let _ = writer.write_all(&vec![1; MAX_IMAGE + 1]);
                                } else {
                                    let _ = writer.write_all(b"invalid PNG");
                                }
                            });
                        }
                        Ok(std::fs::File::from(OwnedFd::from(reader)))
                    },
                )?
                .expect("supported offer");
                Ok(read_payloads(
                    &mut job.readers,
                    &AtomicBool::new(false),
                    &reads,
                    0,
                    Instant::now() + Duration::from_millis(300),
                ))
            })?;
            assert_eq!(
                result,
                Some(Content::Text("valid text".into())),
                "PNG failure: {failure}; requests: {requested:?}"
            );
            assert_eq!(requested, ["text/plain", "image/png"]);
        }
        Ok(())
    }
    #[test]
    fn prepared_offer_keeps_image_when_text_fails_and_handles_single_formats() -> Result<()> {
        for failure in ["invalid", "oversized", "stalled", "request", "read", "none"] {
            for single in [None, Some("text/plain"), Some("image/png")] {
                if failure != "none" && single.is_some() {
                    continue;
                }
                let mimes: Vec<_> = single.map_or_else(
                    || vec!["text/plain".into(), "image/png".into()],
                    |mime| vec![mime.to_owned()],
                );
                let mut stalled = None;
                let mut job = prepare_read_job(
                    &mimes,
                    Selection {
                        observation: None,
                        target: Some(crate::core::ClipboardOffer::test_offer()),
                        content: None,
                        arrived: Instant::now(),
                    },
                    0,
                    |mime| {
                        if mime == "text/plain" && failure == "request" {
                            anyhow::bail!("injected text request failure");
                        }
                        if mime == "text/plain" && failure == "read" {
                            return Ok(std::fs::File::open(".")?);
                        }
                        let bytes: &[u8] = if mime == "image/png" {
                            b"\x89PNG\r\n\x1a\nvalid image"
                        } else if failure == "invalid" {
                            &[255]
                        } else if failure == "oversized" {
                            &vec![b't'; pneu_proto::MAX_CLIPBOARD_BYTES + 1]
                        } else {
                            b"valid text"
                        };
                        let (reader, writer) = pipe(bytes)?;
                        if mime == "text/plain" && failure == "stalled" {
                            stalled = Some(writer);
                        } else {
                            drop(writer);
                        }
                        Ok(reader)
                    },
                )?
                .expect("supported offer");
                let result = read_payloads(
                    &mut job.readers,
                    &AtomicBool::new(false),
                    &ReadQueue::default(),
                    0,
                    Instant::now() + Duration::from_millis(300),
                );
                let image = b"\x89PNG\r\n\x1a\nvalid image".to_vec();
                let expected = if single == Some("text/plain") {
                    Content::Text("valid text".into())
                } else if single == Some("image/png") || failure != "none" {
                    Content::Image(image)
                } else {
                    Content::Formats("valid text".into(), image)
                };
                assert_eq!(
                    result,
                    Some(expected),
                    "text failure: {failure}; single: {single:?}"
                );
            }
        }
        Ok(())
    }
    #[test]
    fn prepared_offer_reads_text_alias_fallbacks_with_stable_preference() -> Result<()> {
        for failure in ["invalid", "oversized", "stalled", "request", "read", "none"] {
            let mut stalled = None;
            let mut requested = Vec::new();
            let mut job = prepare_read_job(
                &[
                    "text/plain;charset=utf-8".into(),
                    "text/plain".into(),
                    "UTF8_STRING".into(),
                ],
                Selection {
                    observation: None,
                    target: Some(crate::core::ClipboardOffer::test_offer()),
                    content: None,
                    arrived: Instant::now(),
                },
                0,
                |mime| {
                    requested.push(mime.to_owned());
                    let preferred = mime == "text/plain;charset=utf-8";
                    if preferred && failure == "request" {
                        anyhow::bail!("injected preferred alias request failure");
                    }
                    if preferred && failure == "read" {
                        return Ok(std::fs::File::open(".")?);
                    }
                    let bytes: &[u8] = if preferred && failure == "invalid" {
                        &[255]
                    } else if preferred && failure == "oversized" {
                        &vec![b't'; pneu_proto::MAX_CLIPBOARD_BYTES + 1]
                    } else if preferred {
                        b"preferred"
                    } else if mime == "text/plain" {
                        b"fallback"
                    } else {
                        b"last alias"
                    };
                    let (reader, writer) = pipe(bytes)?;
                    if preferred && failure == "stalled" {
                        stalled = Some(writer);
                    } else {
                        drop(writer);
                    }
                    Ok(reader)
                },
            )?
            .expect("supported text offer");
            let result = read_payloads(
                &mut job.readers,
                &AtomicBool::new(false),
                &ReadQueue::default(),
                0,
                Instant::now() + Duration::from_millis(300),
            );
            assert_eq!(
                result,
                Some(Content::Text(
                    if failure == "none" {
                        "preferred"
                    } else {
                        "fallback"
                    }
                    .into()
                )),
                "failure: {failure}"
            );
            assert_eq!(
                requested,
                ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"]
            );
        }
        Ok(())
    }
    #[test]
    fn mixed_reader_bounds_each_format_without_losing_the_other() -> Result<()> {
        for oversized_image in [false, true] {
            let (text_reader, mut text_writer) = UnixStream::pair()?;
            let (image_reader, mut image_writer) = UnixStream::pair()?;
            let reads = ReadQueue::default();
            let stop = AtomicBool::new(false);
            let result = std::thread::scope(|scope| {
                scope.spawn(move || {
                    let text = if oversized_image {
                        b"valid text".to_vec()
                    } else {
                        vec![b't'; pneu_proto::MAX_CLIPBOARD_BYTES + 1]
                    };
                    let _ = text_writer.write_all(&text);
                });
                scope.spawn(move || {
                    if image_writer.write_all(b"\x89PNG\r\n\x1a\n").is_err() {
                        return;
                    }
                    if oversized_image {
                        for _ in 0..=MAX_IMAGE / 32768 {
                            if image_writer.write_all(&[1; 32768]).is_err() {
                                break;
                            }
                        }
                    } else {
                        let _ = image_writer.write_all(b"valid image");
                    }
                });
                read_payloads(
                    &mut vec![
                        PayloadReader {
                            mime: "text/plain",
                            reader: std::fs::File::from(OwnedFd::from(text_reader)),
                        },
                        PayloadReader {
                            mime: "image/png",
                            reader: std::fs::File::from(OwnedFd::from(image_reader)),
                        },
                    ],
                    &stop,
                    &reads,
                    0,
                    Instant::now() + Duration::from_secs(5),
                )
            });
            if oversized_image {
                assert_eq!(result, Some(Content::Text("valid text".into())));
            } else {
                assert_eq!(
                    result,
                    Some(Content::Image(b"\x89PNG\r\n\x1a\nvalid image".to_vec()))
                );
            }
        }
        Ok(())
    }
    #[test]
    fn mixed_reader_does_not_let_a_stalled_format_hide_completed_content() -> Result<()> {
        for stalled_image in [false, true] {
            let (text, text_source) = pipe(b"valid text")?;
            let (image, image_source) = pipe(b"\x89PNG\r\n\x1a\nvalid image")?;
            let _stalled = if stalled_image {
                drop(text_source);
                image_source
            } else {
                drop(image_source);
                text_source
            };
            let result = read_payloads(
                &mut vec![
                    PayloadReader {
                        mime: "text/plain",
                        reader: text,
                    },
                    PayloadReader {
                        mime: "image/png",
                        reader: image,
                    },
                ],
                &AtomicBool::new(false),
                &ReadQueue::default(),
                0,
                Instant::now() + Duration::from_millis(300),
            );
            if stalled_image {
                assert_eq!(result, Some(Content::Text("valid text".into())));
            } else {
                assert_eq!(
                    result,
                    Some(Content::Image(b"\x89PNG\r\n\x1a\nvalid image".to_vec()))
                );
            }
        }
        Ok(())
    }
    #[test]
    fn delayed_selection_delivery_keeps_offer_arrival_deadline() {
        let arrived = Instant::now() - Duration::from_secs(2);
        let mut debounce = Debounce::default();
        let mut selection = Selection {
            observation: None,
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
