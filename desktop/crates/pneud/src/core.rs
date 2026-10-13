use crate::{
    Config, Paths,
    actions::{PcActions, ReceivedFile},
    identity::Identity,
    storage::{Stage, VerifiedFile, cache_basename, safe_basename},
    store::{Metadata, Store},
    tls::{Admission, ClientVerifier, Invitation},
};
use anyhow::{Context, Result, bail, ensure};
use pneu_proto::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering},
    },
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::{Notify, Semaphore, broadcast, mpsc, watch},
    task::JoinSet,
    time::Instant,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    ConnectionChanged {
        device: Id,
        connected: bool,
    },
    PhonePaused {
        device: Id,
        paused: bool,
    },
    Paired {
        device: Id,
    },
    Unpaired {
        device: Id,
    },
    TransferStarted {
        device: Id,
        metadata: FileBegin,
    },
    TransferProgress {
        device: Id,
        transfer_id: Id,
        received: u64,
        total: u64,
    },
    TransferFinished {
        device: Id,
        transfer_id: Id,
        path: Option<PathBuf>,
        deduplicated: bool,
    },
    ClipboardReceived {
        device: Id,
        transfer_id: Id,
        origin: ClipboardOrigin,
    },
    ClipboardSent {
        device: Id,
        transfer_id: Id,
    },
    Error {
        device: Option<Id>,
        message: String,
    },
    ConfigChanged,
    PairingProgress {
        name: Option<String>,
    },
    Blocked {
        address: String,
    },
    RemoteUnpaired {
        name: String,
    },
}
#[derive(Clone, Copy)]
enum CloseReason {
    Error(ErrorCode),
    Unpair,
}
impl CloseReason {
    fn message(self) -> Message {
        match self {
            Self::Error(code) => error_message(code),
            Self::Unpair => Message::Unpair(Unpair {}),
        }
    }
}
#[derive(Clone)]
struct ActionScope {
    cancelled: watch::Sender<bool>,
    active: Arc<tokio::sync::Mutex<()>>,
    // Retained across disconnects; only admitting a newer connection advances it.
    connection_generation: Arc<AtomicU64>,
}
impl ActionScope {
    fn new() -> Self {
        Self {
            cancelled: watch::channel(false).0,
            active: Arc::new(tokio::sync::Mutex::new(())),
            connection_generation: Arc::new(AtomicU64::new(0)),
        }
    }
    fn allows(&self, origin: &ActionOrigin) -> bool {
        !*self.cancelled.borrow()
            && origin
                .connection_id
                .is_none_or(|id| id == self.connection_generation.load(Ordering::SeqCst))
    }
    /// Check at the side-effect boundary, after all preceding awaits. The
    /// closure is never invoked for revoked work; cancellation also drops an
    /// in-flight effect (effects already submitted to the OS cannot be undone).
    async fn effect<F, Fut, T>(&self, origin: &ActionOrigin, effect: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let mut cancelled = self.cancelled.subscribe();
        if *cancelled.borrow() {
            return None;
        }
        let _active = tokio::select! { biased;
            _ = cancelled.changed() => return None,
            guard = self.active.lock() => guard,
        };
        if *cancelled.borrow() {
            return None;
        }
        tokio::select! { biased;
            _ = cancelled.changed() => None,
            result = async {
                // A reconnect shares the pairing scope. Check its independent
                // connection authority here, after acquiring the action mutex.
                if !self.allows(origin) {
                    return None;
                }
                Some(effect().await)
            } => result,
        }
    }
}
#[derive(Clone)]
struct ActionOrigin {
    device: Id,
    connection_id: Option<u64>,
}
impl ActionOrigin {
    fn captured(store: &Store, device: &Id) -> Self {
        Self {
            device: device.clone(),
            connection_id: store.connection_id,
        }
    }
}
#[derive(Clone)]
struct ActionJob {
    file: ReceivedFile,
    scope: ActionScope,
    origin: ActionOrigin,
    path: PathBuf,
    metadata: FileBegin,
    config: Config,
    arrived: Instant,
    save_error: Option<NackCode>,
}
impl ActionJob {
    fn saved(&self) -> bool {
        self.metadata.kind != FileKind::ClipboardImage
            && self.config.save_to_folder
            && self.save_error.is_none()
    }
}
// Receipt notices have no file descriptor and outlive their connection worker.
// They retain the same pairing/replacement authority as completed-file actions.
enum ReceiptNotice {
    Text(String),
    TextFailed,
    SaveFailed {
        metadata: FileBegin,
        code: NackCode,
        destination: crate::ui::ReceiveDestination,
        folder: PathBuf,
    },
}
struct ReceiptJob {
    scope: ActionScope,
    origin: ActionOrigin,
    notice: ReceiptNotice,
}
enum NotificationJob {
    File(ActionJob, DesktopAction),
    Receipt(ReceiptJob),
}
impl NotificationJob {
    fn allows(&self) -> bool {
        match self {
            Self::File(job, _) => job.scope.allows(&job.origin),
            Self::Receipt(job) => job.scope.allows(&job.origin),
        }
    }
}
const NOTIFICATION_LIMIT: usize = 16;
const EXPLICIT_IMAGE_LIMIT: usize = 8;
#[derive(Default)]
struct PendingActions {
    automatic: Option<ActionJob>,
    explicit: VecDeque<ActionJob>,
    notifications: VecDeque<NotificationJob>,
}
#[derive(Default)]
struct ActionQueue {
    pending: Mutex<PendingActions>,
    changed: Notify,
}
impl ActionQueue {
    /// False means the explicit request was rejected, with a best-effort
    /// failure notification queued instead. Never wait for desktop capacity.
    fn push(&self, job: ActionJob) -> bool {
        let mut pending = self.pending.lock().expect("pending actions");
        if !job.scope.allows(&job.origin) {
            return true;
        }
        let mut accepted = true;
        if job.metadata.kind == FileKind::ClipboardImage {
            if pending.explicit.len() == EXPLICIT_IMAGE_LIMIT {
                Self::notice(&mut pending, job, DesktopAction::CopyFailed);
                accepted = false;
            } else {
                pending.explicit.push_back(job);
            }
        } else {
            if job.config.notification_preview || job.save_error.is_some() {
                Self::notice(&mut pending, job.clone(), DesktopAction::Notification);
            }
            if supported_image_mime(&job.metadata.mime)
                && (job.config.copy_image_to_clipboard || job.config.open_in_viewer)
            {
                pending.automatic = Some(job);
            }
        }
        drop(pending);
        self.changed.notify_one();
        accepted
    }
    fn notice(pending: &mut PendingActions, job: ActionJob, action: DesktopAction) {
        Self::push_notice(pending, NotificationJob::File(job, action));
    }
    fn push_notice(pending: &mut PendingActions, job: NotificationJob) {
        if pending.notifications.len() == NOTIFICATION_LIMIT {
            pending.notifications.pop_front();
        }
        pending.notifications.push_back(job);
    }
    fn receipt(&self, scope: &ActionScope, origin: ActionOrigin, notice: ReceiptNotice) {
        let mut pending = self.pending.lock().expect("pending actions");
        if !scope.allows(&origin) {
            return;
        }
        Self::push_notice(
            &mut pending,
            NotificationJob::Receipt(ReceiptJob {
                scope: scope.clone(),
                origin,
                notice,
            }),
        );
        drop(pending);
        self.changed.notify_one();
    }
    fn discard_invalid(&self) {
        let mut pending = self.pending.lock().expect("pending actions");
        if pending
            .automatic
            .as_ref()
            .is_some_and(|j| !j.scope.allows(&j.origin))
        {
            pending.automatic = None;
        }
        pending.explicit.retain(|j| j.scope.allows(&j.origin));
        pending.notifications.retain(NotificationJob::allows);
        drop(pending);
        self.changed.notify_one();
    }
}
#[derive(Clone, Copy)]
enum DesktopAction {
    Automatic,
    Explicit,
    Notification,
    CopyFailed,
    Viewer,
}
fn supported_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
    )
}
fn persist_phone_generation(
    ui: &Mutex<crate::ui::State>,
    path: &std::path::Path,
    generation: u64,
) -> Result<()> {
    let ui = ui.lock().expect("UI state");
    if ui.phone_generation == Some(generation) {
        crate::config::atomic_private(path, &serde_json::to_vec(&ui.snapshot.phone)?)?;
    }
    Ok(())
}
fn update_ui_state(ui: &Mutex<crate::ui::State>, event: &Event) {
    {
        let mut ui = ui.lock().expect("UI state");
        match event {
            Event::ConnectionChanged { connected, .. } => {
                ui.snapshot.connected = *connected;
                ui.snapshot.phone.last_seen = crate::now_ms();
                ui.clear_transfers();
            }
            Event::PhonePaused { paused, .. } => ui.snapshot.paused = *paused,
            Event::PairingProgress { name } => ui.snapshot.pairing = name.clone(),
            Event::TransferStarted { metadata, .. } => {
                ui.active_transfers.insert(metadata.transfer_id.clone());
                ui.receiving_until = Some(std::time::Instant::now() + Duration::from_millis(1500));
            }
            Event::TransferFinished { transfer_id, .. } => {
                ui.active_transfers.remove(transfer_id);
            }
            Event::Unpaired { .. } | Event::Paired { .. } => {
                // Metadata from a prior pairing cannot label the next identity,
                // even while its HELLO/publication is still pending.
                ui.snapshot.phone = crate::ui::Phone::default();
                ui.snapshot.connected = false;
                ui.snapshot.paused = false;
                ui.phone_generation = None;
                ui.clear_transfers();
            }
            _ => {}
        }
    }
}

fn emit_event(ui: &Mutex<crate::ui::State>, events: &broadcast::Sender<Event>, event: Event) {
    update_ui_state(ui, &event);
    let _ = events.send(event);
}

enum OutgoingSelection {
    Text(Clipboard),
    Image(Id, Vec<u8>),
}
impl OutgoingSelection {
    fn id(&self) -> &Id {
        match self {
            Self::Text(text) => &text.transfer_id,
            Self::Image(id, _) => id,
        }
    }
}
struct AutomaticWrite {
    target: ClipboardTarget,
    selection: OutgoingSelection,
}
enum WriterWork {
    Frame(Message),
    Automatic(AutomaticWrite),
}
/// Authority is checked at the first accepted byte, including after a socket
/// returned Pending. Once a frame starts, keep completing the in-flight operation.
struct OfferWriter<'a, W> {
    core: &'a Core,
    writer: &'a mut W,
    target: ClipboardTarget,
    started: bool,
    canceled: bool,
}
impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for OfferWriter<'_, W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = &mut *self;
        if this.started {
            return std::pin::Pin::new(&mut *this.writer).poll_write(cx, bytes);
        }
        let sessions = this.core.sessions.lock().expect("sessions");
        let automatic = this.core.automatic.lock().expect("automatic clipboard");
        if this
            .core
            .automatic_target_locked(&sessions, &automatic)
            .as_ref()
            != Some(&this.target)
        {
            this.canceled = true;
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "clipboard offer superseded",
            )));
        }
        let result = std::pin::Pin::new(&mut *this.writer).poll_write(cx, bytes);
        if matches!(result, std::task::Poll::Ready(Ok(n)) if n > 0) {
            this.started = true;
        }
        result
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.writer).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.writer).poll_shutdown(cx)
    }
}
async fn send_pending_messages<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    priority: &mut mpsc::Receiver<Message>,
    normal: &mut mpsc::Receiver<Message>,
    reports: Option<&mpsc::Sender<WriteReport>>,
) -> Result<()> {
    // Bound each drain so incoming traffic cannot indefinitely stall an image.
    for _ in 0..8 {
        let Ok(message) = priority.try_recv() else {
            break;
        };
        send(writer, &message).await?;
        if let Some(reports) = reports {
            report_frame(reports, &message).await;
        }
    }
    if let Ok(message) = normal.try_recv() {
        send(writer, &message).await?;
        if let Some(reports) = reports {
            report_frame(reports, &message).await;
        }
    }
    Ok(())
}
async fn transmit_automatic<W: tokio::io::AsyncWrite + Unpin>(
    core: &Core,
    writer: &mut W,
    work: AutomaticWrite,
    priority: &mut mpsc::Receiver<Message>,
    normal: &mut mpsc::Receiver<Message>,
    reports: Option<&mpsc::Sender<WriteReport>>,
) -> Result<bool> {
    let mut writer = OfferWriter {
        core,
        writer,
        target: work.target,
        started: false,
        canceled: false,
    };
    let result = async {
        match work.selection {
            OutgoingSelection::Text(text) => send(&mut writer, &Message::Clipboard(text)).await?,
            OutgoingSelection::Image(id, bytes) => {
                let begin = FileBegin {
                    transfer_id: id.clone(),
                    file_name: "clipboard.png".into(),
                    mime: "image/png".into(),
                    size: bytes.len() as u64,
                    sha256: Hash::new(hex::encode(Sha256::digest(&bytes)))?,
                    kind: FileKind::ClipboardImage,
                    created_at: crate::now_ms(),
                    source_media_id: None,
                };
                send(&mut writer, &Message::FileBegin(begin)).await?;
                for (index, chunk) in bytes.chunks(CHUNK_SIZE).enumerate() {
                    // Receipts and heartbeats stay prompt between image frames.
                    send_pending_messages(&mut *writer.writer, priority, normal, reports).await?;
                    send(
                        &mut writer,
                        &Message::FileChunk(FileChunk {
                            transfer_id: id.clone(),
                            offset: (index * CHUNK_SIZE) as u64,
                            data: chunk.to_vec(),
                        }),
                    )
                    .await?;
                }
                send_pending_messages(&mut *writer.writer, priority, normal, reports).await?;
                send(&mut writer, &Message::FileEnd(FileEnd { transfer_id: id })).await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if !writer.started
        && (writer.canceled || core.automatic_target().as_ref() != Some(&writer.target))
    {
        return Ok(false);
    }
    result?;
    Ok(true)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardTarget {
    pub device: Id,
    pub connection: u64,
    pub generation: u64,
}
#[derive(Default)]
struct AutomaticClipboard {
    generation: u64,
    selection: Option<(ClipboardTarget, OutgoingSelection)>,
}
impl AutomaticClipboard {
    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.selection = None;
    }
}
struct ReadyState {
    store: Store,
    allow_recovery: bool,
    action_scope: Option<ActionScope>,
}
struct Session {
    device: Id,
    id: u64,
    close: watch::Sender<Option<CloseReason>>,
    wake: Arc<Notify>,
    paused: bool,
}
#[derive(Default)]
struct Connections {
    session: Option<Session>,
    unpairing: bool,
}
/// Clone the Arc<Core> into a tray/window. Commands below are async; subscribe()
/// returns a bounded broadcast stream (lagging UI consumers must resnapshot).
pub struct Core {
    pub identity: Identity,
    pub paths: Paths,
    ui: Arc<Mutex<crate::ui::State>>,
    peer_capabilities: Mutex<Vec<String>>,
    automatic: Arc<Mutex<AutomaticClipboard>>,
    config: watch::Sender<Config>,
    events: broadcast::Sender<Event>,
    store: Store,
    admission: Arc<Mutex<Admission>>,
    invitation_changes: watch::Sender<u64>,
    provisional: Arc<AtomicBool>,
    pairing_gate: Arc<tokio::sync::Mutex<()>>,
    config_gate: Arc<tokio::sync::Mutex<()>>,
    clipboard_gate: Arc<tokio::sync::Mutex<()>>,
    outgoing_gate: tokio::sync::Mutex<()>,
    next_connection: AtomicU64,
    sessions: Arc<Mutex<Connections>>,
    actions: Arc<dyn PcActions>,
    action_queue: Arc<ActionQueue>,
    action_scope: Arc<Mutex<Option<(Id, ActionScope)>>>,
    action_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    action_stop: watch::Sender<bool>,
    shutdown: watch::Sender<bool>,
    port: AtomicU16,
    window_request: Mutex<(u64, Option<String>)>,
    #[cfg(test)]
    file_write_delay: Mutex<Duration>,
}
impl Core {
    pub async fn new(
        paths: Paths,
        config: Config,
        actions: Arc<dyn PcActions>,
    ) -> Result<Arc<Self>> {
        config.validate_with_cache(&paths.cache)?;
        paths.prepare()?;
        let store = Store::open(&paths.data.join("state.sqlite3"))?;
        let identity = Identity::load_or_create(&paths.data)?;
        let admission = Arc::new(Mutex::new(Admission {
            peers: store.peers().await?,
            ..Default::default()
        }));
        match store
            .clean_staging(vec![config.save_path.clone(), paths.cache.clone()])
            .await
        {
            Ok(failures) => {
                for (path, error) in failures {
                    eprintln!(
                        "pneud: staging cleanup deferred for {}: {error}",
                        path.display()
                    );
                }
            }
            Err(error) => eprintln!("pneud: staging cleanup deferred: {error}"),
        }
        let (events, _) = broadcast::channel(128);
        let port = config.port;
        let (config, _) = watch::channel(config);
        let (shutdown, _) = watch::channel(false);
        let (action_stop, _) = watch::channel(false);
        let (invitation_changes, _) = watch::channel(0);
        let action_queue = Arc::new(ActionQueue::default());
        let action_scope = admission
            .lock()
            .expect("admission")
            .peers
            .values()
            .next()
            .map(|id| (id.clone(), ActionScope::new()));
        let phone = std::fs::read(paths.data.join("phone.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let core = Arc::new(Self {
            peer_capabilities: Mutex::new(Vec::new()),
            automatic: Arc::new(Mutex::new(AutomaticClipboard::default())),
            ui: Arc::new(Mutex::new(crate::ui::State {
                snapshot: crate::ui::Snapshot {
                    phone,
                    ..Default::default()
                },
                ..Default::default()
            })),
            identity,
            paths,
            config,
            events,
            store,
            admission,
            invitation_changes,
            provisional: Arc::new(AtomicBool::new(false)),
            pairing_gate: Arc::new(tokio::sync::Mutex::new(())),
            config_gate: Arc::new(tokio::sync::Mutex::new(())),
            sessions: Arc::new(Mutex::new(Connections::default())),
            clipboard_gate: Arc::new(tokio::sync::Mutex::new(())),
            outgoing_gate: tokio::sync::Mutex::new(()),
            next_connection: AtomicU64::new(1),
            actions,
            action_queue: action_queue.clone(),
            action_scope: Arc::new(Mutex::new(action_scope)),
            action_worker: Mutex::new(None),
            action_stop,
            shutdown,
            port: AtomicU16::new(port),
            window_request: Mutex::new((0, None)),
            #[cfg(test)]
            file_write_delay: Mutex::new(Duration::ZERO),
        });
        let weak = Arc::downgrade(&core);
        let stop = core.action_stop.subscribe();
        *core.action_worker.lock().expect("actions") = Some(tokio::spawn(Box::pin(action_loop(
            weak,
            action_queue,
            stop,
        ))));
        Ok(core)
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
    pub fn config(&self) -> Config {
        self.config.borrow().clone()
    }
    pub fn paired_devices(&self) -> Vec<Id> {
        self.admission
            .lock()
            .expect("admission")
            .peers
            .values()
            .cloned()
            .collect()
    }
    pub fn connected_devices(&self) -> Vec<Id> {
        self.sessions
            .lock()
            .expect("sessions")
            .session
            .as_ref()
            .filter(|s| !s.close.is_closed() && s.close.borrow().is_none())
            .map(|s| vec![s.device.clone()])
            .unwrap_or_default()
    }
    pub fn phone_paused(&self, device: &Id) -> Option<bool> {
        self.sessions
            .lock()
            .expect("sessions")
            .session
            .as_ref()
            .filter(|s| &s.device == device && !s.close.is_closed() && s.close.borrow().is_none())
            .map(|s| s.paused)
    }
    fn set_phone_paused(&self, device: &Id, id: u64, paused: bool) {
        let mut state = self.sessions.lock().expect("sessions");
        if let Some(s) = state
            .session
            .as_mut()
            .filter(|s| s.id == id && &s.device == device)
        {
            let changed = s.paused != paused;
            s.paused = paused;
            if changed {
                self.automatic
                    .lock()
                    .expect("automatic clipboard")
                    .invalidate();
            }
            self.event(Event::PhonePaused {
                device: device.clone(),
                paused,
            });
        }
    }
    fn connection_event(&self, store: &Store, device: &Id, event: Event) {
        let state = self.sessions.lock().expect("sessions");
        if store.connection_id.is_none()
            || state
                .session
                .as_ref()
                .is_some_and(|s| &s.device == device && Some(s.id) == store.connection_id)
        {
            self.event(event);
        }
    }
    fn finish_connection(&self, device: &Id, id: u64) -> bool {
        let mut state = self.sessions.lock().expect("sessions");
        if state
            .session
            .as_ref()
            .is_some_and(|s| s.id == id && &s.device == device)
        {
            state.session = None;
            self.automatic
                .lock()
                .expect("automatic clipboard")
                .invalidate();
            self.event(Event::ConnectionChanged {
                device: device.clone(),
                connected: false,
            });
            true
        } else {
            false
        }
    }
    fn connection_current(&self, store: &Store, device: &Id) -> bool {
        self.connection_id_current(store.connection_id, device)
    }
    fn connection_id_current(&self, connection_id: Option<u64>, device: &Id) -> bool {
        let state = self.sessions.lock().expect("sessions");
        connection_id.is_none()
            || state.session.as_ref().is_some_and(|s| {
                &s.device == device && Some(s.id) == connection_id && s.close.borrow().is_none()
            })
    }
    pub fn bound_port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }
    pub fn request_window(&self, choose_folder: bool) {
        self.request_window_with_activation(choose_folder, None);
    }
    pub(crate) fn request_window_with_activation(
        &self,
        choose_folder: bool,
        token: Option<String>,
    ) {
        let mut request = self.window_request.lock().expect("window request");
        request.0 = ((request.0 >> 1) + 1) * 2 + u64::from(choose_folder);
        request.1 = token;
    }
    pub fn window_request(&self) -> u64 {
        self.window_request.lock().expect("window request").0
    }
    pub(crate) fn window_request_with_activation(&self) -> (u64, Option<String>) {
        self.window_request.lock().expect("window request").clone()
    }
    pub async fn close_pairing_if(&self, token: Hash) {
        let _guard = self.pairing_gate.lock().await;
        let mut admission = self.admission.lock().expect("admission");
        if admission
            .invitation
            .as_ref()
            .is_some_and(|i| i.token == token)
        {
            admission.invitation = None;
            self.invitation_changes
                .send_modify(|r| *r = r.wrapping_add(1));
        }
    }
    /// Owner-only IPC supplies the URI; broadcast events never expose its secret.
    pub fn pairing_status(&self) -> (u64, Option<PairingUri>) {
        let state = self.admission.lock().expect("admission");
        (
            *self.invitation_changes.borrow(),
            state
                .invitation
                .as_ref()
                .filter(|i| i.is_active())
                .map(|i| i.uri.clone()),
        )
    }
    pub async fn wait_pairing_change(&self, revision: u64) -> u64 {
        let mut changes = self.invitation_changes.subscribe();
        if *changes.borrow() == revision {
            tokio::select! { _=self.wait_shutdown()=>{}, _=tokio::time::timeout(Duration::from_secs(15), changes.changed())=>{} }
        }
        *changes.borrow()
    }
    /// Take actionable identity, display metadata and status in the writers'
    /// lock order. No UI consumer may recombine this with a later identity read.
    pub fn ui_snapshot(&self) -> crate::ui::Snapshot {
        let sessions = self.sessions.lock().expect("sessions");
        let admission = self.admission.lock().expect("admission");
        let ui = self.ui.lock().expect("UI state");
        let mut snapshot = ui.snapshot.clone();
        snapshot.device = admission.peers.values().next().cloned();
        snapshot.paired = snapshot.device.is_some();
        if !snapshot.paired || snapshot.phone.device != snapshot.device {
            // Older caches without identity are untrusted presentation data.
            // A generic label is safer than attributing the previous phone's name.
            snapshot.phone = crate::ui::Phone {
                device: snapshot.device.clone(),
                name: if snapshot.paired {
                    "Phone".into()
                } else {
                    String::new()
                },
                ..Default::default()
            };
            snapshot.generation = None;
        } else {
            snapshot.generation = ui.phone_generation;
        }
        let session = sessions.session.as_ref().filter(|s| {
            snapshot.device.as_ref() == Some(&s.device)
                && !s.close.is_closed()
                && s.close.borrow().is_none()
        });
        snapshot.connected = session.is_some();
        snapshot.paused = session.is_some_and(|s| s.paused);
        snapshot.receiving = snapshot.connected
            && (!ui.active_transfers.is_empty()
                || ui
                    .receiving_until
                    .is_some_and(|t| t > std::time::Instant::now()));
        snapshot
    }
    pub fn set_receive_error(&self, error: Option<crate::ui::ReceiveError>) {
        self.ui.lock().expect("UI state").snapshot.error = error;
        let _ = self.events.send(Event::ConfigChanged);
    }
    pub fn clear_port_error(&self) {
        self.clear_receive_error(|error| matches!(error, crate::ui::ReceiveError::Port(_)));
    }
    fn connection_storage_result(
        &self,
        store: &Store,
        device: &Id,
        destination: crate::ui::ReceiveDestination,
        path: &std::path::Path,
        code: Option<NackCode>,
    ) {
        let sessions = self.sessions.lock().expect("sessions");
        if store.connection_id.is_some()
            && sessions
                .session
                .as_ref()
                .is_none_or(|s| &s.device != device || Some(s.id) != store.connection_id)
        {
            return;
        }
        let mut ui = self.ui.lock().expect("UI state");
        let config = self.config();
        if destination == crate::ui::ReceiveDestination::SaveFolder
            && (!config.save_to_folder || config.save_path != path)
        {
            return;
        }
        ui.snapshot.storage_result(destination, path, code);
        drop(ui);
        let _ = self.events.send(Event::ConfigChanged);
    }
    fn finish_transfer_activity(&self, store: &Store, device: &Id, id: &Id) {
        let sessions = self.sessions.lock().expect("sessions");
        if store.connection_id.is_none()
            || sessions
                .session
                .as_ref()
                .is_some_and(|s| &s.device == device && Some(s.id) == store.connection_id)
        {
            self.ui
                .lock()
                .expect("UI state")
                .active_transfers
                .remove(id);
        }
    }
    fn clear_receive_error(&self, matches: impl FnOnce(&crate::ui::ReceiveError) -> bool) {
        let mut ui = self.ui.lock().expect("UI state");
        if ui.snapshot.error.as_ref().is_some_and(matches) {
            ui.snapshot.error = None;
            drop(ui);
            let _ = self.events.send(Event::ConfigChanged);
        }
    }
    fn event(&self, event: Event) {
        emit_event(&self.ui, &self.events, event);
    }
    fn error(&self, device: Option<Id>, message: impl Into<String>) {
        self.event(Event::Error {
            device,
            message: message.into(),
        });
    }
    pub async fn start_pairing(&self) -> Result<PairingUri> {
        let addresses: Vec<String> = crate::discovery::pairing_candidates()?
            .into_iter()
            .map(|i| SocketAddr::new(i.ip, self.bound_port()).to_string())
            .collect();
        ensure!(!addresses.is_empty(), "no reachable interface for pairing");
        self.start_pairing_with_addresses(addresses).await
    }
    pub async fn start_pairing_with_addresses(&self, addresses: Vec<String>) -> Result<PairingUri> {
        let _guard = self.pairing_gate.lock().await;
        self.create_pairing(addresses)
    }
    pub async fn ensure_pairing(&self) -> Result<PairingUri> {
        let _guard = self.pairing_gate.lock().await;
        if let Some(uri) = self.pairing_status().1 {
            return Ok(uri);
        }
        let addresses = crate::discovery::pairing_candidates()?
            .into_iter()
            .map(|i| SocketAddr::new(i.ip, self.bound_port()).to_string())
            .collect();
        self.create_pairing(addresses)
    }
    fn create_pairing(&self, addresses: Vec<String>) -> Result<PairingUri> {
        ensure!(!addresses.is_empty(), "no reachable interface for pairing");
        let uri = PairingUri {
            version: 1,
            name: self.config().name,
            fingerprint: self.identity.fingerprint.clone(),
            addresses,
            token: Hash::new(crate::random_hex(32))?,
            expires_at: crate::now_ms() + 300_000,
        };
        uri.validate()?;
        let mut state = self.admission.lock().expect("admission");
        state.invitation = Some(Invitation {
            uri: uri.clone(),
            token: uri.token.clone(),
            expires_at: uri.expires_at,
            deadline: std::time::Instant::now() + Duration::from_secs(300),
        });
        self.invitation_changes
            .send_modify(|r| *r = r.wrapping_add(1));
        Ok(uri)
    }
    pub async fn close_pairing(&self) {
        let _guard = self.pairing_gate.lock().await;
        let mut state = self.admission.lock().expect("admission");
        if state.invitation.take().is_some() {
            self.invitation_changes
                .send_modify(|r| *r = r.wrapping_add(1));
        }
    }
    pub async fn unpair_device(&self, device: Id) -> Result<()> {
        self.forget_pairing(device, CloseReason::Unpair, None, None)
            .await
            .map(|_| ())
    }
    async fn forget_pairing(
        &self,
        device: Id,
        reason: CloseReason,
        expected: Option<u64>,
        remote_name: Option<String>,
    ) -> Result<bool> {
        let guard = self.pairing_gate.clone().lock_owned().await;
        // A stale local command for the phone replaced by a new pairing must
        // not mark the current phone unpairing or disturb its connection.
        if !self.paired_devices().contains(&device) {
            return Ok(false);
        }
        {
            let mut state = self.sessions.lock().expect("sessions");
            if expected.is_some()
                && state
                    .session
                    .as_ref()
                    .is_none_or(|s| Some(s.id) != expected || s.device != device)
            {
                return Ok(false);
            }
            state.unpairing = true;
        }
        let scope = self
            .action_scope
            .lock()
            .expect("actions")
            .as_ref()
            .filter(|(id, _)| *id == device)
            .map(|(_, scope)| scope.clone());
        if let Some(scope) = &scope {
            scope.cancelled.send_replace(true);
            self.action_queue.discard_invalid();
        }
        let admission = self.admission.clone();
        let sessions = self.sessions.clone();
        let events = self.events.clone();
        let actions = self.action_scope.clone();
        let ui = self.ui.clone();
        let automatic = self.automatic.clone();
        let phone_path = self.paths.data.join("phone.json");
        self.store
            .unpair_then(device.clone(), move |persisted| {
                let _guard = guard;
                let mut scope = actions.lock().expect("actions");
                if persisted {
                    if scope.as_ref().is_some_and(|(id, _)| *id == device) {
                        scope.take();
                    }
                    admission
                        .lock()
                        .expect("admission")
                        .peers
                        .retain(|_, id| *id != device);
                } else {
                    // Old work remains cancelled; restored trust gets a fresh
                    // action scope and can reconnect once storage recovers.
                    *scope = Some((device.clone(), ActionScope::new()));
                }
                drop(scope);
                let mut state = sessions.lock().expect("sessions");
                let ended = state.session.take().filter(|s| s.device == device);
                if let Some(session) = &ended {
                    session.close.send_replace(Some(if persisted {
                        reason
                    } else {
                        CloseReason::Error(ErrorCode::Closing)
                    }));
                }
                state.unpairing = false;
                automatic.lock().expect("automatic clipboard").invalidate();
                let disconnected = Event::ConnectionChanged {
                    device: device.clone(),
                    connected: false,
                };
                // Capture/persist the ended timestamp before Unpaired invalidates
                // metadata authority, while preserving public Unpaired -> disconnected order.
                update_ui_state(&ui, &disconnected);
                if let Some(session) = ended
                    && let Err(error) = persist_phone_generation(&ui, &phone_path, session.id)
                {
                    let _ = events.send(Event::Error {
                        device: Some(device.clone()),
                        message: format!("last-seen metadata unavailable: {error}"),
                    });
                }
                if persisted {
                    emit_event(
                        &ui,
                        &events,
                        Event::Unpaired {
                            device: device.clone(),
                        },
                    );
                    if let Some(name) = remote_name {
                        emit_event(&ui, &events, Event::RemoteUnpaired { name });
                    }
                } else {
                    let _ = events.send(Event::Error {
                        device: Some(device.clone()),
                        message: "unpair persistence failed; pairing retained".into(),
                    });
                }
                let _ = events.send(disconnected);
            })
            .await?;
        if let Some(scope) = scope {
            let _idle = scope.active.lock().await;
        }
        Ok(true)
    }
    pub async fn update_config(&self, config: Config) -> Result<()> {
        let guard = self.config_gate.clone().lock_owned().await;
        self.persist_config(config, guard).await
    }
    pub async fn change_save_folder(&self, folder: PathBuf) -> Result<()> {
        let guard = self.config_gate.clone().lock_owned().await;
        let mut config = self.config();
        config.save_path = folder;
        self.persist_config(config, guard).await
    }
    pub async fn toggle_setting(&self, toggle: crate::ui::Toggle) -> Result<()> {
        let guard = self.config_gate.clone().lock_owned().await;
        let mut config = self.config();
        if !toggle.enabled(&config) {
            return Ok(());
        }
        toggle.flip(&mut config);
        self.persist_config(config, guard).await
    }
    async fn persist_config(
        &self,
        config: Config,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<()> {
        config.validate_with_cache(&self.paths.cache)?;
        ensure!(
            config.port == self.config().port,
            "changing the port takes effect after restarting pneud"
        );
        let path = self.paths.config.clone();
        let clear_error = config.save_path != self.config().save_path || !config.save_to_folder;
        let cancel_automatic = config.clipboard_sync_to_phone
            != self.config().clipboard_sync_to_phone
            || config.clipboard_include_images != self.config().clipboard_include_images;
        let automatic = self.automatic.clone();
        let ui = self.ui.clone();
        let settings = self.config.clone();
        let events = self.events.clone();
        self.store
            .blocking(move |_| {
                let _guard = guard;
                config.save(&path)?;
                let mut automatic = automatic.lock().expect("automatic clipboard");
                if cancel_automatic {
                    automatic.invalidate();
                }
                settings.send_replace(config);
                drop(automatic);
                if clear_error {
                    let mut ui = ui.lock().expect("UI state");
                    ui.snapshot
                        .clear_destination(crate::ui::ReceiveDestination::SaveFolder, None);
                }
                let _ = events.send(Event::ConfigChanged);
                Ok(())
            })
            .await
    }
    pub async fn send_clipboard(&self, device: Id, text: String) -> Result<Id> {
        let _clipboard = self.outgoing_gate.lock().await;
        let store = {
            let _guard = self.pairing_gate.lock().await;
            ensure!(
                self.config().clipboard_sync_to_phone,
                "clipboard sync to phone is disabled"
            );
            ensure!(
                self.paired_devices().contains(&device),
                "device is not paired"
            );
            ensure!(
                self.phone_paused(&device) == Some(false),
                "phone is disconnected or paused"
            );
            self.store.with_current_pairing()
        };
        // Clear only this admitted phone's automatic work before preparation;
        // a replacement/new offer during the await must keep its own selection.
        self.cancel_automatic_clipboard(&device);
        // The store assigns the timestamp and replaces the pending value in
        // one transaction, so concurrent IPC jobs cannot reverse their order.
        let text = store.enqueue_clipboard(device.clone(), text).await?;
        self.wake(&device);
        Ok(text.transfer_id)
    }
    pub fn clipboard_target(&self) -> Option<(Id, u64)> {
        if !self.config().clipboard_sync_to_phone {
            return None;
        }
        self.sessions
            .lock()
            .expect("sessions")
            .session
            .as_ref()
            .filter(|s| !s.paused && !s.close.is_closed() && s.close.borrow().is_none())
            .map(|s| (s.device.clone(), s.id))
    }
    pub fn clipboard_image_capable(&self) -> bool {
        self.peer_capabilities
            .lock()
            .expect("capabilities")
            .iter()
            .any(|s| s == "clipboard_image")
    }
    fn automatic_target_locked(
        &self,
        sessions: &Connections,
        automatic: &AutomaticClipboard,
    ) -> Option<ClipboardTarget> {
        if sessions.unpairing || !self.config().clipboard_sync_to_phone {
            return None;
        }
        sessions
            .session
            .as_ref()
            .filter(|s| !s.paused && !s.close.is_closed() && s.close.borrow().is_none())
            .map(|s| ClipboardTarget {
                device: s.device.clone(),
                connection: s.id,
                generation: automatic.generation,
            })
    }
    pub fn automatic_target(&self) -> Option<ClipboardTarget> {
        let sessions = self.sessions.lock().expect("sessions");
        let automatic = self.automatic.lock().expect("automatic clipboard");
        self.automatic_target_locked(&sessions, &automatic)
    }
    /// Every offer, including clear/secret/unsupported, invalidates prepared work.
    pub fn observe_clipboard_offer(&self) -> Option<ClipboardTarget> {
        let sessions = self.sessions.lock().expect("sessions");
        let mut automatic = self.automatic.lock().expect("automatic clipboard");
        automatic.invalidate();
        self.automatic_target_locked(&sessions, &automatic)
    }
    pub fn cancel_automatic_clipboard(&self, device: &Id) {
        let sessions = self.sessions.lock().expect("sessions");
        if sessions
            .session
            .as_ref()
            .is_some_and(|s| &s.device == device)
        {
            self.automatic
                .lock()
                .expect("automatic clipboard")
                .invalidate();
        }
    }
    fn take_automatic(&self, device: &Id, connection: u64) -> Option<AutomaticWrite> {
        let sessions = self.sessions.lock().expect("sessions");
        let mut automatic = self.automatic.lock().expect("automatic clipboard");
        let current = self.automatic_target_locked(&sessions, &automatic)?;
        if &current.device != device || current.connection != connection {
            return None;
        }
        if automatic
            .selection
            .as_ref()
            .is_none_or(|(target, _)| target != &current)
        {
            return None;
        }
        automatic
            .selection
            .take()
            .map(|(target, selection)| AutomaticWrite { target, selection })
    }
    fn publish_automatic(
        &self,
        target: ClipboardTarget,
        selection: OutgoingSelection,
    ) -> Result<()> {
        let sessions = self.sessions.lock().expect("sessions");
        let mut automatic = self.automatic.lock().expect("automatic clipboard");
        ensure!(
            self.automatic_target_locked(&sessions, &automatic).as_ref() == Some(&target),
            "clipboard offer or policy changed"
        );
        automatic.selection = Some((target.clone(), selection));
        drop(automatic);
        drop(sessions);
        self.wake(&target.device);
        Ok(())
    }
    pub async fn send_automatic_clipboard(
        &self,
        device: Id,
        connection: u64,
        text: String,
    ) -> Result<Id> {
        let target = self
            .automatic_target()
            .context("phone is disconnected or paused")?;
        ensure!(
            target.device == device && target.connection == connection,
            "connection changed"
        );
        self.send_observed_clipboard(target, text).await
    }
    pub async fn send_observed_clipboard(
        &self,
        target: ClipboardTarget,
        text: String,
    ) -> Result<Id> {
        let _clipboard = self.outgoing_gate.lock().await;
        ensure!(
            self.automatic_target().as_ref() == Some(&target),
            "clipboard offer or policy changed"
        );
        let text = self
            .store
            .with_current_pairing()
            .ephemeral_clipboard(target.device.clone(), text)
            .await?;
        let id = text.transfer_id.clone();
        self.publish_automatic(target, OutgoingSelection::Text(text))?;
        Ok(id)
    }
    pub async fn send_automatic_image(
        &self,
        device: Id,
        connection: u64,
        bytes: Vec<u8>,
    ) -> Result<Id> {
        let target = self
            .automatic_target()
            .context("phone is disconnected or paused")?;
        ensure!(
            target.device == device && target.connection == connection,
            "connection changed"
        );
        self.send_observed_image(target, bytes).await
    }
    pub async fn send_clipboard_image(&self, device: Id, bytes: Vec<u8>) -> Result<Id> {
        let target = self
            .automatic_target()
            .context("phone is disconnected or paused")?;
        ensure!(target.device == device, "device is not connected");
        self.send_observed_image(target, bytes).await
    }
    pub async fn send_observed_image(&self, target: ClipboardTarget, bytes: Vec<u8>) -> Result<Id> {
        let _clipboard = self.outgoing_gate.lock().await;
        ensure!(
            self.config().clipboard_include_images && self.clipboard_image_capable(),
            "clipboard images are disabled or unsupported"
        );
        ensure!(
            bytes.len() <= crate::ui::clipboard::MAX_IMAGE
                && bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "invalid clipboard PNG"
        );
        let id = Id::new(crate::random_hex(16))?;
        self.publish_automatic(target, OutgoingSelection::Image(id.clone(), bytes))?;
        Ok(id)
    }
    /// Explicit IPC discard is authorized against the current pairing first.
    pub async fn discard_clipboard(&self, device: Id) -> Result<()> {
        let _clipboard = self.outgoing_gate.lock().await;
        let store = {
            let _guard = self.pairing_gate.lock().await;
            ensure!(
                self.paired_devices().contains(&device),
                "device is not paired"
            );
            self.store.with_current_pairing()
        };
        store.discard_outgoing(device.clone()).await?;
        self.cancel_automatic_clipboard(&device);
        self.wake(&device);
        Ok(())
    }
    fn wake(&self, device: &Id) {
        if let Some(s) = self
            .sessions
            .lock()
            .expect("sessions")
            .session
            .as_ref()
            .filter(|s| &s.device == device)
        {
            s.wake.notify_one();
        }
    }
    pub async fn wait_shutdown(&self) {
        let mut stop = self.shutdown.subscribe();
        if !*stop.borrow() {
            let _ = stop.changed().await;
        }
    }
    pub fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
        self.action_stop.send_replace(true);
    }
    pub async fn shutdown_actions(&self) -> Result<()> {
        let task = self.action_worker.lock().expect("actions").take();
        if let Some(task) = task {
            self.action_stop.send_replace(true);
            let _ = task.await;
        }
        self.actions.shutdown().await
    }
    /// Listener injection supports in-process ephemeral-port tests. The daemon
    /// binds [::] (dual stack on Linux) and enables mDNS; tests disable mDNS.
    pub async fn serve(self: Arc<Self>, listener: TcpListener, advertise: bool) -> Result<()> {
        let listener = Arc::new(listener);
        self.port
            .store(listener.local_addr()?.port(), Ordering::SeqCst);
        let mut mdns = if advertise {
            self.optional(Advertisement::new(&self), "mDNS advertisement")
        } else {
            None
        };
        let semaphore = Arc::new(Semaphore::new(16));
        let mut tasks = JoinSet::new();
        let mut shutdown = self.shutdown.subscribe();
        let mut prune = tokio::time::interval(Duration::from_secs(3600));
        let mut discovery = tokio::time::interval(Duration::from_secs(60));
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = discovery.tick(), if advertise => {
                    match crate::discovery::candidates() {
                        Ok(addresses) if mdns.as_ref().is_none_or(|m| m.addresses != addresses) => {
                            drop(mdns.take());
                            mdns = self.optional(Advertisement::new_with_addresses(&self, addresses), "mDNS advertisement");
                        }
                        Ok(_) => {},
                        Err(e) => self.error(None, format!("address discovery failed: {e}")),
                    }
                },
                _ = prune.tick() => { if let Err(e) = self.prune_cache().await { self.error(None,format!("cache pruning failed: {e}")); } },
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
                accepted = listener.accept() => {
                    let (stream,_) = accepted?;
                    if let Ok(permit) = semaphore.clone().try_acquire_owned() {
                        let core = self.clone();
                        tasks.spawn(Box::pin(async move { let _permit = permit; let mut stop = core.shutdown.subscribe(); if *stop.borrow() { return; } let outcome = tokio::select! { biased; _ = stop.changed() => Ok(()), outcome = Box::pin(core.clone().connection(stream)) => outcome }; if let Err(e) = outcome { let reason = if let Some(protocol) = e.downcast_ref::<ProtocolError>() { format!("protocol {:?}",decode_code(protocol)) } else if let Some(io) = e.downcast_ref::<std::io::Error>() { format!("transport {:?}",io.kind()) } else { "transport or local state failure".into() }; core.error(None,format!("connection closed: {reason}")); } }));
                    }
                }
            }
        }
        drop(mdns);
        let ended = {
            let state = self.sessions.lock().expect("sessions");
            state.session.as_ref().map(|session| {
                session
                    .close
                    .send_replace(Some(CloseReason::Error(ErrorCode::Closing)));
                (session.device.clone(), session.id)
            })
        };
        if let Some((device, id)) = ended
            && self.finish_connection(&device, id)
        {
            self.optional(self.persist_phone(id).await, "shutdown last-seen metadata");
        }
        while tasks.join_next().await.is_some() {}
        self.store.drain().await;
        self.shutdown_actions().await?;
        Ok(())
    }
    fn optional<T>(&self, result: Result<T>, operation: &str) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.error(None, format!("{operation} unavailable: {error}"));
                None
            }
        }
    }
    async fn prune_cache(&self) -> Result<()> {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(Duration::from_secs(
                u64::from(self.config().cache_retention_days) * 86400,
            ))
            .context("retention interval")?;
        let cache = self.paths.cache.clone();
        self.store
            .blocking(move |_| -> Result<()> {
                let folder = crate::storage::Folder::open(&cache, false)?;
                crate::storage::prune_cache(&folder, cutoff)?;
                Ok(())
            })
            .await
    }
    async fn connection(self: Arc<Self>, stream: TcpStream) -> Result<()> {
        Box::pin(async move {
            stream.set_nodelay(true)?;
            let verifier = Arc::new(ClientVerifier::new(
                self.admission.clone(),
                self.provisional.clone(),
            ));
            let config = crate::tls::server_config(&self.identity, verifier.clone())?;
            let peer_address = stream.peer_addr()?;
            let handshake = tokio::time::timeout(
                Duration::from_secs(10),
                TlsAcceptor::from(Arc::new(config)).accept(stream),
            )
            .await;
            let mut tls = handshake??;
            let fp = certificate_fingerprint(
                tls.get_ref()
                    .1
                    .peer_certificates()
                    .context("missing client cert")?[0]
                    .as_ref(),
            );
            if !verifier.admit(&fp) {
                self.event(Event::Blocked {
                    address: peer_address.ip().to_string(),
                });
                crate::transport::close(
                    &mut tls,
                    &[Message::Error(ErrorMessage {
                        code: ErrorCode::Unauthorized,
                        message: Some("Client certificate is not admitted".into()),
                        supported_version: None,
                    })],
                )
                .await?;
                return Ok(());
            }
            let hello = match read_handshake(&mut tls).await? {
                Message::Hello(h) => h,
                Message::Error(_) => return Ok(()),
                _ => {
                    send_error(&mut tls, ErrorCode::UnexpectedMessage).await?;
                    return Ok(());
                }
            };
            if hello.version != 1 {
                send_error(&mut tls, ErrorCode::VersionMismatch).await?;
                return Ok(());
            }
            crate::transport::send(
                &mut tls,
                &Message::Hello(Hello {
                    version: 1,
                    device_id: self.identity.device_id.clone(),
                    name: self.config().name,
                    capabilities: vec![
                        "files".into(),
                        "clipboard_text".into(),
                        "clipboard_image".into(),
                    ],
                    paused: None,
                }),
                Duration::from_secs(10),
            )
            .await?;
            let known = self
                .admission
                .lock()
                .expect("admission")
                .peers
                .get(&fp)
                .cloned();
            let allow_recovery = known.is_some();
            if let Some(device) = known {
                if device != hello.device_id {
                    send_error(&mut tls, ErrorCode::Unauthorized).await?;
                    return Ok(());
                }
            } else {
                let token = match read_handshake(&mut tls).await? {
                    Message::PairRequest(p) => p.token,
                    Message::Error(_) => return Ok(()),
                    _ => {
                        send_error(&mut tls, ErrorCode::Unauthorized).await?;
                        return Ok(());
                    }
                };
                self.event(Event::PairingProgress {
                    name: Some(hello.name.clone()),
                });
                let result = self.finish_pairing(&hello.device_id, &fp, &token).await;
                self.event(Event::PairingProgress { name: None });
                let accepted = result.is_ok();
                let reply = Message::PairResult(PairResult {
                    accepted,
                    code: result.err(),
                    message: None,
                });
                if !accepted {
                    crate::transport::close(&mut tls, &[reply]).await?;
                    return Ok(());
                }
                crate::transport::send(&mut tls, &reply, Duration::from_secs(10)).await?;
            }
            // Explicit unpairing could occur while HELLO/pairing was underway.
            if self.admission.lock().expect("admission").peers.get(&fp) != Some(&hello.device_id) {
                send_error(&mut tls, ErrorCode::Unauthorized).await?;
                return Ok(());
            }
            if *self.shutdown.borrow() {
                send_error(&mut tls, ErrorCode::Closing).await?;
                return Ok(());
            }
            verifier.release_provisional();
            let (close_tx, close_rx) = watch::channel(None);
            let wake = Arc::new(Notify::new());
            // Fail fast while unpair persistence is pending, then serialize the
            // binding recheck and generation capture with pair/unpair callbacks.
            if self.sessions.lock().expect("sessions").unpairing {
                send_error(&mut tls, ErrorCode::Unauthorized).await?;
                return Ok(());
            }
            let (registered, mut store, action_scope, connection_id) = {
                let _guard = self.pairing_gate.lock().await;
                // Registration and generation-scoped events share this mutex.
                let mut state = self.sessions.lock().expect("sessions");
                // Allocate in admission order, rather than TLS-handshake order.
                let connection_id = self.next_connection.fetch_add(1, Ordering::SeqCst);
                let scope = self
                    .action_scope
                    .lock()
                    .expect("actions")
                    .as_ref()
                    .filter(|(device, _)| *device == hello.device_id)
                    .map(|(_, scope)| scope.clone());
                let registered = if state.unpairing
                    || self.admission.lock().expect("admission").peers.get(&fp)
                        != Some(&hello.device_id)
                {
                    false
                } else {
                    if let Some(scope) = &scope {
                        scope
                            .connection_generation
                            .store(connection_id, Ordering::SeqCst);
                        self.action_queue.discard_invalid();
                    }
                    if let Some(previous) = state.session.take() {
                        previous
                            .close
                            .send_replace(Some(CloseReason::Error(ErrorCode::ConnectionReplaced)));
                    }
                    state.session = Some(Session {
                        device: hello.device_id.clone(),
                        id: connection_id,
                        close: close_tx.clone(),
                        wake: wake.clone(),
                        paused: hello.paused.unwrap_or(false),
                    });
                    self.automatic
                        .lock()
                        .expect("automatic clipboard")
                        .invalidate();
                    self.publish_phone(&state, connection_id, &hello, peer_address.ip());
                    self.event(Event::PhonePaused {
                        device: hello.device_id.clone(),
                        paused: hello.paused.unwrap_or(false),
                    });
                    self.event(Event::ConnectionChanged {
                        device: hello.device_id.clone(),
                        connected: true,
                    });
                    true
                };
                (
                    registered,
                    self.store.with_current_pairing(),
                    scope,
                    connection_id,
                )
            };
            if !registered {
                send_error(&mut tls, ErrorCode::Unauthorized).await?;
                return Ok(());
            }
            self.optional(self.persist_phone(connection_id).await, "phone metadata");
            store.connection_id = Some(connection_id);
            let result = self
                .clone()
                .ready(
                    tls,
                    &hello,
                    wake,
                    ReadyState {
                        store,
                        allow_recovery,
                        action_scope,
                    },
                    close_tx,
                    close_rx,
                )
                .await;
            let current = self.finish_connection(&hello.device_id, connection_id);
            if current {
                self.optional(
                    self.persist_phone(connection_id).await,
                    "last-seen metadata",
                );
            }
            drop(verifier);
            if current { result } else { Ok(()) }
        })
        .await
    }
    fn publish_phone(
        &self,
        state: &Connections,
        connection: u64,
        hello: &Hello,
        address: std::net::IpAddr,
    ) -> bool {
        if !state
            .session
            .as_ref()
            .is_some_and(|s| s.id == connection && s.device == hello.device_id)
        {
            return false;
        }
        let mut ui = self.ui.lock().expect("UI state");
        ui.phone_generation = Some(connection);
        ui.snapshot.phone = crate::ui::Phone {
            device: Some(hello.device_id.clone()),
            name: hello.name.clone(),
            route: crate::ui::route(address).into(),
            last_seen: crate::now_ms(),
        };
        *self.peer_capabilities.lock().expect("capabilities") = hello.capabilities.clone();
        true
    }
    async fn persist_phone(&self, connection: u64) -> Result<()> {
        let sessions = self.sessions.clone();
        let ui = self.ui.clone();
        let path = self.paths.data.join("phone.json");
        self.store
            .blocking(move |_| {
                let state = sessions.lock().expect("sessions");
                if state.session.as_ref().is_none_or(|s| s.id == connection) {
                    persist_phone_generation(&ui, &path, connection)?;
                }
                Ok(())
            })
            .await
    }
    async fn finish_pairing(
        &self,
        device: &Id,
        fp: &Hash,
        token: &Hash,
    ) -> std::result::Result<(), PairCode> {
        let guard = self.pairing_gate.clone().lock_owned().await;
        {
            let state = self.admission.lock().expect("admission");
            let invitation = state.invitation.as_ref().ok_or(PairCode::PairingClosed)?;
            if !invitation.is_active() {
                return Err(PairCode::ExpiredToken);
            }
            if !bool::from(
                invitation
                    .token
                    .as_str()
                    .as_bytes()
                    .ct_eq(token.as_str().as_bytes()),
            ) {
                return Err(PairCode::InvalidToken);
            }
        }
        let admission = self.admission.clone();
        let events = self.events.clone();
        let actions = self.action_scope.clone();
        let pending = self.action_queue.clone();
        let sessions = self.sessions.clone();
        let ui = self.ui.clone();
        let automatic = self.automatic.clone();
        let device = device.clone();
        let fp = fp.clone();
        let invitation_changes = self.invitation_changes.clone();
        self.store
            .pair_then(device.clone(), fp.clone(), move || {
                let _guard = guard;
                let mut connections = sessions.lock().expect("sessions");
                connections.unpairing = false;
                automatic.lock().expect("automatic clipboard").invalidate();
                if let Some(old) = connections.session.take() {
                    old.close
                        .send_replace(Some(CloseReason::Error(ErrorCode::Unauthorized)));
                }
                let mut scopes = actions.lock().expect("actions");
                if let Some((_, old)) = scopes.take() {
                    old.cancelled.send_replace(true);
                }
                pending.discard_invalid();
                *scopes = Some((device.clone(), ActionScope::new()));
                let mut state = admission.lock().expect("admission");
                for old in state.peers.values() {
                    emit_event(
                        &ui,
                        &events,
                        Event::Unpaired {
                            device: old.clone(),
                        },
                    );
                }
                state.peers.clear();
                state.peers.insert(fp, device.clone());
                state.invitation = None;
                invitation_changes.send_modify(|r| *r = r.wrapping_add(1));
                drop(state);
                emit_event(&ui, &events, Event::Paired { device });
            })
            .await
            .map_err(|_| PairCode::StorageFailed)
    }
    async fn ready(
        self: Arc<Self>,
        tls: TlsStream<TcpStream>,
        hello: &Hello,
        wake: Arc<Notify>,
        state: ReadyState,
        close_tx: watch::Sender<Option<CloseReason>>,
        mut close: watch::Receiver<Option<CloseReason>>,
    ) -> Result<()> {
        Box::pin(async move {

        let ReadyState {
            store,
            allow_recovery,
            action_scope,
        } = state;
        let mut config = self.config.subscribe();
        let (mut reader, mut writer) = tokio::io::split(tls);
        let (write_tx, write_rx) = mpsc::channel::<Message>(32);
        let (priority_tx, priority_rx) = mpsc::channel::<Message>(8);
        let (automatic_tx, automatic_rx) = mpsc::channel::<AutomaticWrite>(1);
        let (reports_tx, mut reports_rx) = mpsc::channel::<WriteReport>(4);
        let (read_tx, mut read_rx) = mpsc::channel::<Message>(8);
        let (file_tx, file_rx) = mpsc::channel::<Message>(4);
        let (text_tx, mut text_rx) = mpsc::channel::<Message>(2);
        let writer_core = self.clone();
        let writer_close = close.clone();
        let writer_task = tokio::spawn(Box::pin(async move {
            writer_loop(writer_core, &mut writer, WriterQueues {
                normal: write_rx, priority: priority_rx, automatic: automatic_rx,
                reports: reports_tx, close: writer_close,
            }).await
        }));
        let reader_signal = close_tx.clone();
        let mut reader_close = close.clone();
        let reader_task = tokio::spawn(Box::pin(async move {
            loop {
                let frame = tokio::select! {biased;_=reader_close.changed()=>return Ok(()),frame=tokio::time::timeout(Duration::from_secs(180),read_frame(&mut reader))=>frame};
                let message = match frame {
                    Ok(Ok(Some(message))) => message,
                    Ok(Ok(None)) => return Ok(()),
                    Ok(Err(error)) => {
                        let _ = reader_signal.send(Some(CloseReason::Error(decode_code(&error))));
                        return Err(error.into());
                    }
                    Err(_) => {
                        let _ = reader_signal.send(Some(CloseReason::Error(ErrorCode::Timeout)));
                        bail!("peer idle timeout");
                    }
                };
                let ended = matches!(message, Message::Error(_));
                let queued = match message {
                    Message::Ping(p) => {
                        enqueue(&priority_tx, Message::Pong(p), &mut reader_close, None).await?
                    }
                    m => enqueue(&read_tx, m, &mut reader_close, None).await?,
                };
                if !queued {
                    return Ok(());
                }
                if ended {
                    return Ok(());
                }
            }
        }));
        let file_core = self.clone();
        let device = hello.device_id.clone();
        let file_writer = write_tx.clone();
        let file_close = close_tx.clone();
        let file_store = store.clone();
        let file_scope = action_scope.clone();
        let file_task = tokio::spawn(Box::pin(async move {
            file_core
                .file_loop(
                    file_store,
                    file_scope,
                    device,
                    file_rx,
                    file_writer,
                    file_close,
                )
                .await
        }));
        let text_core = self.clone();
        let device = hello.device_id.clone();
        let text_writer = write_tx.clone();
        let text_close = close_tx.clone();
        let text_store = store.clone();
        let text_task = tokio::spawn(Box::pin(async move {
            let mut incoming = None;
            while let Some(message) = text_rx.recv().await {
                match tokio::time::timeout(
                    Duration::from_secs(120),
                    text_core.receive(
                        &text_store,
                        action_scope.as_ref(),
                        &device,
                        message,
                        &mut incoming,
                        &text_writer,
                    ),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        let _ = text_close
                            .send(Some(CloseReason::Error(processing_error_code(&error))));
                        return Err(error);
                    }
                    Err(_) => {
                        let _ = text_close.send(Some(CloseReason::Error(ErrorCode::Timeout)));
                        bail!("text processing timeout");
                    }
                }
            }
            Ok(())
        }));
        let mut workers = ConnectionTasks {
            reader: reader_task,
            writer: writer_task,
            file: file_task,
            text: text_task,
        };
        let mut writer_finished = false;
        let mut applications_finished = false;
        let mut pending: Option<PendingOutgoing> = None;
        let mut retry_at: Option<Instant> = None;
        let mut query_outgoing = true;
        let mut recovered_pairing = !allow_recovery;
        let mut outgoing_query = None;
        let result:Result<()>=async {
            loop {
                if close.borrow().is_some() || !self.connection_current(&store, &hello.device_id) {break;}
                if pending.is_none()
                    && let Some(work) = self.take_automatic(&hello.device_id, store.connection_id.expect("connection id")) {
                    let id = work.selection.id().clone();
                    let supported = match &work.selection {
                        OutgoingSelection::Text(_) => hello.capabilities.iter().any(|c|c=="clipboard_text"),
                        OutgoingSelection::Image(_, _) => self.config().clipboard_include_images && hello.capabilities.iter().any(|c|c=="clipboard_image"),
                    };
                    if supported {
                        let deadline = Instant::now()+Duration::from_secs(60);
                        let queued = tokio::select! { biased;
                            _=close.changed()=>false,
                            sent=tokio::time::timeout_at(deadline, automatic_tx.send(work))=>{sent??;true},
                        };
                        if !queued {break;}
                        pending=Some(PendingOutgoing::queued(-1,id));
                    }
                }
                if outgoing_query.is_none() && query_outgoing && pending.is_none()
                    && self.phone_paused(&hello.device_id)==Some(false)
                    && self.config().clipboard_sync_to_phone
                    && hello.capabilities.iter().any(|c|c=="clipboard_text") {
                    let store = store.clone();
                    let device = hello.device_id.clone();
                    outgoing_query = Some(Box::pin(async move { store.first_outgoing(device).await }));
                    query_outgoing = false;
                }
                let receipt_deadline=pending.as_ref().and_then(|p|p.deadline).unwrap_or_else(||Instant::now()+Duration::from_secs(86400));
                tokio::select! {
                    biased;
                    _=close.changed()=>break,
                    Some(report)=reports_rx.recv()=>{
                        match report {
                            WriteReport::Canceled(id) => {
                                if pending.as_ref().is_some_and(|p| p.seq == -1 && p.id == id) { pending=None;query_outgoing=true; }
                            }
                            WriteReport::Sent { id, at } => {
                                if let Some(pending) = pending.as_mut() { pending.sent(&id, at); }
                            }
                        }
                    },
                    writer=&mut workers.writer=>{writer_finished=true;if close.borrow().is_none() {writer??;}break;},
                    _=tokio::time::sleep_until(receipt_deadline),if pending.as_ref().is_some_and(|p|p.deadline.is_some())=>{let _=close_tx.send(Some(CloseReason::Error(ErrorCode::Timeout)));bail!("outgoing receipt timeout");},
                    _=wake.notified()=>query_outgoing=true,
                    _=config.changed()=>query_outgoing=true,
                    _=retry_timer(retry_at),if !query_outgoing=>{retry_at=None;query_outgoing=true;},
                    outgoing=async { outgoing_query.as_mut().expect("outgoing query").await },if outgoing_query.is_some()=>{
                        outgoing_query=None;retry_at=None;
                        if pending.is_some() || self.phone_paused(&hello.device_id)!=Some(false) || !self.config().clipboard_sync_to_phone {query_outgoing=true;continue;}
                        if query_outgoing { continue; }
                        if let Some((seq,text,next))=outgoing? {
                            if next as u64>crate::now_ms() {retry_at=Some(Instant::now()+Duration::from_millis((next as u64).saturating_sub(crate::now_ms())));}
                            else {
                                if !enqueue(&write_tx,Message::Clipboard(text.clone()),&mut close,Some(Instant::now()+Duration::from_secs(60))).await? {break;}
                                pending=Some(PendingOutgoing::queued(seq,text.transfer_id));
                            }
                        }
                    },
                    message=read_rx.recv()=>{
                        let Some(message)=message else {break;};
                        if !self.connection_current(&store, &hello.device_id) {break;}
                        match message {
                            Message::Error(_)=>break,
                            Message::Status(status)=>{self.set_phone_paused(&hello.device_id,store.connection_id.expect("connection id"),status.paused);query_outgoing=true;},
                            Message::Unpair(_)=>{
                                workers.file.abort(); workers.text.abort();
                                let _ = (&mut workers.file).await;
                                let _ = (&mut workers.text).await;
                                applications_finished = true;
                                self.forget_pairing(hello.device_id.clone(), CloseReason::Error(ErrorCode::Closing), store.connection_id, Some(hello.name.clone())).await?;
                                break;
                            },
                            Message::PairRequest(_) if !recovered_pairing=>{
                                // TLS + HELLO binding were checked before READY.
                                // A consumed/expired token is irrelevant to this recovery.
                                recovered_pairing=true;
                                if !enqueue(&write_tx,Message::PairResult(PairResult {accepted:true,code:None,message:None}),&mut close,Some(receipt_deadline.min(Instant::now()+Duration::from_secs(60)))).await? {break;}
                            },
                            Message::Ack(ack)=>{
                                let Some(PendingOutgoing {seq,id,deadline})=pending.take() else {bail!("unsolicited ACK");};let deadline=deadline.unwrap_or_else(||Instant::now()+Duration::from_secs(120));ensure!(id==ack.transfer_id,"wrong ACK ID");
                                if seq >= 0 { tokio::time::timeout_at(deadline,store.outgoing_receipt(hello.device_id.clone(),seq,false,true)).await??; }
                                self.connection_event(&store,&hello.device_id,Event::ClipboardSent {device:hello.device_id.clone(),transfer_id:id});query_outgoing=true;
                            },
                            Message::Nack(nack)=>{
                                let Some(PendingOutgoing {seq,id,deadline})=pending.take() else {bail!("unsolicited NACK");};let deadline=deadline.unwrap_or_else(||Instant::now()+Duration::from_secs(120));ensure!(id==nack.transfer_id,"wrong NACK ID");
                                let blocked=!nack.retryable || matches!(nack.code,NackCode::HashMismatch|NackCode::SizeMismatch);
                                if seq >= 0 { tokio::time::timeout_at(deadline,store.outgoing_receipt(hello.device_id.clone(),seq,blocked,false)).await??; }
                                query_outgoing=true;
                                self.error(Some(hello.device_id.clone()),format!("clipboard delivery rejected: {:?}",nack.code));
                            },
                            m @ (Message::FileBegin(_)|Message::FileChunk(_)|Message::FileEnd(_))=>{if !enqueue(&file_tx,m,&mut close,pending.as_ref().and_then(|p|p.deadline)).await? {break;}},
                            m @ Message::Clipboard(_)=>{if !enqueue(&text_tx,m,&mut close,pending.as_ref().and_then(|p|p.deadline)).await? {break;}},
                            _=>bail!("unexpected READY frame"),
                        }
                    }
                }
            }Ok(())
        }.await;
        if let Err(error) = &result
            && close.borrow().is_none()
        {
            let _ = close_tx.send(Some(CloseReason::Error(processing_error_code(error))));
        }
        workers.reader.abort();
        workers.file.abort();
        workers.text.abort();
        let _ = (&mut workers.reader).await;
        if !applications_finished {
            let _ = (&mut workers.file).await;
            let _ = (&mut workers.text).await;
        }
        drop(file_tx);
        drop(text_tx);
        drop(write_tx);
        drop(automatic_tx);
        if !writer_finished
            && tokio::time::timeout(Duration::from_secs(2), &mut workers.writer)
                .await
                .is_err()
        {
            workers.writer.abort();
            let _ = (&mut workers.writer).await;
        }
        result

        }).await
    }
    async fn file_loop(
        self: Arc<Self>,
        store: Store,
        action_scope: Option<ActionScope>,
        device: Id,
        mut messages: mpsc::Receiver<Message>,
        writer: mpsc::Sender<Message>,
        close: watch::Sender<Option<CloseReason>>,
    ) -> Result<()> {
        Box::pin(async move {
            let mut incoming = None;
            let mut progress = Instant::now();
            let result: Result<()> = async {
                loop {
                    let deadline = progress + Duration::from_secs(60);
                    let message = if incoming.is_some() {
                        tokio::time::timeout_at(deadline, messages.recv()).await?
                    } else {
                        messages.recv().await
                    };
                    let Some(message) = message else {
                        break;
                    };
                    let deadline = if matches!(message, Message::FileEnd(_)) {
                        Instant::now() + Duration::from_secs(120)
                    } else if incoming.is_some() {
                        deadline
                    } else {
                        Instant::now() + Duration::from_secs(60)
                    };
                    tokio::time::timeout_at(
                        deadline,
                        self.receive(
                            &store,
                            action_scope.as_ref(),
                            &device,
                            message,
                            &mut incoming,
                            &writer,
                        ),
                    )
                    .await??;
                    progress = Instant::now();
                }
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                let _ = close.send(Some(CloseReason::Error(processing_error_code(error))));
            }
            if let Some(mut state) = incoming {
                state.file.take();
                if let Some(stage) = state.stage.take() {
                    store.discard_stage(stage).await?;
                }
            }
            result
        })
        .await
    }
    async fn receive(
        &self,
        store: &Store,
        action_scope: Option<&ActionScope>,
        device: &Id,
        message: Message,
        incoming: &mut Option<Incoming>,
        tx: &mpsc::Sender<Message>,
    ) -> Result<()> {
        Box::pin(async move {
            match message {
                Message::FileBegin(begin) => {
                    ensure!(incoming.is_none(), "second FILE_BEGIN");
                    let config = self.config();
                    let mut state = Incoming {
                        begin: begin.clone(),
                        received: 0,
                        hash: Sha256::new(),
                        file: None,
                        stage: None,
                        destination: None,
                        receipt: None,
                        save_error: None,
                        deduplicated: false,
                        storage_kind: if config.save_to_folder
                            && begin.kind != FileKind::ClipboardImage
                        {
                            crate::ui::ReceiveDestination::SaveFolder
                        } else {
                            crate::ui::ReceiveDestination::Cache
                        },
                        config,
                    };
                    match store
                        .receipt(device.clone(), begin.transfer_id.clone())
                        .await
                    {
                        Ok(Some((meta, _))) if meta == Metadata::File(begin.clone()) => {
                            state.deduplicated = true;
                        }
                        Ok(Some(_)) => state.receipt = Some(NackCode::IdConflict),
                        Ok(None) if begin.size <= state.config.max_file_bytes => {
                            self.prepare_incoming(store, &mut state).await
                        }
                        _ => state.receipt = Some(NackCode::StorageFailed),
                    }
                    self.connection_event(
                        store,
                        device,
                        Event::TransferStarted {
                            device: device.clone(),
                            metadata: begin,
                        },
                    );
                    *incoming = Some(state);
                }
                Message::FileChunk(chunk) => {
                    let state = incoming.as_mut().context("chunk without BEGIN")?;
                    ensure!(
                        chunk.transfer_id == state.begin.transfer_id
                            && chunk.offset == state.received
                            && state.received + chunk.data.len() as u64 <= state.begin.size,
                        "noncontiguous chunk or size overflow"
                    );
                    if state.receipt.is_none() && !state.deduplicated {
                        state.hash.update(&chunk.data);
                        #[cfg(test)]
                        {
                            let delay = *self.file_write_delay.lock().expect("file write delay");
                            if !delay.is_zero() {
                                tokio::time::sleep(delay).await;
                            }
                        }
                        let written = async {
                            if let Some(file) = state.file.as_mut() {
                                file.write_all(&chunk.data).await?;
                                // Tokio File may report a kernel write failure only
                                // on flush. Keep this chunk until that write resolves.
                                file.flush().await?;
                            }
                            Ok::<_, std::io::Error>(())
                        }
                        .await;
                        if let Err(error) = written {
                            let code = crate::store::save_failure_code(&error.into());
                            if state.config.save_to_folder
                                && state.save_error.is_none()
                                && state.begin.kind != FileKind::ClipboardImage
                                && matches!(code, NackCode::NotWritable | NackCode::DiskFull)
                                && self
                                    .fallback_cache(store, state, code, state.received)
                                    .await
                                    .map_err(|error| {
                                        state.receipt =
                                            Some(crate::store::save_failure_code(&error));
                                    })
                                    .is_ok()
                            {
                                if let Err(error) = state
                                    .file
                                    .as_mut()
                                    .expect("cache file")
                                    .write_all(&chunk.data)
                                    .await
                                {
                                    state.receipt =
                                        Some(crate::store::save_failure_code(&error.into()));
                                }
                            } else {
                                state.file.take();
                                state.receipt = state.receipt.or(Some(code));
                            }
                        }
                    }
                    state.received += chunk.data.len() as u64;
                    self.connection_event(
                        store,
                        device,
                        Event::TransferProgress {
                            device: device.clone(),
                            transfer_id: state.begin.transfer_id.clone(),
                            received: state.received,
                            total: state.begin.size,
                        },
                    );
                }
                Message::FileEnd(end) => {
                    ensure!(
                        incoming
                            .as_ref()
                            .is_some_and(|s| s.begin.transfer_id == end.transfer_id),
                        "END without matching BEGIN"
                    );
                    let state = incoming.as_mut().expect("checked incoming");
                    if state.received != state.begin.size {
                        state.receipt = Some(NackCode::SizeMismatch);
                    }
                    let verified = if state.receipt.is_none() && !state.deduplicated {
                        match VerifiedFile::from_stream(
                            state.begin.clone(),
                            state.received,
                            state.hash.clone().finalize(),
                        ) {
                            Ok(verified) => Some(verified),
                            Err(code) => {
                                state.receipt = Some(code);
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let mut received_file = None;
                    if let Some(verified) = verified {
                        let stored: Result<bool> = async {
                            let mut file = state.file.take().context("missing staging file")?;
                            file.flush().await?;
                            drop(file);
                            if state.save_error.is_some() {
                                store
                                    .commit_cache(
                                        device.clone(),
                                        verified,
                                        state.stage.clone().context("missing cache stage")?,
                                    )
                                    .await
                            } else if state.config.save_to_folder
                                && state.begin.kind != FileKind::ClipboardImage
                            {
                                store
                                    .commit_saved_file(
                                        device.clone(),
                                        verified,
                                        state.stage.clone().context("missing staging handle")?,
                                        self.paths.cache.clone(),
                                    )
                                    .await
                            } else {
                                store
                                    .commit_file(
                                        device.clone(),
                                        verified,
                                        state.stage.clone().context("missing staging handle")?,
                                    )
                                    .await
                            }
                        }
                        .await;
                        if matches!(stored, Ok(false)) {
                            state.deduplicated = true;
                            state.destination = None;
                        }
                        if matches!(stored, Ok(true))
                            && let Some(stage) = &state.stage
                            && let Ok(path) = stage.destination_path()
                        {
                            received_file = Some(ReceivedFile::from_file(
                                path.clone(),
                                stage.0.file.try_clone()?,
                            )?);
                            state.destination = Some(path);
                        }
                        if stored.is_ok() {
                            // Placement and receipt are already durable. Residue
                            // cleanup cannot suppress their ACK; the guard tracks it.
                            state.stage.take();
                        }
                        if let Err(error) = stored {
                            let code = crate::store::save_failure_code(&error);
                            if state.config.save_to_folder
                                && state.save_error.is_none()
                                && state.begin.kind != FileKind::ClipboardImage
                                && matches!(code, NackCode::NotWritable | NackCode::DiskFull)
                                && self
                                    .fallback_cache(store, state, code, state.received)
                                    .await
                                    .map_err(|error| {
                                        state.receipt =
                                            Some(crate::store::save_failure_code(&error));
                                    })
                                    .is_ok()
                            {
                                let verified = VerifiedFile::from_stream(
                                    state.begin.clone(),
                                    state.received,
                                    state.hash.clone().finalize(),
                                )
                                .map_err(|_| anyhow::anyhow!("cache verification failed"))?;
                                let stage = state.stage.as_ref().context("cache stage")?.clone();
                                match store
                                    .commit_cache(device.clone(), verified, stage.clone())
                                    .await
                                {
                                    Ok(true) => {
                                        let path = stage.destination_path()?;
                                        received_file = Some(ReceivedFile::from_file(
                                            path.clone(),
                                            stage.0.file.try_clone()?,
                                        )?);
                                        state.destination = Some(path);
                                        state.stage.take();
                                    }
                                    Ok(false) => {
                                        state.deduplicated = true;
                                        state.destination = None;
                                        state.stage.take();
                                    }
                                    Err(error) => {
                                        state.receipt =
                                            Some(crate::store::save_failure_code(&error));
                                    }
                                }
                            } else {
                                state.receipt = state.receipt.or(Some(code));
                            }
                        }
                    }
                    state.file.take();
                    if let Some(stage) = state.stage.take() {
                        store.discard_stage(stage).await?;
                    }
                    if let Some(code) = state.receipt {
                        self.finish_transfer_activity(store, device, &end.transfer_id);
                        self.connection_event(
                            store,
                            device,
                            Event::Error {
                                device: Some(device.clone()),
                                message: if matches!(
                                    code,
                                    NackCode::NotWritable | NackCode::DiskFull
                                ) {
                                    format!(
                                        "file transfer {} rejected at {}: {code:?}",
                                        end.transfer_id,
                                        crate::ui::display_path(state.storage_folder(&self.paths))
                                    )
                                } else {
                                    format!("file transfer {} rejected: {code:?}", end.transfer_id)
                                },
                            },
                        );
                        if matches!(code, NackCode::NotWritable | NackCode::DiskFull) {
                            self.connection_storage_result(
                                store,
                                device,
                                state.storage_kind,
                                state.storage_folder(&self.paths),
                                Some(code),
                            );
                            if let Some(scope) = action_scope {
                                let origin = ActionOrigin::captured(store, device);
                                self.action_queue.receipt(
                                    scope,
                                    origin,
                                    ReceiptNotice::SaveFailed {
                                        metadata: state.begin.clone(),
                                        code,
                                        destination: state.storage_kind,
                                        folder: state.storage_folder(&self.paths).to_owned(),
                                    },
                                );
                            }
                        }
                        tx.send(nack(end.transfer_id, code)).await?;
                    } else {
                        // Enqueue completed-file effects before publishing ACK, so
                        // a sender closing immediately after ACK cannot abort this
                        // worker between receipt publication and action admission.
                        if !state.deduplicated
                            && let Some(file) = received_file
                            && let Some(scope) = action_scope
                        {
                            let origin = ActionOrigin::captured(store, device);
                            if scope.allows(&origin) {
                                self.queue_file_actions(
                                    scope,
                                    origin,
                                    file,
                                    state.begin.clone(),
                                    state.config.clone(),
                                    state.save_error,
                                );
                            }
                        }
                        if !state.deduplicated {
                            self.connection_storage_result(
                                store,
                                device,
                                state.storage_kind,
                                state.storage_folder(&self.paths),
                                None,
                            );
                        }
                        if let Some(code) = state.save_error {
                            self.connection_storage_result(
                                store,
                                device,
                                crate::ui::ReceiveDestination::SaveFolder,
                                &state.config.save_path,
                                Some(code),
                            );
                        }
                        tx.send(if let Some(code) = state.save_error {
                            nack(end.transfer_id.clone(), code)
                        } else {
                            Message::Ack(Ack {
                                transfer_id: end.transfer_id.clone(),
                                deduplicated: state.deduplicated,
                            })
                        })
                        .await?;
                        self.connection_event(
                            store,
                            device,
                            Event::TransferFinished {
                                device: device.clone(),
                                transfer_id: end.transfer_id,
                                path: state.destination.clone(),
                                deduplicated: state.deduplicated,
                            },
                        );
                    }
                    incoming.take();
                }
                Message::Clipboard(text) => {
                    let Some(scope) = action_scope else {
                        return Ok(());
                    };
                    let origin = ActionOrigin::captured(store, device);
                    // One clipboard action at a time, including connection replacement.
                    let clipboard_application = self.clipboard_gate.clone().lock_owned().await;
                    if !scope.allows(&origin) {
                        return Ok(());
                    }
                    let claim = match store.claim_text(device.clone(), text.clone()).await {
                        Ok(None) => {
                            tx.send(Message::Ack(Ack {
                                transfer_id: text.transfer_id,
                                deduplicated: true,
                            }))
                            .await?;
                            return Ok(());
                        }
                        Err(error) => {
                            tx.send(nack(
                                text.transfer_id,
                                crate::store::save_failure_code(&error),
                            ))
                            .await?;
                            return Ok(());
                        }
                        Ok(Some(claim)) => claim,
                    };
                    let Some(copied) = scope
                        .effect(&origin, || self.actions.clipboard_text(&text.text))
                        .await
                    else {
                        claim.discard().await?;
                        return Ok(());
                    };
                    if copied.is_err() {
                        claim.discard().await?;
                        self.action_queue
                            .receipt(scope, origin, ReceiptNotice::TextFailed);
                        // Failed clipboard writes retain no pending receipt/barrier.
                        tx.send(nack(text.transfer_id, NackCode::ClipboardUnavailable))
                            .await?;
                        return Ok(());
                    }
                    if !scope.allows(&origin) {
                        claim.discard().await?;
                        return Ok(());
                    }
                    if let Err(error) = claim.finalize(clipboard_application).await {
                        tx.send(nack(
                            text.transfer_id,
                            crate::store::save_failure_code(&error),
                        ))
                        .await?;
                        return Ok(());
                    }
                    if self.config().notification_preview {
                        self.action_queue.receipt(
                            scope,
                            origin,
                            ReceiptNotice::Text(text.text.clone()),
                        );
                    }
                    tx.send(Message::Ack(Ack {
                        transfer_id: text.transfer_id.clone(),
                        deduplicated: false,
                    }))
                    .await?;
                    self.connection_event(
                        store,
                        device,
                        Event::ClipboardReceived {
                            device: device.clone(),
                            transfer_id: text.transfer_id,
                            origin: text.origin,
                        },
                    );
                }
                _ => bail!("unexpected application message"),
            }
            Ok(())
        })
        .await
    }
    /// Recover the already received prefix through its retained descriptor,
    /// then keep streaming in cache. Never resolve the selected folder again.
    async fn fallback_cache(
        &self,
        store: &Store,
        state: &mut Incoming,
        code: NackCode,
        prefix: u64,
    ) -> Result<()> {
        state.storage_kind = crate::ui::ReceiveDestination::Cache;
        state.save_error = Some(code);
        if let Some(mut file) = state.file.take() {
            let _ = file.flush().await;
        }
        let source = state.stage.as_ref().context("source stage")?.clone();
        let cache = self.paths.cache.clone();
        store
            .blocking(move |_| crate::config::private_dir(&cache))
            .await?;
        let stage = store
            .create_stage(
                self.paths.cache.clone(),
                cache_basename(&state.begin.file_name),
                state.begin.size,
            )
            .await?;
        let destination = stage.clone();
        let input = source.clone();
        store
            .blocking(move |_| {
                use std::os::unix::fs::FileExt;
                let mut offset = 0;
                let mut bytes = [0; 32768];
                while offset < prefix {
                    let n = (prefix - offset).min(bytes.len() as u64) as usize;
                    input.0.file.read_exact_at(&mut bytes[..n], offset)?;
                    destination.0.file.write_all_at(&bytes[..n], offset)?;
                    offset += n as u64;
                }
                rustix::fs::seek(&destination.0.file, rustix::fs::SeekFrom::Start(prefix))?;
                Ok(())
            })
            .await?;
        state.destination = Some(stage.destination_path()?);
        state.file = Some(tokio::fs::File::from_std(stage.0.file.try_clone()?));
        state.stage = Some(stage);
        let _ = store.discard_stage(source).await;
        Ok(())
    }
    async fn prepare_incoming(&self, store: &Store, state: &mut Incoming) {
        let result: Result<(Stage, tokio::fs::File, PathBuf)> = async {
            ensure!(
                state.begin.size <= state.config.max_file_bytes,
                "file exceeds configured limit"
            );
            store.admission().await?;
            let folder =
                if state.begin.kind == FileKind::ClipboardImage || !state.config.save_to_folder {
                    self.paths.cache.clone()
                } else {
                    state.config.save_path.clone()
                };
            if folder == self.paths.cache {
                // Optional startup cache preparation may have failed. Retry
                // private daemon-owned permissions before creating cache stages.
                let cache = folder.clone();
                store
                    .blocking(move |_| crate::config::private_dir(&cache))
                    .await?;
            }
            let destination =
                if state.begin.kind == FileKind::ClipboardImage || !state.config.save_to_folder {
                    cache_basename(&state.begin.file_name)
                } else {
                    safe_basename(&state.begin.file_name, 255)
                };
            let size = state.begin.size;
            let stage = match async {
                if state.config.save_to_folder && state.begin.kind != FileKind::ClipboardImage {
                    ensure!(
                        !crate::config::save_folder_overlaps_cache(&folder, &self.paths.cache),
                        rustix::io::Errno::ACCESS
                    );
                }
                store.create_stage(folder, destination, size).await
            }
            .await
            {
                Ok(stage) => stage,
                Err(error)
                    if state.config.save_to_folder
                        && state.begin.kind != FileKind::ClipboardImage =>
                {
                    let code = crate::store::save_failure_code(&error);
                    if !matches!(code, NackCode::NotWritable | NackCode::DiskFull) {
                        return Err(error);
                    }
                    state.save_error = Some(code);
                    state.storage_kind = crate::ui::ReceiveDestination::Cache;
                    let cache = self.paths.cache.clone();
                    store
                        .blocking(move |_| crate::config::private_dir(&cache))
                        .await?;
                    store
                        .create_stage(
                            self.paths.cache.clone(),
                            cache_basename(&state.begin.file_name),
                            size,
                        )
                        .await?
                }
                Err(error) => return Err(error),
            };
            let file = tokio::fs::File::from_std(stage.0.file.try_clone()?);
            let path = stage.destination_path()?;
            Ok((stage, file, path))
        }
        .await;
        match result {
            Ok((stage, file, path)) => {
                state.stage = Some(stage);
                state.file = Some(file);
                state.destination = Some(path);
            }
            Err(error) => state.receipt = Some(crate::store::save_failure_code(&error)),
        }
    }
    fn queue_file_actions(
        &self,
        scope: &ActionScope,
        origin: ActionOrigin,
        file: ReceivedFile,
        metadata: FileBegin,
        config: Config,
        save_error: Option<NackCode>,
    ) {
        // Scope is the one captured at session admission, never looked up again
        // by device ID. Enqueue cannot wait for any optional desktop action.
        let error_device = origin.device.clone();
        if !self.action_queue.push(ActionJob {
            scope: scope.clone(),
            origin,
            path: file.path().to_owned(),
            file,
            metadata,
            config,
            arrived: Instant::now(),
            save_error,
        }) {
            self.error(
                Some(error_device),
                "couldn't copy image: explicit image queue is full",
            );
        }
    }
    async fn file_actions(&self, job: &ActionJob, image: bool, action: DesktopAction) {
        if matches!(action, DesktopAction::CopyFailed) {
            if let Some(Err(error)) = job
                .scope
                .effect(&job.origin, || {
                    self.actions
                        .notify_image_failure(&job.file, &job.metadata, job.saved())
                })
                .await
            {
                self.error(
                    Some(job.origin.device.clone()),
                    format!("notification unavailable: {error}"),
                );
            }
            return;
        }
        if matches!(action, DesktopAction::Notification) {
            let mut metadata = job.metadata.clone();
            if !image {
                metadata.mime = "application/octet-stream".into();
            }
            if let Some(Err(error)) = job
                .scope
                .effect(&job.origin, || {
                    self.actions.notify_file_at(
                        &job.file,
                        &metadata,
                        job.saved(),
                        job.save_error,
                        &job.config.save_path,
                        job.arrived.into_std(),
                    )
                })
                .await
            {
                self.error(
                    Some(job.origin.device.clone()),
                    format!("notification unavailable: {error}"),
                );
            }
            return;
        }
        let explicit = matches!(action, DesktopAction::Explicit);
        if explicit && !image {
            self.error(
                Some(job.origin.device.clone()),
                "clipboard image content is unsupported or invalid",
            );
            let _ = job
                .scope
                .effect(&job.origin, || {
                    self.actions
                        .notify_image_failure(&job.file, &job.metadata, job.saved())
                })
                .await;
            return;
        }
        let copied = if image && (explicit || job.config.copy_image_to_clipboard) {
            match job
                .scope
                .effect(&job.origin, || {
                    self.actions
                        .clipboard_received(&job.file, &job.metadata.mime)
                })
                .await
            {
                Some(Ok(())) => true,
                None => false,
                Some(Err(e)) => {
                    self.error(
                        Some(job.origin.device.clone()),
                        format!("image clipboard unavailable: {e}"),
                    );
                    false
                }
            }
        } else {
            false
        };
        if !copied
            && image
            && (explicit
                || (job.config.copy_image_to_clipboard && !job.config.notification_preview))
        {
            let _ = job
                .scope
                .effect(&job.origin, || {
                    self.actions
                        .notify_image_failure(&job.file, &job.metadata, job.saved())
                })
                .await;
        }
        if explicit
            && copied
            && job.config.notification_preview
            && let Some(Err(e)) = job
                .scope
                .effect(&job.origin, || {
                    self.actions
                        .notify_copied_received(&job.file, &job.metadata.mime)
                })
                .await
        {
            self.error(
                Some(job.origin.device.clone()),
                format!("notification unavailable: {e}"),
            );
        }
    }
}
/// The same pairing epoch and cancellation gate cover queued actions, running
/// clipboard/notification work, and debounced viewer launches.
async fn run_action(
    core: &Core,
    job: &ActionJob,
    stop: &mut watch::Receiver<bool>,
    action: DesktopAction,
    image: bool,
) -> Option<crate::ui::portal::Interaction> {
    Box::pin(async move {
        let mut cancelled = job.scope.cancelled.subscribe();
        if *cancelled.borrow() || *stop.borrow() { return None; }
        let result = tokio::select! { biased;
            _ = stop.changed() => return None,
            _ = cancelled.changed() => return None,
            result = async {
                if matches!(action, DesktopAction::Viewer) {
                    // Only authorisation and issuance hold the side-effect gate.
                    job.scope.effect(&job.origin, || core.actions.begin_open_received(&job.file))
                        .await.transpose()
                } else {
                    tokio::time::timeout(Duration::from_secs(15), core.file_actions(job, image, action)).await?;
                    Ok(None)
                }
            } => result,
        };
        match result {
            Ok(wait) => wait,
            Err(_) => {
                core.error(Some(job.origin.device.clone()), if matches!(action, DesktopAction::Viewer) { "viewer unavailable" } else { "desktop action timeout" });
                None
            }
        }
    }).await
}
async fn run_receipt_notice(core: &Core, job: &ReceiptJob, stop: &mut watch::Receiver<bool>) {
    if *stop.borrow() {
        return;
    }
    let effect = job.scope.effect(&job.origin, || async {
        match &job.notice {
            ReceiptNotice::Text(text) => core.actions.notify_text(text).await,
            ReceiptNotice::TextFailed => core.actions.notify_text_failure().await,
            ReceiptNotice::SaveFailed {
                metadata,
                code,
                destination,
                folder,
            } => {
                core.actions
                    .notify_save_failure(metadata, *code, *destination, folder)
                    .await
            }
        }
    });
    tokio::select! { biased;
        _ = stop.changed() => {},
        result = tokio::time::timeout(Duration::from_secs(5), effect) => {
            match result {
                Ok(Some(Err(error))) => core.error(Some(job.origin.device.clone()), format!("notification unavailable: {error}")),
                Err(_) => core.error(Some(job.origin.device.clone()), "notification timeout"),
                _ => {},
            }
        }
    }
}
async fn launch_viewer(
    core: &Arc<Core>,
    job: &ActionJob,
    stop: &mut watch::Receiver<bool>,
    interactions: &mut crate::ui::portal::Interactions,
) {
    if !interactions.available() {
        return;
    }
    let Some(wait) = run_action(core, job, stop, DesktopAction::Viewer, true).await else {
        return;
    };
    let scope = job.scope.clone();
    let origin = job.origin.clone();
    let mut stop = stop.clone();
    let mut cancelled = scope.cancelled.subscribe();
    let core = Arc::downgrade(core);
    interactions.push(async move {
        // Pairing revocation has a watch; connection replacement is an atomic
        // generation, so also recheck it while the human response is pending.
        let mut authority = tokio::time::interval(Duration::from_millis(100));
        tokio::pin!(wait);
        loop {
            if *stop.borrow() || !scope.allows(&origin) {
                break;
            }
            tokio::select! { biased;
                _ = stop.changed() => break,
                _ = cancelled.changed() => break,
                _ = authority.tick() => {},
                result = &mut wait => {
                    if result.is_err() && let Some(core) = core.upgrade() {
                        core.error(Some(origin.device.clone()), "viewer unavailable");
                    }
                    break;
                }
            }
        }
    });
}
async fn action_loop(
    core: std::sync::Weak<Core>,
    jobs: Arc<ActionQueue>,
    mut stop: watch::Receiver<bool>,
) {
    Box::pin(async move {

    let mut latest: Option<ActionJob> = None;
    let mut interactions = crate::ui::portal::Interactions::new();
    loop {
        // Register before inspecting the mailbox, including after slow actions.
        let changed = jobs.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if *stop.borrow() {
            break;
        }
        if latest.as_ref().is_some_and(|j| !j.scope.allows(&j.origin)) {
            latest = None;
        }
        let next = {
            let mut pending = jobs.pending.lock().expect("pending actions");
            pending
                .explicit
                .pop_front()
                .map(|j| (j, DesktopAction::Explicit))
                .or_else(|| {
                    pending
                        .automatic
                        .take()
                        .map(|j| (j, DesktopAction::Automatic))
                })
        };
        if let Some((job, action)) = next {
            let Some(core) = core.upgrade() else {
                break;
            };
            if !job.scope.allows(&job.origin) {
                continue;
            }
            let file = job.file.clone();
            let mime = job.metadata.mime.clone();
            let image = core
                .store
                .blocking(move |_| file.is_image(&mime))
                .await
                .unwrap_or(false);
            if matches!(action, DesktopAction::Automatic) {
                latest = (image
                    && image_extension(&job.path, &job.metadata.mime)
                    && job.config.open_in_viewer)
                    .then(|| job.clone());
            }
            run_action(&core, &job, &mut stop, action, image).await;
            continue;
        }
        let deadline = latest.as_ref().map(|j| j.arrived + Duration::from_secs(1));
        if deadline.is_some_and(|due| due <= Instant::now()) {
            let Some(core) = core.upgrade() else {
                break;
            };
            launch_viewer(
                &core,
                &latest.take().expect("latest image"),
                &mut stop,
                &mut interactions,
            )
            .await;
            continue;
        }
        let notification = jobs
            .pending
            .lock()
            .expect("pending actions")
            .notifications
            .pop_front();
        if let Some(notification) = notification {
            let Some(core) = core.upgrade() else {
                break;
            };
            match notification {
                NotificationJob::File(job, action) => {
                    if !job.scope.allows(&job.origin) { continue; }
                    let file = job.file.clone();
                    let mime = job.metadata.mime.clone();
                    let image = core.store.blocking(move |_| file.is_image(&mime)).await.unwrap_or(false);
                    run_action(&core, &job, &mut stop, action, image).await;
                }
                NotificationJob::Receipt(job) => run_receipt_notice(&core, &job, &mut stop).await,
            }
            continue;
        }
        let mut cancelled = latest.as_ref().map(|j| j.scope.cancelled.subscribe());
        tokio::select! { biased;
            _ = stop.changed() => break,
            _ = async { if let Some(cancelled) = &mut cancelled { if !*cancelled.borrow() { let _ = cancelled.changed().await; } } else { std::future::pending::<()>().await; } } => latest = None,
            _ = &mut changed => {},
            _ = retry_timer(deadline) => {},
        }
    }
    interactions.shutdown().await;

        }).await
}
// Cancellation (for example a server error) must close both TLS halves too.
struct ConnectionTasks {
    reader: tokio::task::JoinHandle<Result<()>>,
    writer: tokio::task::JoinHandle<Result<()>>,
    file: tokio::task::JoinHandle<Result<()>>,
    text: tokio::task::JoinHandle<Result<()>>,
}
impl Drop for ConnectionTasks {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
        self.file.abort();
        self.text.abort();
    }
}
// Transmission is governed by frame write/flush deadlines. Receipt time starts
// only after the last frame is flushed, including durable text operations.
struct PendingOutgoing {
    seq: i64,
    id: Id,
    deadline: Option<Instant>,
}
impl PendingOutgoing {
    fn queued(seq: i64, id: Id) -> Self {
        Self {
            seq,
            id,
            deadline: None,
        }
    }
    fn sent(&mut self, id: &Id, at: Instant) {
        if self.id == *id && self.deadline.is_none() {
            self.deadline = Some(at + Duration::from_secs(120));
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
enum WriteReport {
    Canceled(Id),
    Sent { id: Id, at: Instant },
}
fn frame_receipt_id(message: &Message) -> Option<Id> {
    match message {
        Message::Clipboard(text) => Some(text.transfer_id.clone()),
        Message::FileEnd(end) => Some(end.transfer_id.clone()),
        _ => None,
    }
}
async fn report_frame(reports: &mpsc::Sender<WriteReport>, message: &Message) {
    if let Some(id) = frame_receipt_id(message) {
        let _ = reports
            .send(WriteReport::Sent {
                id,
                at: Instant::now(),
            })
            .await;
    }
}
struct WriterQueues {
    normal: mpsc::Receiver<Message>,
    priority: mpsc::Receiver<Message>,
    automatic: mpsc::Receiver<AutomaticWrite>,
    reports: mpsc::Sender<WriteReport>,
    close: watch::Receiver<Option<CloseReason>>,
}
async fn writer_loop<W: tokio::io::AsyncWrite + Unpin>(
    core: Arc<Core>,
    writer: &mut W,
    queues: WriterQueues,
) -> Result<()> {
    Box::pin(async move {
    let WriterQueues { normal: mut write_rx, priority: mut priority_rx, automatic: mut automatic_rx, reports: reports_tx, close: mut writer_close } = queues;
            loop {
                let reason = *writer_close.borrow();
                if let Some(reason) = reason {
                    return close_transport(writer, reason).await;
                }
                let work = tokio::select! {biased;
                    _=writer_close.changed()=>{let reason=writer_close.borrow().unwrap_or(CloseReason::Error(ErrorCode::Closing));return close_transport(writer,reason).await;},
                    message=priority_rx.recv()=>message.map(WriterWork::Frame),
                    message=write_rx.recv()=>message.map(WriterWork::Frame),
                    automatic=automatic_rx.recv()=>automatic.map(WriterWork::Automatic),
                };
                let Some(work) = work else { break; };
                let message = match work {
                    WriterWork::Frame(message) => message,
                    WriterWork::Automatic(work) => {
                        let id = work.selection.id().clone();
                        let sent = tokio::select! { biased;
                            _ = writer_close.changed() => None,
                            sent = transmit_automatic(&core, writer, work, &mut priority_rx, &mut write_rx, Some(&reports_tx)) => Some(sent),
                        };
                        match sent {
                            Some(Ok(false)) => { let _ = reports_tx.send(WriteReport::Canceled(id)).await; }
                            Some(result) => { result?; let _ = reports_tx.send(WriteReport::Sent {id, at: Instant::now()}).await; }
                            None => return crate::transport::close(writer, &[]).await,
                        }
                        continue;
                    }
                };
                if matches!(message, Message::Error(_) | Message::Unpair(_)) {
                    return crate::transport::close(writer, &[message]).await;
                }
                let sent = tokio::select! { biased;
                    _ = writer_close.changed() => None,
                    sent = send(writer, &message) => Some(sent),
                };
                match sent {
                    Some(result) => {
                        result?;
                        report_frame(&reports_tx,&message).await;
                    },
                    // A canceled frame may be partial. Do not append ERROR or
                    // UNPAIR to that frame; close the unusable stream promptly.
                    None => return crate::transport::close(writer, &[]).await,
                }
            }
            crate::transport::close(writer, &[]).await
    }).await
}

struct Incoming {
    begin: FileBegin,
    received: u64,
    hash: Sha256,
    file: Option<tokio::fs::File>,
    stage: Option<Stage>,
    destination: Option<PathBuf>,
    receipt: Option<NackCode>,
    save_error: Option<NackCode>,
    storage_kind: crate::ui::ReceiveDestination,
    deduplicated: bool,
    config: Config,
}
impl Incoming {
    fn storage_folder<'a>(&'a self, paths: &'a Paths) -> &'a std::path::Path {
        match self.storage_kind {
            crate::ui::ReceiveDestination::SaveFolder => &self.config.save_path,
            crate::ui::ReceiveDestination::Cache => &paths.cache,
        }
    }
}
async fn close_transport<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    reason: CloseReason,
) -> Result<()> {
    let messages = match reason {
        CloseReason::Unpair => vec![reason.message(), error_message(ErrorCode::Closing)],
        _ => vec![reason.message()],
    };
    crate::transport::close(writer, &messages).await
}

async fn read_handshake(tls: &mut TlsStream<TcpStream>) -> Result<Message> {
    match tokio::time::timeout(Duration::from_secs(10), read_frame(tls)).await {
        Ok(Ok(Some(m))) => Ok(m),
        Ok(Err(e)) => {
            let _ = send_error(tls, decode_code(&e)).await;
            Err(e.into())
        }
        Ok(Ok(None)) => bail!("peer closed handshake"),
        Err(_) => {
            let _ = send_error(tls, ErrorCode::Timeout).await;
            bail!("handshake message timeout");
        }
    }
}
fn decode_code(error: &ProtocolError) -> ErrorCode {
    match error {
        ProtocolError::UnknownType(_) => ErrorCode::UnknownMessage,
        ProtocolError::InvalidLength(n) if *n > MAX_CONTROL_PAYLOAD + 1 => ErrorCode::FrameTooLarge,
        _ => ErrorCode::MalformedMessage,
    }
}
fn error_message(code: ErrorCode) -> Message {
    Message::Error(ErrorMessage {
        code,
        message: None,
        supported_version: (code == ErrorCode::VersionMismatch).then_some(1),
    })
}
fn nack(id: Id, code: NackCode) -> Message {
    Message::Nack(Nack {
        transfer_id: id,
        code,
        retryable: code.retryable(),
        message: None,
    })
}
async fn send_error(tls: &mut TlsStream<TcpStream>, code: ErrorCode) -> Result<()> {
    crate::transport::close(tls, &[error_message(code)]).await
}
/// An overdue retry is immediately ready; an absent retry stays parked.
async fn retry_timer(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}
/// Bounded queues propagate backpressure to TCP instead of rejecting frames.
/// Waiting for capacity must still honor connection closure and receipt timers.
async fn enqueue(
    tx: &mpsc::Sender<Message>,
    message: Message,
    close: &mut watch::Receiver<Option<CloseReason>>,
    deadline: Option<Instant>,
) -> Result<bool> {
    if close.borrow().is_some() {
        return Ok(false);
    }
    let queued = async {
        tokio::select! { biased;
            _ = close.changed() => Ok(false),
            sent = tx.send(message) => {
                sent.context("connection queue closed")?;
                Ok(true)
            }
        }
    };
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, queued).await?,
        None => queued.await,
    }
}
async fn send<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, message: &Message) -> Result<()> {
    crate::transport::send(writer, message, Duration::from_secs(60)).await
}
struct Advertisement {
    daemon: mdns_sd::ServiceDaemon,
    fullname: String,
    addresses: Vec<crate::discovery::InterfaceAddress>,
}
impl Advertisement {
    fn new(core: &Core) -> Result<Self> {
        Self::new_with_addresses(core, crate::discovery::candidates()?)
    }
    fn new_with_addresses(
        core: &Core,
        addresses: Vec<crate::discovery::InterfaceAddress>,
    ) -> Result<Self> {
        ensure!(!addresses.is_empty(), "no reachable interface for mDNS");
        let instance = format!("Pneu-{}", &core.identity.device_id.as_str()[..8]);
        let ips: Vec<_> = addresses.iter().map(|i| i.ip).collect();
        let mut info = mdns_sd::ServiceInfo::new(
            "_pneu._tcp.local.",
            &instance,
            &format!("{instance}.local."),
            ips.as_slice(),
            core.bound_port(),
            [("v", "1"), ("fp", core.identity.fingerprint.as_str())].as_slice(),
        )?;
        info.set_interfaces(ips.iter().copied().map(mdns_sd::IfKind::Addr).collect());
        let fullname = info.get_fullname().to_owned();
        let daemon = mdns_sd::ServiceDaemon::new()?;
        // Exclude bridges and unrelated virtual interfaces from multicast too.
        let advertisement = Self {
            daemon,
            fullname,
            addresses,
        };
        advertisement
            .daemon
            .disable_interface(mdns_sd::IfKind::All)?;
        advertisement.daemon.enable_interface(ips)?;
        advertisement.daemon.register(info)?;
        Ok(advertisement)
    }
}
impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        if let Ok(done) = self.daemon.shutdown() {
            let _ = done.recv_timeout(Duration::from_secs(2));
        }
    }
}

/// Automatic viewers also require a recognized, matching image extension.
fn image_extension(path: &std::path::Path, mime: &str) -> bool {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        (mime, ext.as_str()),
        ("image/png", "png")
            | ("image/jpeg", "jpg" | "jpeg")
            | ("image/gif", "gif")
            | ("image/webp", "webp")
            | ("image/bmp", "bmp")
    )
}

fn processing_error_code(error: &anyhow::Error) -> ErrorCode {
    if error
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some()
    {
        ErrorCode::Timeout
    } else {
        ErrorCode::UnexpectedMessage
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn receipt_notices_share_bounded_queue_and_revocation_rules() {
        let queue = ActionQueue::default();
        let scope = ActionScope::new();
        let origin = ActionOrigin {
            device: Id::new("a".repeat(32)).unwrap(),
            connection_id: Some(0),
        };
        for index in 0..NOTIFICATION_LIMIT + 3 {
            queue.receipt(
                &scope,
                origin.clone(),
                ReceiptNotice::Text(index.to_string()),
            );
        }
        {
            let pending = queue.pending.lock().unwrap();
            assert_eq!(pending.notifications.len(), NOTIFICATION_LIMIT);
            assert!(
                matches!(pending.notifications.front(), Some(NotificationJob::Receipt(ReceiptJob { notice: ReceiptNotice::Text(text), .. })) if text == "3")
            );
        }
        scope.connection_generation.store(1, Ordering::SeqCst);
        queue.discard_invalid();
        assert!(queue.pending.lock().unwrap().notifications.is_empty());
        queue.receipt(&scope, origin.clone(), ReceiptNotice::TextFailed);
        assert!(queue.pending.lock().unwrap().notifications.is_empty());
        let origin = ActionOrigin {
            connection_id: Some(1),
            ..origin
        };
        queue.receipt(&scope, origin.clone(), ReceiptNotice::TextFailed);
        scope.cancelled.send_replace(true);
        queue.discard_invalid();
        queue.receipt(&scope, origin, ReceiptNotice::TextFailed);
        assert!(queue.pending.lock().unwrap().notifications.is_empty());
    }
    use super::*;
    impl Core {
        async fn receive_test(
            &self,
            store: &Store,
            device: &Id,
            message: Message,
            incoming: &mut Option<Incoming>,
            tx: &mpsc::Sender<Message>,
        ) -> Result<()> {
            let scope = self
                .action_scope
                .lock()
                .unwrap()
                .as_ref()
                .map(|(_, scope)| scope.clone());
            self.receive(store, scope.as_ref(), device, message, incoming, tx)
                .await
        }
        async fn file_loop_test(
            self: Arc<Self>,
            store: Store,
            device: Id,
            messages: mpsc::Receiver<Message>,
            writer: mpsc::Sender<Message>,
            close: watch::Sender<Option<CloseReason>>,
        ) -> Result<()> {
            let scope = self
                .action_scope
                .lock()
                .unwrap()
                .as_ref()
                .map(|(_, scope)| scope.clone());
            self.file_loop(store, scope, device, messages, writer, close)
                .await
        }
    }
    fn admit_ui_test_phone(
        core: &Core,
        device: &Id,
        id: u64,
    ) -> Result<watch::Receiver<Option<CloseReason>>> {
        let (close, receiver) = watch::channel(None);
        core.admission
            .lock()
            .unwrap()
            .peers
            .insert(Hash::new(crate::random_hex(32))?, device.clone());
        let mut state = core.sessions.lock().unwrap();
        state.session = Some(Session {
            device: device.clone(),
            id,
            close,
            wake: Arc::new(Notify::new()),
            paused: false,
        });
        core.publish_phone(
            &state,
            id,
            &Hello {
                version: 1,
                device_id: device.clone(),
                name: format!("Phone {id}"),
                capabilities: vec!["clipboard_text".into(), "clipboard_image".into()],
                paused: Some(false),
            },
            "192.168.1.2".parse()?,
        );
        core.event(Event::ConnectionChanged {
            device: device.clone(),
            connected: true,
        });
        Ok(receiver)
    }
    struct ObservedWriter<W> {
        inner: W,
        polled: Arc<Notify>,
    }
    impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for ObservedWriter<W> {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let result = std::pin::Pin::new(&mut self.inner).poll_write(cx, bytes);
            self.polled.notify_one();
            result
        }
        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }
    async fn test_automatic_work(core: &Core, device: &Id, image: bool) -> Result<AutomaticWrite> {
        let target = core.observe_clipboard_offer().context("target")?;
        if image {
            core.send_observed_image(target, b"\x89PNG\r\n\x1a\nautomatic".to_vec())
                .await?;
        } else {
            core.send_observed_clipboard(target, "automatic".into())
                .await?;
        }
        core.take_automatic(device, 1).context("automatic work")
    }
    #[derive(Default)]
    struct InteractiveViewer {
        issued: AtomicU64,
        effects: AtomicU64,
        alive: Arc<AtomicU64>,
        notified_arrivals: Mutex<Vec<std::time::Instant>>,
    }
    struct ViewerLifetime(Arc<AtomicU64>);
    impl Drop for ViewerLifetime {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl PcActions for InteractiveViewer {
        async fn clipboard_text(&self, _text: &str) -> Result<()> {
            self.effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn clipboard_image(&self, _path: &std::path::Path, _mime: &str) -> Result<()> {
            self.effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn notify(&self, _path: &std::path::Path, _mime: &str) -> Result<()> {
            self.effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn notify_file_at(
            &self,
            _file: &ReceivedFile,
            _metadata: &FileBegin,
            _saved: bool,
            _save_error: Option<NackCode>,
            _folder: &std::path::Path,
            arrived: std::time::Instant,
        ) -> Result<()> {
            self.notified_arrivals.lock().unwrap().push(arrived);
            self.effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn open(&self, _path: &std::path::Path) -> Result<()> {
            unreachable!("use split viewer API")
        }
        async fn begin_open_received(
            &self,
            _file: &ReceivedFile,
        ) -> Result<crate::ui::portal::Interaction> {
            self.issued.fetch_add(1, Ordering::SeqCst);
            self.alive.fetch_add(1, Ordering::SeqCst);
            let lifetime = ViewerLifetime(self.alive.clone());
            Ok(Box::pin(async move {
                let _lifetime = lifetime;
                std::future::pending().await
            }))
        }
    }
    #[tokio::test(start_paused = true)]
    async fn viewer_wait_releases_gate_is_bounded_and_cancels_on_revocation_and_shutdown()
    -> Result<()> {
        let actions = Arc::new(InteractiveViewer::default());
        let (dir, core, device) = fixture(actions.clone()).await?;
        let scope = core
            .action_scope
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .clone();
        let job = ActionJob {
            file: ReceivedFile::from_file(dir.path().join("file"), tempfile::tempfile()?)?,
            scope: scope.clone(),
            origin: ActionOrigin {
                device,
                connection_id: Some(0),
            },
            path: dir.path().join("file"),
            metadata: test_begin(b""),
            config: core.config(),
            arrived: Instant::now(),
            save_error: None,
        };
        let (stop, mut rx) = watch::channel(false);
        let mut interactions = crate::ui::portal::Interactions::new();
        for _ in 0..crate::ui::portal::Interactions::LIMIT + 2 {
            launch_viewer(&core, &job, &mut rx, &mut interactions).await;
        }
        assert_eq!(actions.issued.load(Ordering::SeqCst), 8);
        tokio::time::advance(Duration::from_secs(180)).await;
        assert_eq!(
            actions.alive.load(Ordering::SeqCst),
            8,
            "human waits must not time out"
        );
        // The text side-effect mutex and ordinary actions remain available.
        scope
            .effect(&job.origin, || actions.clipboard_text("next"))
            .await
            .unwrap()?;
        run_action(&core, &job, &mut rx, DesktopAction::Notification, false).await;
        assert_eq!(actions.effects.load(Ordering::SeqCst), 2);
        // Processing happens 180 seconds later; Core must forward the job's
        // original arrival to the backend rather than create a dispatch time.
        assert_eq!(
            *actions.notified_arrivals.lock().unwrap(),
            vec![job.arrived.into_std()]
        );
        scope.connection_generation.store(1, Ordering::SeqCst);
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            actions.alive.load(Ordering::SeqCst),
            0,
            "replacement cancels old response waits"
        );
        let current = ActionJob {
            origin: ActionOrigin {
                connection_id: Some(1),
                ..job.origin.clone()
            },
            ..job
        };
        launch_viewer(&core, &current, &mut rx, &mut interactions).await;
        scope.cancelled.send_replace(true);
        tokio::task::yield_now().await;
        assert_eq!(
            actions.alive.load(Ordering::SeqCst),
            0,
            "unpair cancels response waits"
        );
        let current = ActionJob {
            scope: ActionScope::new(),
            origin: ActionOrigin {
                connection_id: None,
                ..current.origin.clone()
            },
            ..current
        };
        launch_viewer(&core, &current, &mut rx, &mut interactions).await;
        stop.send_replace(true);
        interactions.shutdown().await;
        assert_eq!(actions.alive.load(Ordering::SeqCst), 0);
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn interactive_viewer_does_not_starve_action_worker() -> Result<()> {
        let actions = Arc::new(InteractiveViewer::default());
        let (dir, core, device) = fixture(actions.clone()).await?;
        let scope = core
            .action_scope
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .clone();
        let path = dir.path().join("image.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n")?;
        let job = ActionJob {
            file: ReceivedFile::open(&path)?,
            scope,
            origin: ActionOrigin {
                device,
                connection_id: None,
            },
            path,
            metadata: FileBegin {
                mime: "image/png".into(),
                ..test_begin(b"")
            },
            config: Config {
                open_in_viewer: true,
                copy_image_to_clipboard: false,
                notification_preview: false,
                ..core.config()
            },
            arrived: Instant::now() - Duration::from_secs(2),
            save_error: None,
        };
        core.action_queue.push(job.clone());
        tokio::time::timeout(Duration::from_secs(2), async {
            while actions.issued.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let explicit = ActionJob {
            metadata: FileBegin {
                kind: FileKind::ClipboardImage,
                ..job.metadata.clone()
            },
            ..job
        };
        core.action_queue.push(explicit);
        tokio::time::timeout(Duration::from_secs(2), async {
            while actions.effects.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        core.shutdown_actions().await?;
        assert_eq!(
            actions.alive.load(Ordering::SeqCst),
            0,
            "shutdown must join viewer tasks"
        );
        Ok(())
    }
    struct SlowFlushWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    }
    impl tokio::io::AsyncWrite for SlowFlushWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            std::task::Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            use std::future::Future;
            let delay = self
                .sleep
                .get_or_insert_with(|| Box::pin(tokio::time::sleep(Duration::from_secs(45))));
            if delay.as_mut().poll(cx).is_pending() {
                return std::task::Poll::Pending;
            }
            self.sleep = None;
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    #[tokio::test(start_paused = true)]
    async fn receipt_deadline_begins_after_last_frame_flush_for_all_outgoing_kinds() -> Result<()> {
        for (image, durable) in [(true, false), (false, false), (false, true)] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let close = admit_ui_test_phone(&core, &device, 1)?;
            core.toggle_setting(crate::ui::Toggle::IncludeImages)
                .await?;
            let work = test_automatic_work(&core, &device, image).await?;
            let id = work.selection.id().clone();
            let mut pending = PendingOutgoing::queued(if durable { 1 } else { -1 }, id.clone());
            let (normal_tx, normal) = mpsc::channel(4);
            let (_priority_tx, priority) = mpsc::channel(4);
            let (automatic_tx, automatic) = mpsc::channel(1);
            let (reports, mut rx) = mpsc::channel(4);
            if durable {
                let OutgoingSelection::Text(text) = work.selection else {
                    unreachable!()
                };
                normal_tx.send(Message::Clipboard(text)).await?;
            } else {
                automatic_tx
                    .send(work)
                    .await
                    .map_err(|_| anyhow::anyhow!("writer queue"))?;
            }
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let output = bytes.clone();
            let writer_core = core.clone();
            let writer = tokio::spawn(Box::pin(async move {
                let mut stream = SlowFlushWriter {
                    bytes: output,
                    sleep: None,
                };
                writer_loop(
                    writer_core,
                    &mut stream,
                    WriterQueues {
                        normal,
                        priority,
                        automatic,
                        reports,
                        close,
                    },
                )
                .await
            }));
            tokio::task::yield_now().await;
            let start = Instant::now();
            let frames = if image { 3 } else { 1 };
            for index in 0..frames {
                assert!(pending.deadline.is_none());
                assert!(rx.try_recv().is_err());
                tokio::time::advance(Duration::from_secs(45)).await;
                tokio::task::yield_now().await;
                if index + 1 < frames {
                    assert!(rx.try_recv().is_err());
                }
            }
            let WriteReport::Sent { id: sent, at } =
                rx.recv().await.context("completion report")?
            else {
                bail!("unexpected cancellation");
            };
            assert_eq!(sent, id);
            assert_eq!(at - start, Duration::from_secs(45 * frames));
            pending.sent(&sent, at);
            assert_eq!(pending.deadline, Some(at + Duration::from_secs(120)));
            let deadline = pending.deadline;
            pending.sent(&sent, at + Duration::from_secs(1));
            assert_eq!(pending.deadline, deadline);
            if image {
                let mut bytes = std::io::Cursor::new(bytes.lock().unwrap().clone());
                assert!(matches!(
                    read_frame(&mut bytes).await?,
                    Some(Message::FileBegin(_))
                ));
                assert!(matches!(
                    read_frame(&mut bytes).await?,
                    Some(Message::FileChunk(_))
                ));
                assert!(matches!(
                    read_frame(&mut bytes).await?,
                    Some(Message::FileEnd(_))
                ));
            }
            writer.abort();
            let _ = writer.await;
            core.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn writer_queue_drops_stale_text_and_images_and_remains_usable() -> Result<()> {
        for image in [false, true] {
            for disable in [false, true] {
                let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
                let close = admit_ui_test_phone(&core, &device, 1)?;
                core.toggle_setting(crate::ui::Toggle::IncludeImages)
                    .await?;
                let work = test_automatic_work(&core, &device, image).await?;
                let stale = work.selection.id().clone();
                let (normal_tx, normal) = mpsc::channel(4);
                let (_priority_tx, priority) = mpsc::channel(4);
                let (automatic_tx, automatic) = mpsc::channel(1);
                let (canceled, mut cancellations) = mpsc::channel(1);
                let ping = Message::Ping(Ping {
                    nonce: Nonce::new("a".repeat(16))?,
                });
                normal_tx.send(ping.clone()).await?;
                automatic_tx
                    .send(work)
                    .await
                    .map_err(|_| anyhow::anyhow!("writer queue closed"))?;
                let (stream, mut reader) = tokio::io::duplex(1);
                let polled = Arc::new(Notify::new());
                let mut writer = ObservedWriter {
                    inner: stream,
                    polled: polled.clone(),
                };
                let writer_core = core.clone();
                let task = tokio::spawn(Box::pin(async move {
                    writer_loop(
                        writer_core,
                        &mut writer,
                        WriterQueues {
                            normal,
                            priority,
                            automatic,
                            reports: canceled,
                            close,
                        },
                    )
                    .await
                }));
                polled.notified().await; // earlier plain frame blocks the automatic queue
                if disable {
                    core.toggle_setting(crate::ui::Toggle::Clipboard).await?;
                } else {
                    core.observe_clipboard_offer();
                }
                assert_eq!(read_frame(&mut reader).await?, Some(ping.clone()));
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), cancellations.recv()).await?,
                    Some(WriteReport::Canceled(stale))
                );
                normal_tx.send(ping.clone()).await?;
                assert_eq!(read_frame(&mut reader).await?, Some(ping)); // canceled work wrote zero bytes
                if disable {
                    core.toggle_setting(crate::ui::Toggle::Clipboard).await?;
                }
                let work = test_automatic_work(&core, &device, image).await?;
                let id = work.selection.id().clone();
                automatic_tx
                    .send(work)
                    .await
                    .map_err(|_| anyhow::anyhow!("writer queue closed"))?;
                if image {
                    assert!(
                        matches!(read_frame(&mut reader).await?,Some(Message::FileBegin(begin)) if begin.transfer_id==id)
                    );
                    core.observe_clipboard_offer(); // begun image finishes despite newer offer
                    normal_tx
                        .send(Message::Ack(Ack {
                            transfer_id: Id::new("f".repeat(32))?,
                            deduplicated: false,
                        }))
                        .await?;
                    let mut chunk_seen = false;
                    let mut receipt_seen = false;
                    loop {
                        match read_frame(&mut reader).await? {
                            Some(Message::FileChunk(chunk)) => {
                                assert_eq!(chunk.transfer_id, id);
                                chunk_seen = true;
                            }
                            Some(Message::Ack(_)) => receipt_seen = true,
                            Some(Message::FileEnd(end)) => {
                                assert_eq!(end.transfer_id, id);
                                assert!(
                                    chunk_seen && receipt_seen,
                                    "incoming receipt must interleave before image END"
                                );
                                break;
                            }
                            other => panic!("unexpected image frame {other:?}"),
                        }
                    }
                } else {
                    assert!(
                        matches!(read_frame(&mut reader).await?,Some(Message::Clipboard(text)) if text.transfer_id==id)
                    );
                }
                drop(normal_tx);
                task.await??;
                core.shutdown_actions().await?;
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn first_byte_rechecks_authority_after_pending_without_aborting_started_frames()
    -> Result<()> {
        use tokio::io::AsyncReadExt;
        for image in [false, true] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let _close = admit_ui_test_phone(&core, &device, 1)?;
            core.toggle_setting(crate::ui::Toggle::IncludeImages)
                .await?;
            let work = test_automatic_work(&core, &device, image).await?;
            let (mut stream, mut reader) = tokio::io::duplex(1);
            stream.write_all(&[42]).await?;
            let polled = Arc::new(Notify::new());
            let mut writer = ObservedWriter {
                inner: stream,
                polled: polled.clone(),
            };
            let writer_core = core.clone();
            let (_tx, mut priority) = mpsc::channel(1);
            let (_normal_tx, mut normal) = mpsc::channel(1);
            let task = tokio::spawn(Box::pin(async move {
                transmit_automatic(
                    &writer_core,
                    &mut writer,
                    work,
                    &mut priority,
                    &mut normal,
                    None,
                )
                .await
            }));
            polled.notified().await; // no automatic byte accepted: duplex is full
            core.observe_clipboard_offer();
            assert_eq!(reader.read_u8().await?, 42);
            assert!(!task.await??);
            assert_eq!(
                reader.read_u8().await.unwrap_err().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
            core.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn old_ready_cannot_take_replacement_selection() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _old = admit_ui_test_phone(&core, &device, 1)?;
        let _new = admit_ui_test_phone(&core, &device, 2)?;
        let target = core.observe_clipboard_offer().unwrap();
        let id = core
            .send_observed_clipboard(target, "replacement".into())
            .await?;
        assert!(core.take_automatic(&device, 1).is_none());
        assert_eq!(core.take_automatic(&device, 2).unwrap().selection.id(), &id);
        let transfer = test_begin(b"new session activity");
        core.event(Event::TransferStarted {
            device: device.clone(),
            metadata: transfer.clone(),
        });
        let mut old_store = core.store.clone();
        old_store.connection_id = Some(1);
        core.finish_transfer_activity(&old_store, &device, &transfer.transfer_id);
        assert!(core.ui_snapshot().receiving);
        assert!(!core.finish_connection(&device, 1));
        assert!(core.ui_snapshot().receiving);
        assert!(core.finish_connection(&device, 2));
        assert!(!core.ui_snapshot().receiving);
        assert!(core.ui.lock().unwrap().receiving_until.is_none());
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn cli_and_window_share_invitation_identity_and_subscription() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        core.unpair_device(device).await?;
        let (listener, _lock) = crate::ipc::bind(&core.paths.socket)?;
        let (stop, rx) = watch::channel(false);
        let daemon = tokio::spawn(crate::ipc::serve(core.clone(), listener, rx));
        let first = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        let ensured: PairingUri = serde_json::from_value(
            crate::ipc::request(&core.paths.socket, crate::ipc::Command::EnsurePair).await?,
        )?;
        assert_eq!(ensured, first);
        let status = crate::ipc::request(&core.paths.socket, crate::ipc::Command::UiStatus).await?;
        assert_eq!(
            serde_json::from_value::<PairingUri>(status["invitation"].clone())?,
            first
        );
        let revision = status["invitation_revision"].as_u64().unwrap();
        let socket = core.paths.socket.clone();
        let notification = tokio::spawn(async move {
            crate::ipc::request(&socket, crate::ipc::Command::WaitPairingChange { revision }).await
        });
        // Same Core replacement used by the CLI, with deterministic addresses.
        let next = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        assert_ne!(first.token, next.token);
        assert_ne!(notification.await??["revision"].as_u64(), Some(revision));
        let status = crate::ipc::request(&core.paths.socket, crate::ipc::Command::UiStatus).await?;
        assert_eq!(
            serde_json::from_value::<PairingUri>(status["invitation"].clone())?,
            next
        );
        // A delayed Close from the old QR cannot revoke a CLI replacement,
        // even if both invitations happen to have identical expiry milliseconds.
        core.close_pairing_if(first.token.clone()).await;
        assert_eq!(core.pairing_status().1, Some(next.clone()));
        assert_eq!(
            core.finish_pairing(
                &Id::from_bytes([2; 16]),
                &Hash::new(crate::random_hex(32))?,
                &first.token
            )
            .await,
            Err(PairCode::InvalidToken)
        );
        let revision = core.pairing_status().0;
        core.finish_pairing(
            &Id::from_bytes([2; 16]),
            &Hash::new(crate::random_hex(32))?,
            &next.token,
        )
        .await
        .map_err(|e| anyhow::anyhow!("pairing: {e:?}"))?;
        assert!(core.pairing_status().1.is_none());
        assert_ne!(core.wait_pairing_change(revision).await, revision);
        let active = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        core.close_pairing_if(active.token).await;
        assert!(core.pairing_status().1.is_none());
        core.start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        core.admission
            .lock()
            .unwrap()
            .invitation
            .as_mut()
            .unwrap()
            .deadline = std::time::Instant::now() - Duration::from_secs(1);
        assert!(core.pairing_status().1.is_none());
        core.request_shutdown();
        stop.send_replace(true);
        daemon.await??;
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn expired_invitations_are_hidden_rejected_and_refreshed_for_either_clock() -> Result<()>
    {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        for (wall, monotonic) in [(true, false), (false, true), (true, true)] {
            let old = core
                .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
                .await?;
            assert_eq!(core.ensure_pairing().await?, old);
            let revision = core.pairing_status().0;
            {
                let mut admission = core.admission.lock().unwrap();
                let invitation = admission.invitation.as_mut().unwrap();
                if wall {
                    invitation.expires_at = crate::now_ms() - 1;
                }
                if monotonic {
                    invitation.deadline = std::time::Instant::now() - Duration::from_secs(1);
                }
            }
            assert!(core.pairing_status().1.is_none());
            assert_eq!(
                core.finish_pairing(&device, &Hash::new(crate::random_hex(32))?, &old.token)
                    .await,
                Err(PairCode::ExpiredToken)
            );
            let new = core.ensure_pairing().await?;
            assert_ne!(old.token, new.token);
            assert_eq!(core.pairing_status().1, Some(new.clone()));
            assert_ne!(core.pairing_status().0, revision);
            assert_eq!(core.ensure_pairing().await?, new);
        }
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn ui_and_status_ipc_use_one_phone_snapshot_across_replacement() -> Result<()> {
        let (_dir, core, old_device) = fixture(Arc::new(NoActions)).await?;
        let _old = admit_ui_test_phone(&core, &old_device, 1)?;
        core.persist_phone(1).await?;
        let (listener, _lock) = crate::ipc::bind(&core.paths.socket)?;
        let (stop, stopped) = watch::channel(false);
        let ipc = tokio::spawn(crate::ipc::serve(core.clone(), listener, stopped));
        for generation in 2..6 {
            let old = core.ui_snapshot();
            assert_eq!(old.phone.device, old.device);
            assert_eq!(old.generation, Some(generation - 1));
            let invitation = core
                .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
                .await?;
            let device = Id::new(crate::random_hex(16))?;
            core.finish_pairing(
                &device,
                &Hash::new(crate::random_hex(32))?,
                &invitation.token,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            // The replacement is actionable before HELLO publishes its name.
            let reply =
                crate::ipc::request(&core.paths.socket, crate::ipc::Command::UiStatus).await?;
            let pending: crate::ui::Snapshot = serde_json::from_value(reply["snapshot"].clone())?;
            assert_eq!(pending.device, Some(device.clone()));
            assert_eq!(pending.phone.device, pending.device);
            assert_eq!(reply["device"], reply["snapshot"]["device"]);
            assert_eq!(pending.phone.name, "Phone");
            assert_eq!(pending.phone.last_seen, 0);
            assert!(pending.generation.is_none());
            assert!(!pending.connected && !pending.paused && !pending.receiving);
            let _current = admit_ui_test_phone(&core, &device, generation)?;
            core.set_phone_paused(&device, generation, true);
            let reply =
                crate::ipc::request(&core.paths.socket, crate::ipc::Command::UiStatus).await?;
            let current: crate::ui::Snapshot = serde_json::from_value(reply["snapshot"].clone())?;
            assert_eq!(reply["device"], reply["snapshot"]["device"]);
            assert_eq!(current.phone.device, current.device);
            assert_eq!(current.phone.name, format!("Phone {generation}"));
            assert_eq!(current.generation, Some(generation));
            assert!(current.connected && current.paused);
            let status =
                crate::ipc::request(&core.paths.socket, crate::ipc::Command::Status).await?;
            for key in ["paired_devices", "connected_devices", "paused_devices"] {
                assert_eq!(status[key], serde_json::json!([device]));
            }
            core.unpair_device(old.device.unwrap()).await?;
            assert_eq!(core.ui_snapshot().device, Some(device));
        }
        let current = core.ui_snapshot().device.unwrap();
        core.unpair_device(current).await?;
        let reply = crate::ipc::request(&core.paths.socket, crate::ipc::Command::UiStatus).await?;
        assert!(reply["device"].is_null());
        assert!(reply["snapshot"]["phone"]["device"].is_null());
        assert_eq!(reply["snapshot"]["phone"]["name"], "");
        assert!(reply["snapshot"]["generation"].is_null());
        let status = crate::ipc::request(&core.paths.socket, crate::ipc::Command::Status).await?;
        for key in ["paired_devices", "connected_devices", "paused_devices"] {
            assert_eq!(status[key], serde_json::json!([]));
        }
        stop.send_replace(true);
        ipc.await??;
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn cached_phone_metadata_is_bound_to_identity_after_restart() -> Result<()> {
        for cache in ["matching", "different", "legacy"] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let _connection = admit_ui_test_phone(&core, &device, 1)?;
            core.persist_phone(1).await?;
            let paths = core.paths.clone();
            let config = core.config();
            core.shutdown_actions().await?;
            drop(core);
            let file = paths.data.join("phone.json");
            let mut phone: serde_json::Value = serde_json::from_slice(&std::fs::read(&file)?)?;
            if cache == "legacy" {
                phone.as_object_mut().unwrap().remove("device");
            }
            if cache == "different" {
                phone["device"] = serde_json::to_value(Id::from_bytes([9; 16]))?;
            }
            crate::config::atomic_private(&file, &serde_json::to_vec(&phone)?)?;
            let restarted = Core::new(paths, config, Arc::new(NoActions)).await?;
            let snapshot = restarted.ui_snapshot();
            assert_eq!(snapshot.device, Some(device));
            assert_eq!(snapshot.phone.device, snapshot.device);
            assert_eq!(
                snapshot.phone.name,
                if cache == "matching" {
                    "Phone 1"
                } else {
                    "Phone"
                }
            );
            assert_eq!(snapshot.phone.last_seen != 0, cache == "matching");
            assert!(snapshot.generation.is_none());
            assert!(snapshot.paired && !snapshot.connected && !snapshot.paused);
            restarted.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn only_resolved_error_classes_are_cleared() -> Result<()> {
        use crate::ui::{ReceiveDestination as D, ReceiveError};
        let (dir, core, _) = fixture(Arc::new(NoActions)).await?;
        let device = Id::new(crate::random_hex(16))?;
        core.set_receive_error(Some(ReceiveError::Port(47821)));
        core.change_save_folder(dir.path().join("new-folder"))
            .await?;
        core.connection_storage_result(
            &core.store,
            &device,
            D::SaveFolder,
            &core.config().save_path,
            Some(NackCode::NotWritable),
        );
        assert_eq!(core.ui_snapshot().error, Some(ReceiveError::Port(47821)));
        core.clear_port_error();
        for code in [NackCode::NotWritable, NackCode::DiskFull] {
            let current = core.config().save_path;
            core.connection_storage_result(
                &core.store,
                &device,
                D::SaveFolder,
                &current,
                Some(code),
            );
            let error = core.ui_snapshot().error;
            assert!(error.is_some());
            core.clear_port_error();
            assert_eq!(core.ui_snapshot().error, error);
            core.connection_storage_result(&core.store, &device, D::Cache, &core.paths.cache, None);
            assert_eq!(core.ui_snapshot().error, error);
            let mut obsolete = core.store.clone();
            obsolete.connection_id = Some(99);
            core.connection_storage_result(&obsolete, &device, D::SaveFolder, &current, None);
            assert_eq!(core.ui_snapshot().error, error);
            core.change_save_folder(dir.path().join(crate::random_hex(8)))
                .await?;
            assert!(core.ui_snapshot().error.is_none());
        }
        let folder = core.config().save_path;
        core.connection_storage_result(
            &core.store,
            &device,
            D::SaveFolder,
            &folder,
            Some(NackCode::NotWritable),
        );
        core.toggle_setting(crate::ui::Toggle::Save).await?;
        assert!(core.ui_snapshot().error.is_none());
        core.connection_storage_result(
            &core.store,
            &device,
            D::SaveFolder,
            &folder,
            Some(NackCode::DiskFull),
        );
        assert!(core.ui_snapshot().error.is_none());
        core.connection_storage_result(
            &core.store,
            &device,
            D::Cache,
            &core.paths.cache,
            Some(NackCode::NotWritable),
        );
        let error = core.ui_snapshot().error;
        core.change_save_folder(dir.path().join("another-folder"))
            .await?;
        assert_eq!(core.ui_snapshot().error, error);
        core.connection_storage_result(&core.store, &device, D::Cache, &core.paths.cache, None);
        assert!(core.ui_snapshot().error.is_none());
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn failed_cache_arrivals_and_recovery_use_actual_destination() -> Result<()> {
        use crate::ui::{ReceiveDestination as D, ReceiveError};
        for mode in 0..4 {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            if mode == 0 {
                core.toggle_setting(crate::ui::Toggle::Save).await?;
            }
            if mode == 2 {
                let folder = core.config().save_path;
                if folder.exists() {
                    std::fs::remove_dir_all(&folder)?;
                }
                std::fs::write(folder, b"folder unavailable")?;
            }
            std::fs::remove_dir_all(&core.paths.cache)?;
            std::fs::write(&core.paths.cache, b"cache unavailable")?;
            let (tx, mut rx) = mpsc::channel(8);
            for recovered in [false, true] {
                if recovered {
                    std::fs::remove_file(&core.paths.cache)?;
                    if mode == 3 {
                        core.toggle_setting(crate::ui::Toggle::Save).await?;
                    }
                }
                let mut begin = test_begin(b"bytes");
                if mode == 1 {
                    begin.kind = FileKind::ClipboardImage;
                }
                let id = begin.transfer_id.clone();
                let mut incoming: Option<Incoming> = None;
                for message in [
                    Message::FileBegin(begin),
                    Message::FileChunk(pneu_proto::FileChunk {
                        transfer_id: id.clone(),
                        offset: 0,
                        data: b"bytes".to_vec(),
                    }),
                    Message::FileEnd(pneu_proto::FileEnd { transfer_id: id }),
                ] {
                    if mode == 3 && !recovered && matches!(message, Message::FileChunk(_)) {
                        incoming.as_mut().expect("begun transfer").file = Some(
                            tokio::fs::OpenOptions::new()
                                .write(true)
                                .open("/dev/full")
                                .await?,
                        );
                    }
                    core.receive(&core.store, None, &device, message, &mut incoming, &tx)
                        .await?;
                }
                let reply = rx.recv().await.context("transfer reply")?;
                if !recovered {
                    assert!(matches!(
                        reply,
                        Message::Nack(Nack {
                            code: NackCode::NotWritable,
                            ..
                        })
                    ));
                    assert!(
                        matches!(core.ui_snapshot().error, Some(ReceiveError::Storage { destination: D::Cache, ref path, .. }) if *path == crate::ui::display_path(&core.paths.cache))
                    );
                } else if mode == 2 {
                    assert!(matches!(reply, Message::Nack(_)));
                    assert!(matches!(
                        core.ui_snapshot().error,
                        Some(ReceiveError::Storage {
                            destination: D::SaveFolder,
                            ..
                        })
                    ));
                } else {
                    assert!(matches!(reply, Message::Ack(_)));
                    assert!(core.ui_snapshot().error.is_none());
                }
            }
            core.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn final_phone_metadata_persists_only_for_latest_generation() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _old = admit_ui_test_phone(&core, &device, 1)?;
        core.ui.lock().unwrap().snapshot.phone.last_seen = 1;
        core.persist_phone(1).await?;
        assert!(core.finish_connection(&device, 1));
        let ended = core.ui_snapshot().phone.last_seen;
        core.persist_phone(1).await?;
        let path = core.paths.data.join("phone.json");
        let phone: crate::ui::Phone = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(phone.last_seen, ended);
        assert!(ended > 1);
        let _new = admit_ui_test_phone(&core, &device, 2)?;
        core.persist_phone(2).await?;
        let before = std::fs::read(&path)?;
        assert!(!core.finish_connection(&device, 1));
        core.persist_phone(1).await?;
        assert_eq!(std::fs::read(&path)?, before);
        core.unpair_device(device).await?;
        assert!(core.ui.lock().unwrap().phone_generation.is_none());
        core.persist_phone(2).await?;
        assert_eq!(
            serde_json::from_slice::<crate::ui::Phone>(&std::fs::read(path)?)?.name,
            "Phone 2"
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn obsolete_admission_cannot_publish_or_persist_phone_metadata() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        core.admission.lock().unwrap().peers = core.store.peers().await?;
        let (close, _rx) = watch::channel(None);
        core.sessions.lock().unwrap().session = Some(Session {
            device: device.clone(),
            id: 2,
            close,
            wake: Arc::new(Notify::new()),
            paused: false,
        });
        let current = Hello {
            version: pneu_proto::PROTOCOL_VERSION,
            device_id: device.clone(),
            name: "Current phone".into(),
            capabilities: vec!["clipboard_image".into()],
            paused: Some(false),
        };
        let old = Hello {
            name: "Obsolete phone".into(),
            capabilities: vec!["clipboard_text".into()],
            ..current.clone()
        };
        {
            let state = core.sessions.lock().unwrap();
            assert!(core.publish_phone(&state, 2, &current, "100.64.0.1".parse()?));
            assert!(!core.publish_phone(&state, 1, &old, "192.168.0.1".parse()?));
        }
        core.persist_phone(2).await?;
        let before = std::fs::read(core.paths.data.join("phone.json"))?;
        core.persist_phone(1).await?;
        assert_eq!(before, std::fs::read(core.paths.data.join("phone.json"))?);
        assert_eq!(core.ui_snapshot().phone.name, "Current phone");
        assert_eq!(core.ui_snapshot().phone.route, "Tailscale");
        assert!(core.clipboard_image_capable());
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn production_ui_policy_cleanup_preserves_explicit_outbox() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let fp = Hash::new(crate::random_hex(32))?;
        core.admission
            .lock()
            .unwrap()
            .peers
            .insert(fp, device.clone());
        let (close, _receiver) = watch::channel(None);
        core.sessions.lock().unwrap().session = Some(Session {
            device: device.clone(),
            id: 7,
            close,
            wake: Arc::new(Notify::new()),
            paused: false,
        });
        let explicit = core
            .send_clipboard(device.clone(), "explicit".into())
            .await?;
        for event in [
            Event::PhonePaused {
                device: device.clone(),
                paused: true,
            },
            Event::ConfigChanged,
            Event::ConnectionChanged {
                device: device.clone(),
                connected: false,
            },
        ] {
            if matches!(event, Event::PhonePaused { .. }) {
                core.set_phone_paused(&device, 7, true);
            }
            if matches!(event, Event::ConfigChanged) {
                core.toggle_setting(crate::ui::Toggle::Clipboard).await?;
            }
            if matches!(event, Event::ConnectionChanged { .. }) {
                core.finish_connection(&device, 7);
            }
            crate::ui::runtime::clipboard_policy_event(&core, &event);
            assert_eq!(
                core.store
                    .first_outgoing(device.clone())
                    .await?
                    .unwrap()
                    .1
                    .transfer_id,
                explicit
            );
        }
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn clipboard_observation_ignores_paused_offers_during_slow_reads() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let (close, _receiver) = watch::channel(None);
        core.sessions.lock().unwrap().session = Some(Session {
            device: device.clone(),
            id: 7,
            close,
            wake: Arc::new(Notify::new()),
            paused: false,
        });
        crate::ui::clipboard::tests::delayed_source_pause_resume(&core, |paused| {
            core.set_phone_paused(&device, 7, paused)
        })
        .await?;
        let current = core
            .take_automatic(&device, 7)
            .expect("resumed latest offer");
        let OutgoingSelection::Text(text) = current.selection else {
            panic!("expected text");
        };
        assert_eq!(text.text, "latest");
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn full_connection_queue_waits_for_capacity() -> Result<()> {
        let (tx, mut rx) = mpsc::channel(1);
        let (_close, mut closed) = watch::channel(None);
        tx.send(Message::Status(Status { paused: true })).await?;
        let queued = enqueue(
            &tx,
            Message::Status(Status { paused: false }),
            &mut closed,
            None,
        );
        tokio::pin!(queued);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut queued)
                .await
                .is_err()
        );
        assert!(matches!(
            rx.recv().await,
            Some(Message::Status(Status { paused: true }))
        ));
        assert!(queued.await?);
        assert!(matches!(
            rx.recv().await,
            Some(Message::Status(Status { paused: false }))
        ));
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn full_connection_queue_still_honors_close_and_receipt_deadline() -> Result<()> {
        let (tx, mut rx) = mpsc::channel(1);
        let (close, mut closed) = watch::channel(None);
        tx.send(Message::Status(Status { paused: true })).await?;
        let queued = enqueue(
            &tx,
            Message::Status(Status { paused: false }),
            &mut closed,
            None,
        );
        tokio::pin!(queued);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut queued)
                .await
                .is_err()
        );
        close.send(Some(CloseReason::Error(ErrorCode::Closing)))?;
        assert!(!queued.await?);
        let (_close, mut closed) = watch::channel(None);
        let error = enqueue(
            &tx,
            Message::Status(Status { paused: false }),
            &mut closed,
            Some(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap_err();
        assert!(error.is::<tokio::time::error::Elapsed>());
        assert!(matches!(
            rx.recv().await,
            Some(Message::Status(Status { paused: true }))
        ));
        assert!(rx.try_recv().is_err());
        Ok(())
    }
    #[tokio::test]
    async fn streaming_file_backpressures_slow_storage() -> Result<()> {
        let (dir, core, _) = fixture(Arc::new(NoActions)).await?;
        *core.file_write_delay.lock().expect("file write delay") = Duration::from_millis(25);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let server = tokio::spawn(core.clone().serve(listener, false));
        let phone = crate::client::Phone::new(dir.path().join("phone"))?;
        let invitation = core.start_pairing_with_addresses(vec![address]).await?;
        let mut connection = phone.pair(&invitation).await?;
        // More than the combined reader/file queue capacity; localhost streams
        // much faster than the mocked 25 ms per 256 KiB storage write.
        let bytes = vec![0xa5; 8 * 1024 * 1024];
        let begin = test_begin(&bytes);
        let outcome: Result<()> = async {
            connection.write(&Message::FileBegin(begin.clone())).await?;
            for (index, data) in bytes.chunks(CHUNK_SIZE).enumerate() {
                connection.write(&Message::FileChunk(FileChunk {
                    transfer_id: begin.transfer_id.clone(),
                    offset: (index * CHUNK_SIZE) as u64,
                    data: data.to_vec(),
                })).await?;
            }
            connection.write(&Message::FileEnd(FileEnd {
                transfer_id: begin.transfer_id.clone(),
            })).await?;
            ensure!(matches!(connection.read(Duration::from_secs(10)).await?,
                Message::Ack(Ack { transfer_id, deduplicated: false }) if transfer_id == begin.transfer_id),
                "stream did not receive a durable ACK");
            let saved = std::fs::read_dir(core.config().save_path)?
                .collect::<std::io::Result<Vec<_>>>()?.into_iter().filter(|e| e.path().is_file()).collect::<Vec<_>>();
            ensure!(saved.len() == 1, "expected exactly one received file");
            ensure!(tokio::fs::read(saved[0].path()).await? == bytes, "received bytes differ");
            ensure!(core.store.receipt(phone.identity.device_id.clone(), begin.transfer_id).await?
                .is_some_and(|(_, complete)| complete), "receipt is not durable");
            Ok(())
        }.await;
        drop(connection);
        core.request_shutdown();
        tokio::time::timeout(Duration::from_secs(5), server).await???;
        outcome
    }
    #[tokio::test]
    async fn clipboard_retry_overdue_during_file_backpressure_is_delivered_without_another_wake()
    -> Result<()> {
        let (dir, core, _) = fixture(Arc::new(NoActions)).await?;
        *core.file_write_delay.lock().expect("file write delay") = Duration::from_millis(50);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let server = tokio::spawn(core.clone().serve(listener, false));
        let phone = crate::client::Phone::new(dir.path().join("phone"))?;
        let invitation = core.start_pairing_with_addresses(vec![address]).await?;
        let original = phone.pair(&invitation).await?;
        let text = phone.text("retry after rejection".into(), ClipboardOrigin::Clipboard)?;
        core.store
            .enqueue(phone.identity.device_id.clone(), text.clone())
            .await?;
        core.store
            .call(|c| {
                c.execute(
                    "UPDATE outgoing SET retry_at=?",
                    [(crate::now_ms() + 500) as i64],
                )?;
                Ok(())
            })
            .await?;
        drop(original);
        let mut connection = phone.reconnect().await?;
        // Let READY schedule the persisted retry before filling its dispatch
        // queues. No enqueue/status/config event wakes it during the transfer.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let bytes = vec![0xa5; 8 * 1024 * 1024];
        let begin = test_begin(&bytes);
        let outcome: Result<()> = async {
            connection.write(&Message::FileBegin(begin.clone())).await?;
            for (index, data) in bytes.chunks(CHUNK_SIZE).enumerate() {
                connection
                    .write(&Message::FileChunk(FileChunk {
                        transfer_id: begin.transfer_id.clone(),
                        offset: (index * CHUNK_SIZE) as u64,
                        data: data.to_vec(),
                    }))
                    .await?;
            }
            connection
                .write(&Message::FileEnd(FileEnd {
                    transfer_id: begin.transfer_id.clone(),
                }))
                .await?;
            let (mut retried, mut saved) = (false, false);
            while !retried || !saved {
                match connection.read(Duration::from_secs(4)).await? {
                    Message::Clipboard(delivery) if delivery == text => {
                        connection
                            .write(&Message::Ack(Ack {
                                transfer_id: delivery.transfer_id,
                                deduplicated: false,
                            }))
                            .await?;
                        retried = true;
                    }
                    Message::Ack(ack) if ack.transfer_id == begin.transfer_id => saved = true,
                    _ => bail!("unexpected frame during retry"),
                }
            }
            Ok(())
        }
        .await;
        drop(connection);
        core.request_shutdown();
        tokio::time::timeout(Duration::from_secs(5), server).await???;
        outcome
    }
    #[tokio::test(start_paused = true)]
    async fn overdue_retry_fires_after_a_blocked_dispatch_without_spinning_when_idle() -> Result<()>
    {
        let (tx, mut rx) = mpsc::channel(1);
        let (_close, mut closed) = watch::channel(None);
        tx.send(Message::Status(Status { paused: true })).await?;
        let deadline = Instant::now() + Duration::from_secs(1);
        let dispatch = enqueue(
            &tx,
            Message::Status(Status { paused: false }),
            &mut closed,
            None,
        );
        tokio::pin!(dispatch);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), &mut dispatch)
                .await
                .is_err()
        );
        rx.recv().await;
        assert!(dispatch.await?);
        let now = Instant::now();
        tokio::time::timeout(Duration::from_millis(1), retry_timer(Some(deadline))).await?;
        assert_eq!(now, Instant::now());
        assert!(
            tokio::time::timeout(Duration::from_secs(1), retry_timer(None))
                .await
                .is_err()
        );
        Ok(())
    }
    struct NoActions;
    struct CountingActions(AtomicU64);
    #[async_trait::async_trait]
    impl PcActions for CountingActions {
        async fn clipboard_text(&self, _: &str) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn clipboard_image(&self, _: &std::path::Path, _: &str) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn notify(&self, _: &std::path::Path, _: &str) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn notify_clipboard_failure(&self) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn open(&self, _: &std::path::Path) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[async_trait::async_trait]
    impl PcActions for NoActions {
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
    struct PathActions(mpsc::UnboundedSender<(&'static str, PathBuf)>);
    #[async_trait::async_trait]
    impl PcActions for PathActions {
        async fn clipboard_text(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn clipboard_image(&self, _: &std::path::Path, _: &str) -> Result<()> {
            Ok(())
        }
        async fn notify(&self, path: &std::path::Path, _: &str) -> Result<()> {
            self.0.send(("notify", path.into()))?;
            Ok(())
        }
        async fn open(&self, path: &std::path::Path) -> Result<()> {
            self.0.send(("open", path.into()))?;
            Ok(())
        }
    }
    #[tokio::test]
    async fn readable_save_names_propagate_to_receipts_events_and_actions_on_replay() -> Result<()>
    {
        let (actions, mut observed) = mpsc::unbounded_channel();
        let (_dir, core, device) = fixture(Arc::new(PathActions(actions))).await?;
        let mut events = core.subscribe();
        let mut config = core.config();
        config.open_in_viewer = true;
        config.copy_image_to_clipboard = false;
        config.notification_preview = true;
        core.update_config(config).await?;
        let bytes = b"\x89PNG\r\n\x1a\nreceived";
        let mut begin = test_begin(bytes);
        begin.file_name = "写真.png".into();
        begin.mime = "image/png".into();
        let folder = core.config().save_path;
        let (tx, mut rx) = mpsc::channel(2);
        // First transfer keeps its name; a different ID collides; its replay
        // uses the durable receipt and does not place or act on another file.
        for (index, expected) in ["写真.png", "写真 (2).png", "写真 (2).png"]
            .into_iter()
            .enumerate()
        {
            if index == 1 {
                begin.transfer_id = Id::new(crate::random_hex(16))?;
            }
            let mut incoming = None;
            for message in [
                Message::FileBegin(begin.clone()),
                Message::FileChunk(FileChunk {
                    transfer_id: begin.transfer_id.clone(),
                    offset: 0,
                    data: bytes.to_vec(),
                }),
                Message::FileEnd(FileEnd {
                    transfer_id: begin.transfer_id.clone(),
                }),
            ] {
                core.receive_test(&core.store, &device, message, &mut incoming, &tx)
                    .await?;
            }
            assert!(
                matches!(rx.recv().await, Some(Message::Ack(Ack { deduplicated, .. })) if deduplicated == (index == 2))
            );
            let expected_path = folder.join(expected);
            let id = begin.transfer_id.clone();
            let peer = device.clone();
            let receipt_path: String = core
                .store
                .call(move |c| {
                    Ok(c.query_row(
                        "SELECT destination FROM receipts WHERE device=? AND id=?",
                        rusqlite::params![peer.as_str(), id.as_str()],
                        |r| r.get(0),
                    )?)
                })
                .await?;
            assert_eq!(PathBuf::from(receipt_path), expected_path);
            let mut finished = false;
            while let Ok(event) = events.try_recv() {
                if let Event::TransferFinished {
                    path, deduplicated, ..
                } = event
                {
                    finished = true;
                    assert_eq!(deduplicated, index == 2);
                    assert_eq!(path, (index != 2).then_some(expected_path.clone()));
                }
            }
            assert!(finished, "missing transfer completion event");
            if index != 2 {
                let mut effects = Vec::new();
                for _ in 0..2 {
                    let (effect, path) =
                        tokio::time::timeout(Duration::from_secs(5), observed.recv())
                            .await?
                            .context("desktop action")?;
                    assert_eq!(path, expected_path);
                    effects.push(effect);
                }
                effects.sort_unstable();
                assert_eq!(effects, ["notify", "open"]);
            }
            assert_eq!(std::fs::read(expected_path)?, bytes);
        }
        core.shutdown_actions().await?;
        assert!(observed.try_recv().is_err());
        assert_eq!(
            std::fs::read_dir(folder)?
                .filter(|entry| entry.as_ref().is_ok_and(|entry| entry.path().is_file()))
                .count(),
            2
        );
        Ok(())
    }
    #[tokio::test]
    async fn unavailable_folders_load_and_late_cache_overlap_falls_back_without_save_receipt()
    -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for moved_after_begin in [false, true] {
            // Root bypasses mode bits; the mid-transfer move still runs there.
            if !moved_after_begin && rustix::process::geteuid().as_raw() == 0 {
                continue;
            }
            let dir = tempfile::tempdir()?;
            let blocked = dir.path().join("blocked");
            let cache = if moved_after_begin {
                dir.path().join("cache")
            } else {
                blocked.join("cache")
            };
            std::fs::create_dir_all(&cache)?;
            let config = Config {
                save_path: if moved_after_begin {
                    dir.path().join("saved")
                } else {
                    cache.clone()
                },
                ..Config::default()
            };
            let config_path = dir.path().join("config.toml");
            if !moved_after_begin {
                std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000))?;
                assert_eq!(
                    std::fs::symlink_metadata(&cache).unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
            }
            let startup: Result<_> = async {
                config.save(&config_path)?;
                let loaded = Config::load(&config_path)?;
                assert_eq!(loaded.save_path, config.save_path);
                let core = Core::new(
                    Paths {
                        data: dir.path().join("data"),
                        cache: cache.clone(),
                        config: config_path.clone(),
                        socket: dir.path().join("socket"),
                    },
                    loaded,
                    Arc::new(NoActions),
                )
                .await?;
                let mut settings = core.config();
                settings.notification_preview = false;
                core.update_config(settings).await?;
                assert!(!core.config().notification_preview);
                assert!(!Config::load(&config_path)?.notification_preview);
                Ok(core)
            }
            .await;
            // Restore access even when an assertion-independent startup error occurs.
            if !moved_after_begin {
                std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700))?;
            }
            let core = startup?;
            let device = Id::new(crate::random_hex(16))?;
            core.store
                .pair(device.clone(), Hash::new(crate::random_hex(32))?)
                .await?;
            let bytes = b"received bytes";
            let mut begin = test_begin(bytes);
            begin.file_name = "received-photo.jpg".into();
            let (tx, mut rx) = mpsc::channel(2);
            let mut incoming = None;
            for message in [
                Message::FileBegin(begin.clone()),
                Message::FileChunk(FileChunk {
                    transfer_id: begin.transfer_id.clone(),
                    offset: 0,
                    data: bytes.to_vec(),
                }),
                Message::FileEnd(FileEnd {
                    transfer_id: begin.transfer_id.clone(),
                }),
            ] {
                if moved_after_begin && matches!(message, Message::FileEnd(_)) {
                    std::fs::rename(core.config().save_path, cache.join("moved-save"))?;
                }
                core.receive_test(&core.store, &device, message, &mut incoming, &tx)
                    .await?;
            }
            assert!(matches!(
                rx.recv().await,
                Some(Message::Nack(Nack {
                    code: NackCode::NotWritable,
                    retryable: true,
                    ..
                }))
            ));
            assert!(
                core.store
                    .receipt(device, begin.transfer_id)
                    .await?
                    .is_none()
            );
            assert!(!cache.join("received-photo.jpg").exists());
            assert!(!cache.join("moved-save/received-photo.jpg").exists());
            let cached = std::fs::read_dir(&cache)?
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .filter(|entry| entry.path().is_file())
                .collect::<Vec<_>>();
            assert_eq!(cached.len(), 1);
            assert!(
                cached[0]
                    .file_name()
                    .to_string_lossy()
                    .starts_with(crate::storage::CACHE_PREFIX)
            );
            assert_eq!(std::fs::read(cached[0].path())?, bytes);
            core.shutdown_actions().await?;
            core.store.drain().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn file_end_commits_using_streamed_verification_without_reading_stage() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let bytes = vec![42; 3 * 1024 * 1024];
        let begin = test_begin(&bytes);
        let (tx, mut rx) = mpsc::channel(2);
        let mut incoming = None;
        core.receive_test(
            &core.store,
            &device,
            Message::FileBegin(begin.clone()),
            &mut incoming,
            &tx,
        )
        .await?;
        for (n, data) in bytes.chunks(CHUNK_SIZE).enumerate() {
            core.receive_test(
                &core.store,
                &device,
                Message::FileChunk(FileChunk {
                    transfer_id: begin.transfer_id.clone(),
                    offset: (n * CHUNK_SIZE) as u64,
                    data: data.to_vec(),
                }),
                &mut incoming,
                &tx,
            )
            .await?;
        }
        let stage = incoming.as_mut().unwrap().stage.as_mut().unwrap();
        let destination = stage.destination_path()?;
        let inner = Arc::get_mut(&mut stage.0).unwrap();
        // Reading this descriptor is EBADF. fstat, fsync and anchored placement
        // work, proving FILE_END never runs another content pass.
        inner.file = std::fs::File::from(rustix::fs::openat(
            &*inner.directory.file,
            &inner.name,
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?);
        core.receive_test(
            &core.store,
            &device,
            Message::FileEnd(FileEnd {
                transfer_id: begin.transfer_id.clone(),
            }),
            &mut incoming,
            &tx,
        )
        .await?;
        assert!(matches!(
            rx.recv().await,
            Some(Message::Ack(Ack {
                deduplicated: false,
                ..
            }))
        ));
        assert_eq!(std::fs::read(destination)?, bytes);
        assert!(
            core.store
                .receipt(device, begin.transfer_id)
                .await?
                .is_some()
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn unavailable_historical_folder_and_optional_cache_do_not_prevent_startup() -> Result<()>
    {
        let (dir, core, _) = fixture(Arc::new(NoActions)).await?;
        let history = dir.path().join("historical-volume");
        let stage = core
            .store
            .create_stage(history.clone(), "unfinished".into(), 1)
            .await?;
        drop(stage);
        core.store.drain().await;
        let healthy = core.config().save_path;
        let stage = core
            .store
            .create_stage(healthy.clone(), "temporary".into(), 1)
            .await?;
        drop(stage);
        core.store.drain().await;
        std::fs::write(healthy.join(".pneu-staging/orphan"), b"crashed")?;
        let paths = core.paths.clone();
        let config = core.config();
        core.shutdown_actions().await?;
        drop(core);
        std::fs::rename(&history, dir.path().join("offline-volume"))?;
        std::fs::write(&history, b"unavailable volume placeholder")?;
        std::fs::remove_dir_all(&paths.cache)?;
        std::fs::write(&paths.cache, b"unavailable cache placeholder")?;
        let core = Core::new(paths.clone(), config, Arc::new(NoActions)).await?;
        assert!(
            !healthy.join(".pneu-staging/orphan").exists(),
            "historical failure stopped healthy cleanup"
        );
        assert!(
            core.store
                .create_stage(history.clone(), "new".into(), 1)
                .await
                .is_err()
        );
        let saved = core
            .store
            .create_stage(core.config().save_path, "new".into(), 1)
            .await?;
        drop(saved);
        core.store.drain().await;
        // Recover without a restart. The deferred root is swept before reuse.
        std::fs::remove_file(&history)?;
        std::fs::rename(dir.path().join("offline-volume"), &history)?;
        std::fs::write(history.join(".pneu-staging/orphan"), b"crashed")?;
        let stage = core
            .store
            .create_stage(history.clone(), "recovered".into(), 1)
            .await?;
        assert!(!history.join(".pneu-staging/orphan").exists());
        let live = stage.path()?;
        let second = core
            .store
            .create_stage(history.clone(), "also-recovered".into(), 1)
            .await?;
        assert!(live.exists(), "first use swept an active transfer");
        drop(second);
        drop(stage);
        std::fs::remove_file(&paths.cache)?;
        // Optional cache preparation is retried privately when a cache transfer
        // is first attempted after the directory becomes available.
        let mut begin = test_begin(b"image");
        begin.kind = FileKind::ClipboardImage;
        let mut state = Incoming {
            begin,
            received: 0,
            hash: Sha256::new(),
            file: None,
            stage: None,
            destination: None,
            receipt: None,
            save_error: None,
            storage_kind: crate::ui::ReceiveDestination::Cache,
            deduplicated: false,
            config: core.config(),
        };
        core.prepare_incoming(&core.store, &mut state).await;
        assert!(state.receipt.is_none());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&paths.cache)?.permissions().mode() & 0o777,
            0o700
        );
        drop(state);
        core.store.drain().await;
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn failed_optional_mdns_reports_error_without_disabling_listener() -> Result<()> {
        let (_dir, core, _) = fixture(Arc::new(NoActions)).await?;
        let mut events = core.subscribe();
        // No multicast side effects are needed to exercise the same result
        // handling used on startup and discovery refresh.
        assert!(
            core.optional(
                Advertisement::new_with_addresses(&core, Vec::new()),
                "mDNS advertisement"
            )
            .is_none()
        );
        assert!(matches!(events.try_recv()?, Event::Error { .. }));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(core.clone().serve(listener, false));
        let connection = TcpStream::connect(address).await?;
        drop(connection);
        core.request_shutdown();
        tokio::time::timeout(Duration::from_secs(3), server).await???;
        Ok(())
    }
    #[tokio::test]
    async fn disk_full_on_flush_nacks_and_retains_cache_without_receipt() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let core = Core::new(
            Paths {
                data: dir.path().join("data"),
                cache: dir.path().join("cache"),
                config: dir.path().join("config/config.toml"),
                socket: dir.path().join("socket"),
            },
            Config {
                save_path: dir.path().join("saved"),
                ..Config::default()
            },
            Arc::new(NoActions),
        )
        .await?;
        let device = Id::new(crate::random_hex(16))?;
        core.store
            .pair(device.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        let bytes = b"original bytes";
        let begin = FileBegin {
            transfer_id: Id::new(crate::random_hex(16))?,
            file_name: "file.bin".into(),
            mime: "application/octet-stream".into(),
            size: bytes.len() as u64,
            sha256: Hash::new(hex::encode(Sha256::digest(bytes)))?,
            kind: FileKind::File,
            created_at: 0,
            source_media_id: None,
        };
        let (tx, mut rx) = mpsc::channel(2);
        let mut incoming = None;
        core.receive_test(
            &core.store,
            &device,
            Message::FileBegin(begin.clone()),
            &mut incoming,
            &tx,
        )
        .await?;
        let state = incoming.as_mut().unwrap();
        let stage = state.stage.clone().unwrap();
        // /dev/full gives a real ENOSPC without exhausting a shared filesystem.
        // Keep the real staging file so cleanup proves it removes the copy.
        state.file = Some(
            tokio::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .await?,
        );
        core.receive_test(
            &core.store,
            &device,
            Message::FileChunk(FileChunk {
                transfer_id: begin.transfer_id.clone(),
                offset: 0,
                data: bytes.to_vec(),
            }),
            &mut incoming,
            &tx,
        )
        .await?;
        core.receive_test(
            &core.store,
            &device,
            Message::FileEnd(FileEnd {
                transfer_id: begin.transfer_id.clone(),
            }),
            &mut incoming,
            &tx,
        )
        .await?;
        assert!(matches!(
            rx.recv().await,
            Some(Message::Nack(Nack {
                code: NackCode::DiskFull,
                retryable: true,
                ..
            }))
        ));
        assert!(!stage.path()?.exists());
        let cached: Vec<_> = std::fs::read_dir(&core.paths.cache)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .collect();
        assert_eq!(cached.len(), 1);
        assert_eq!(std::fs::read(cached[0].path())?, bytes);
        assert!(
            std::fs::read_dir(core.config().save_path)?.all(|e| e.is_ok_and(|e| e.path().is_dir()))
        );
        assert!(
            core.store
                .receipt(device.clone(), begin.transfer_id.clone())
                .await?
                .is_none()
        );
        assert!(incoming.is_none());
        // Retry keeps its ID and succeeds after storage becomes writable.
        core.receive_test(
            &core.store,
            &device,
            Message::FileBegin(begin.clone()),
            &mut incoming,
            &tx,
        )
        .await?;
        core.receive_test(
            &core.store,
            &device,
            Message::FileChunk(FileChunk {
                transfer_id: begin.transfer_id.clone(),
                offset: 0,
                data: bytes.to_vec(),
            }),
            &mut incoming,
            &tx,
        )
        .await?;
        core.receive_test(
            &core.store,
            &device,
            Message::FileEnd(FileEnd {
                transfer_id: begin.transfer_id,
            }),
            &mut incoming,
            &tx,
        )
        .await?;
        assert!(matches!(
            rx.recv().await,
            Some(Message::Ack(Ack {
                deduplicated: false,
                ..
            }))
        ));
        Ok(())
    }
    async fn fixture(actions: Arc<dyn PcActions>) -> Result<(tempfile::TempDir, Arc<Core>, Id)> {
        let dir = tempfile::tempdir()?;
        let core = Core::new(
            Paths {
                data: dir.path().join("data"),
                cache: dir.path().join("cache"),
                config: dir.path().join("config/config.toml"),
                socket: dir.path().join("socket"),
            },
            Config {
                save_path: dir.path().join("saved"),
                ..Config::default()
            },
            actions,
        )
        .await?;
        let device = Id::new(crate::random_hex(16))?;
        core.store
            .pair(device.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        *core.action_scope.lock().unwrap() = Some((device.clone(), ActionScope::new()));
        Ok((dir, core, device))
    }
    fn test_begin(bytes: &[u8]) -> FileBegin {
        FileBegin {
            transfer_id: Id::new(crate::random_hex(16)).unwrap(),
            file_name: "file.bin".into(),
            mime: "application/octet-stream".into(),
            size: bytes.len() as u64,
            sha256: Hash::new(hex::encode(Sha256::digest(bytes))).unwrap(),
            kind: FileKind::File,
            created_at: 0,
            source_media_id: None,
        }
    }
    #[tokio::test]
    async fn canceled_chunk_flush_retains_cleanup_ownership() -> Result<()> {
        use std::io::Read;
        let (dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let bytes = vec![42; CHUNK_SIZE];
        let begin = test_begin(&bytes);
        let (tx, _rx) = mpsc::channel(2);
        let mut incoming = None;
        core.receive_test(
            &core.store,
            &device,
            Message::FileBegin(begin.clone()),
            &mut incoming,
            &tx,
        )
        .await?;
        let stage_path = incoming.as_ref().unwrap().stage.as_ref().unwrap().path()?;
        let fifo = dir.path().join("blocked-io");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )?;
        let pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)?;
        let mut drain = pipe.try_clone()?;
        incoming.as_mut().unwrap().file = Some(tokio::fs::File::from_std(pipe));
        // Each chunk now waits for its blocking write, so cancellation happens
        // at the chunk flush instead of deferring that write until FILE_END.
        let timed_out = tokio::time::timeout(
            Duration::from_millis(20),
            core.receive_test(
                &core.store,
                &device,
                Message::FileChunk(FileChunk {
                    transfer_id: begin.transfer_id.clone(),
                    offset: 0,
                    data: bytes.clone(),
                }),
                &mut incoming,
                &tx,
            ),
        )
        .await
        .is_err();
        let retained = incoming.is_some();
        drop(incoming);
        core.store.drain().await;
        let cleaned = !stage_path.exists();
        // Release and join the actual blocking FIFO write before any assertion.
        tokio::task::spawn_blocking(move || {
            let mut got = vec![0; bytes.len()];
            drain.read_exact(&mut got)?;
            anyhow::ensure!(got == bytes, "FIFO bytes");
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        assert!(timed_out && retained && cleaned);
        assert!(
            core.store
                .receipt(device, begin.transfer_id)
                .await?
                .is_none()
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    struct BlockingText(tokio::sync::Notify);
    #[async_trait::async_trait]
    impl PcActions for BlockingText {
        async fn clipboard_text(&self, _: &str) -> Result<()> {
            self.0.notify_one();
            std::future::pending().await
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
    async fn file_progress_deadline_runs_while_clipboard_is_blocked() -> Result<()> {
        let actions = Arc::new(BlockingText(tokio::sync::Notify::new()));
        let (_dir, core, device) = fixture(actions.clone()).await?;
        let (writer, _rx) = mpsc::channel(4);
        let text_core = core.clone();
        let text_device = device.clone();
        let text_writer = writer.clone();
        let text = tokio::spawn(async move {
            text_core
                .receive_test(
                    &text_core.store,
                    &text_device,
                    Message::Clipboard(Clipboard {
                        transfer_id: Id::new(crate::random_hex(16))?,
                        text: "blocked".into(),
                        created_at: 0,
                        origin: ClipboardOrigin::Clipboard,
                    }),
                    &mut None,
                    &text_writer,
                )
                .await
        });
        actions.0.notified().await;
        let mut events = core.subscribe();
        let (frames, messages) = mpsc::channel(4);
        let (close, closed) = watch::channel(None);
        let worker = tokio::spawn(core.clone().file_loop_test(
            core.store.clone(),
            device,
            messages,
            writer,
            close,
        ));
        frames
            .send(Message::FileBegin(test_begin(b"unfinished")))
            .await?;
        while !matches!(events.recv().await?, Event::TransferStarted { .. }) {}
        tokio::task::yield_now().await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        let result = worker.await?;
        let code = *closed.borrow();
        let text_still_blocked = !text.is_finished();
        text.abort();
        let _ = text.await;
        tokio::time::resume();
        core.shutdown_actions().await?;
        assert!(result.is_err());
        assert!(matches!(code, Some(CloseReason::Error(ErrorCode::Timeout))));
        assert!(text_still_blocked);
        assert!(
            std::fs::read_dir(core.config().save_path)?.all(|e| e.is_ok_and(|e| e.path().is_dir()))
        );
        Ok(())
    }
    #[tokio::test]
    async fn file_claim_wins_while_text_waits_for_clipboard_admission() -> Result<()> {
        let actions = Arc::new(CountingActions(AtomicU64::new(0)));
        let (_dir, core, device) = fixture(actions.clone()).await?;
        let mut config = core.config();
        config.notification_preview = false;
        core.update_config(config).await?;
        let application = core.clipboard_gate.lock().await;
        let begin = test_begin(b"file");
        let text = Clipboard {
            transfer_id: begin.transfer_id.clone(),
            text: "must not apply".into(),
            created_at: 1,
            origin: ClipboardOrigin::Clipboard,
        };
        let held = core.clone();
        let peer = device.clone();
        let (tx, mut rx) = mpsc::channel(8);
        let text_tx = tx.clone();
        let waiting = tokio::spawn(async move {
            held.receive_test(
                &held.store,
                &peer,
                Message::Clipboard(text),
                &mut None,
                &text_tx,
            )
            .await
        });
        tokio::task::yield_now().await;
        let mut incoming = None;
        for message in [
            Message::FileBegin(begin.clone()),
            Message::FileChunk(FileChunk {
                transfer_id: begin.transfer_id.clone(),
                offset: 0,
                data: b"file".to_vec(),
            }),
            Message::FileEnd(FileEnd {
                transfer_id: begin.transfer_id,
            }),
        ] {
            core.receive_test(&core.store, &device, message, &mut incoming, &tx)
                .await?;
        }
        assert!(matches!(rx.recv().await, Some(Message::Ack(_))));
        drop(application);
        waiting.await??;
        assert!(matches!(
            rx.recv().await,
            Some(Message::Nack(Nack {
                code: NackCode::IdConflict,
                ..
            }))
        ));
        assert_eq!(
            actions.0.load(Ordering::SeqCst),
            0,
            "conflicting text reached the OS"
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn ordinary_disconnect_keeps_admitted_text_effect_and_history_authority() -> Result<()> {
        let actions = Arc::new(CountingActions(AtomicU64::new(0)));
        let (_dir, core, device) = fixture(actions.clone()).await?;
        let scope = core
            .action_scope
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .clone();
        scope.connection_generation.store(1, Ordering::SeqCst);
        let (close, _closed) = watch::channel(None);
        core.sessions.lock().unwrap().session = Some(Session {
            device: device.clone(),
            id: 1,
            close,
            wake: Arc::new(Notify::new()),
            paused: false,
        });
        let mut store = core.store.with_current_pairing();
        store.connection_id = Some(1);
        let text = Clipboard {
            transfer_id: Id::new(crate::random_hex(16))?,
            text: "admitted text".into(),
            created_at: 1,
            origin: ClipboardOrigin::Clipboard,
        };
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let held = core.store.clone();
        let holder = tokio::spawn(async move {
            held.call(move |_| {
                let _ = entered.send(());
                blocked.recv()?;
                Ok(())
            })
            .await
        });
        wait.await?;
        let worker_core = core.clone();
        let worker_device = device.clone();
        let worker_text = text.clone();
        let worker_scope = scope.clone();
        let (tx, mut rx) = mpsc::channel(4);
        let worker = tokio::spawn(async move {
            worker_core
                .receive(
                    &store,
                    Some(&worker_scope),
                    &worker_device,
                    Message::Clipboard(worker_text),
                    &mut None,
                    &tx,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while core.clipboard_gate.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(core.finish_connection(&device, 1));
        assert_eq!(scope.connection_generation.load(Ordering::SeqCst), 1);
        release.send(())?;
        holder.await??;
        tokio::time::timeout(Duration::from_secs(2), worker).await???;
        assert_eq!(actions.0.load(Ordering::SeqCst), 1);
        assert!(matches!(
            rx.recv().await,
            Some(Message::Ack(Ack {
                deduplicated: false,
                ..
            }))
        ));
        assert!(
            core.store.text_is_stale(device, text).await?,
            "ordinary disconnect suppressed text history"
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn reconnect_rejects_blocked_old_text_and_applies_its_replay_once() -> Result<()> {
        let actions = Arc::new(CountingActions(AtomicU64::new(0)));
        let (dir, core, _) = fixture(actions.clone()).await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let server = tokio::spawn(core.clone().serve(listener, false));
        let phone = crate::client::Phone::new(dir.path().join("phone"))?;
        let invitation = core.start_pairing_with_addresses(vec![address]).await?;
        let mut events = core.subscribe();
        let mut original = phone.pair(&invitation).await?;
        while !matches!(
            events.recv().await?,
            Event::ConnectionChanged {
                connected: true,
                ..
            }
        ) {}
        let device = phone.identity.device_id.clone();
        let scope = core
            .action_scope
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .clone();
        let mut old_store = core.store.with_current_pairing();
        old_store.connection_id = Some(core.sessions.lock().unwrap().session.as_ref().unwrap().id);
        let text = phone.text("replayed operation".into(), ClipboardOrigin::Clipboard)?;
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let held = core.store.clone();
        let holder = tokio::spawn(async move {
            held.call(move |_| {
                let _ = entered.send(());
                blocked.recv()?;
                Ok(())
            })
            .await
        });
        wait.await?;
        // Model an old READY worker that cannot be joined yet, so connection
        // replacement alone cannot hide a missing side-effect authority check.
        let worker_core = core.clone();
        let worker_scope = scope.clone();
        let worker_text = text.clone();
        let worker_device = device.clone();
        let (tx, mut rx) = mpsc::channel(4);
        let worker = tokio::spawn(async move {
            worker_core
                .receive(
                    &old_store,
                    Some(&worker_scope),
                    &worker_device,
                    Message::Clipboard(worker_text),
                    &mut None,
                    &tx,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while core.clipboard_gate.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let mut replacement = phone.reconnect().await?;
        while !matches!(
            events.recv().await?,
            Event::ConnectionChanged {
                connected: true,
                ..
            }
        ) {}
        assert!(
            !*scope.cancelled.borrow(),
            "reconnect unexpectedly revoked the pairing scope"
        );
        release.send(())?;
        holder.await??;
        tokio::time::timeout(Duration::from_secs(2), worker).await???;
        assert_eq!(
            actions.0.load(Ordering::SeqCst),
            0,
            "replaced text reached the OS"
        );
        assert!(rx.recv().await.is_none());
        assert!(matches!(
            original.read(Duration::from_secs(2)).await?,
            Message::Error(ErrorMessage {
                code: ErrorCode::ConnectionReplaced,
                ..
            })
        ));
        let applied =
            tokio::time::timeout(Duration::from_secs(2), replacement.send_text(text.clone()))
                .await??;
        assert!(!applied.deduplicated);
        let replay =
            tokio::time::timeout(Duration::from_secs(2), replacement.send_text(text)).await??;
        assert!(replay.deduplicated);
        assert_eq!(actions.0.load(Ordering::SeqCst), 1);
        drop(original);
        drop(replacement);
        core.request_shutdown();
        tokio::time::timeout(Duration::from_secs(3), server).await???;
        Ok(())
    }
    #[tokio::test]
    async fn replaced_connection_gates_delayed_images_notifications_and_viewers() -> Result<()> {
        let actions = Arc::new(CountingActions(AtomicU64::new(0)));
        let (dir, core, device) = fixture(actions.clone()).await?;
        let scope = core
            .action_scope
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .clone();
        let (close, _closed) = watch::channel(None);
        core.sessions.lock().unwrap().session = Some(Session {
            device: device.clone(),
            id: 1,
            close,
            wake: Arc::new(Notify::new()),
            paused: false,
        });
        scope.connection_generation.store(1, Ordering::SeqCst);
        let job = ActionJob {
            file: ReceivedFile::from_file(dir.path().join("image.png"), tempfile::tempfile()?)?,
            scope: scope.clone(),
            origin: ActionOrigin {
                device,
                connection_id: Some(1),
            },
            path: dir.path().join("image.png"),
            metadata: FileBegin {
                kind: FileKind::ClipboardImage,
                ..test_begin(b"image")
            },
            config: Config {
                notification_preview: true,
                copy_image_to_clipboard: true,
                ..core.config()
            },
            arrived: Instant::now(),
            save_error: None,
        };
        let active = scope.active.lock().await;
        let worker_core = core.clone();
        let worker_job = job.clone();
        let worker = tokio::spawn(async move {
            worker_core
                .file_actions(&worker_job, true, DesktopAction::Explicit)
                .await
        });
        tokio::task::yield_now().await;
        core.sessions.lock().unwrap().session.as_mut().unwrap().id = 2;
        scope.connection_generation.store(2, Ordering::SeqCst);
        drop(active);
        worker.await?;
        core.file_actions(&job, true, DesktopAction::Automatic)
            .await;
        core.file_actions(&job, true, DesktopAction::Notification)
            .await;
        core.file_actions(&job, true, DesktopAction::CopyFailed)
            .await;
        run_action(
            &core,
            &job,
            &mut core.shutdown.subscribe(),
            DesktopAction::Viewer,
            true,
        )
        .await;
        assert_eq!(actions.0.load(Ordering::SeqCst), 0);
        let current = ActionJob {
            origin: ActionOrigin {
                connection_id: Some(2),
                ..job.origin.clone()
            },
            ..job
        };
        core.file_actions(&current, true, DesktopAction::Explicit)
            .await;
        assert_eq!(
            actions.0.load(Ordering::SeqCst),
            2,
            "current copy and notification were suppressed"
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn revoked_text_waiting_for_storage_never_reaches_the_clipboard() -> Result<()> {
        let actions = Arc::new(CountingActions(AtomicU64::new(0)));
        let (_dir, core, device) = fixture(actions.clone()).await?;
        let scope = core
            .action_scope
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .clone();
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let held = core.store.clone();
        let holder = tokio::spawn(async move {
            held.call(move |_| {
                let _ = entered.send(());
                blocked.recv()?;
                Ok(())
            })
            .await
        });
        wait.await?;
        let worker_core = core.clone();
        let worker_scope = scope.clone();
        let (tx, mut rx) = mpsc::channel(4);
        let worker = tokio::spawn(async move {
            worker_core
                .receive(
                    &worker_core.store,
                    Some(&worker_scope),
                    &device,
                    Message::Clipboard(Clipboard {
                        transfer_id: Id::new(crate::random_hex(16))?,
                        text: "revoked".into(),
                        created_at: 1,
                        origin: ClipboardOrigin::Clipboard,
                    }),
                    &mut None,
                    &tx,
                )
                .await
        });
        while core.store.active_jobs() < 2 {
            tokio::task::yield_now().await;
        }
        scope.cancelled.send_replace(true);
        release.send(())?;
        holder.await??;
        worker.await??;
        assert_eq!(actions.0.load(Ordering::SeqCst), 0);
        assert!(
            rx.recv().await.is_none(),
            "revoked application sent a receipt"
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn side_effect_gate_observes_revocation_after_waiting_for_an_active_effect() -> Result<()>
    {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let origin = ActionOrigin::captured(&core.store, &device);
        let scope = ActionScope::new();
        let active = scope.active.lock().await;
        let waiting_scope = scope.clone();
        let invoked = Arc::new(AtomicBool::new(false));
        let effect_invoked = invoked.clone();
        let worker = tokio::spawn(async move {
            waiting_scope
                .effect(&origin, || async {
                    effect_invoked.store(true, Ordering::SeqCst);
                })
                .await
        });
        tokio::task::yield_now().await;
        scope.cancelled.send_replace(true);
        drop(active);
        assert!(worker.await?.is_none());
        assert!(!invoked.load(Ordering::SeqCst));
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn revocation_between_image_copy_and_notification_gates_every_followup_effect()
    -> Result<()> {
        struct RevokeAfterCopy {
            scope: ActionScope,
            calls: AtomicU64,
        }
        #[async_trait::async_trait]
        impl PcActions for RevokeAfterCopy {
            async fn clipboard_text(&self, _: &str) -> Result<()> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn clipboard_image(&self, _: &std::path::Path, _: &str) -> Result<()> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.scope.cancelled.send_replace(true);
                Ok(())
            }
            async fn notify(&self, _: &std::path::Path, _: &str) -> Result<()> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn notify_clipboard_failure(&self) -> Result<()> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn open(&self, _: &std::path::Path) -> Result<()> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let scope = ActionScope::new();
        let actions = Arc::new(RevokeAfterCopy {
            scope: scope.clone(),
            calls: AtomicU64::new(0),
        });
        let (dir, core, device) = fixture(actions.clone()).await?;
        let job = ActionJob {
            file: ReceivedFile::from_file(dir.path().join("image.png"), tempfile::tempfile()?)?,
            scope,
            origin: ActionOrigin::captured(&core.store, &device),
            path: dir.path().join("image.png"),
            metadata: FileBegin {
                kind: FileKind::ClipboardImage,
                ..test_begin(b"image")
            },
            config: Config {
                notification_preview: true,
                ..core.config()
            },
            arrived: Instant::now(),
            save_error: None,
        };
        core.file_actions(&job, true, DesktopAction::Explicit).await;
        core.file_actions(&job, true, DesktopAction::Notification)
            .await;
        core.file_actions(&job, true, DesktopAction::CopyFailed)
            .await;
        core.file_actions(&job, true, DesktopAction::Automatic)
            .await;
        run_action(
            &core,
            &job,
            &mut core.shutdown.subscribe(),
            DesktopAction::Viewer,
            true,
        )
        .await;
        assert_eq!(
            actions.calls.load(Ordering::SeqCst),
            1,
            "revoked work initiated a follow-up OS effect"
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn configured_port_is_available_before_serve_and_live_changes_are_rejected() -> Result<()>
    {
        let (_dir, core, _) = fixture(Arc::new(NoActions)).await?;
        assert_eq!(core.bound_port(), core.config().port);
        core.update_config(core.config()).await?;
        let persisted = std::fs::read(&core.paths.config)?;
        let mut events = core.subscribe();
        let original = core.config().port;
        let mut next = core.config();
        next.port = original + 1;
        let error = core.update_config(next).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "changing the port takes effect after restarting pneud"
        );
        assert_eq!(core.config().port, original);
        assert_eq!(core.bound_port(), original);
        assert_eq!(std::fs::read(&core.paths.config)?, persisted);
        assert!(events.try_recv().is_err());
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn unpair_confirmation_for_replaced_phone_cannot_remove_current_binding() -> Result<()> {
        let (_dir, core, displayed) = fixture(Arc::new(NoActions)).await?;
        core.admission.lock().unwrap().peers = core.store.peers().await?;
        // The window captured displayed before the CLI paired another device.
        let invitation = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        let replacement = Id::new(crate::random_hex(16))?;
        let fingerprint = Hash::new(crate::random_hex(32))?;
        core.finish_pairing(&replacement, &fingerprint, &invitation.token)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let scope = core
            .action_scope
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .clone();
        let mut events = core.subscribe();
        core.unpair_device(displayed).await?;
        assert_eq!(core.paired_devices(), vec![replacement]);
        assert_eq!(
            core.store.peers().await?,
            core.admission.lock().unwrap().peers
        );
        assert!(!core.sessions.lock().unwrap().unpairing);
        assert!(!*scope.cancelled.borrow());
        assert!(events.try_recv().is_err());
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn revoked_admission_scope_cannot_be_replaced_by_same_device_id() -> Result<()> {
        let (dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let peers = core.store.peers().await?;
        core.admission.lock().unwrap().peers = peers;
        let admitted = ActionScope::new();
        *core.action_scope.lock().unwrap() = Some((device.clone(), admitted.clone()));
        let invitation = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        core.finish_pairing(
            &device,
            &Hash::new(crate::random_hex(32))?,
            &invitation.token,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert!(*admitted.cancelled.borrow());
        let replacement = core
            .action_scope
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .clone();
        assert!(!*replacement.cancelled.borrow());
        for kind in [FileKind::Screenshot, FileKind::ClipboardImage] {
            let mut metadata = test_begin(b"image");
            metadata.mime = "image/png".into();
            metadata.kind = kind;
            core.queue_file_actions(
                &admitted,
                ActionOrigin::captured(&core.store, &device),
                ReceivedFile::from_file(dir.path().join("old.png"), tempfile::tempfile()?)?,
                metadata,
                core.config(),
                None,
            );
        }
        {
            let pending = core.action_queue.pending.lock().unwrap();
            assert!(
                pending.automatic.is_none()
                    && pending.explicit.is_empty()
                    && pending.notifications.is_empty()
            );
        }
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn concurrent_clipboard_ipc_assigns_timestamps_in_commit_order() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let peers = core.store.peers().await?;
        core.admission.lock().unwrap().peers = peers;
        let (close, _closed) = watch::channel(None);
        core.sessions.lock().unwrap().session = Some(Session {
            device: device.clone(),
            id: 1,
            close,
            wake: Arc::new(Notify::new()),
            paused: false,
        });
        core.store.call(|c| {
            c.execute_batch("CREATE TEMP TABLE enqueues(id TEXT,created_at INTEGER);
                CREATE TEMP TRIGGER record_enqueue AFTER INSERT ON outgoing BEGIN
                INSERT INTO enqueues VALUES (json_extract(NEW.payload,'$.transfer_id'),json_extract(NEW.payload,'$.created_at')); END;")?;
            Ok(())
        }).await?;
        let mut requests = JoinSet::new();
        for n in 0..32 {
            let core = core.clone();
            let device = device.clone();
            requests.spawn(async move { core.send_clipboard(device, format!("text {n}")).await });
        }
        let mut ids = Vec::new();
        while let Some(result) = requests.join_next().await {
            ids.push(result??);
        }
        let timestamps = core
            .store
            .call(|c| {
                let mut q = c.prepare("SELECT id,created_at FROM enqueues ORDER BY rowid")?;
                Ok(q.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        assert_eq!(timestamps.len(), 32);
        assert!(timestamps.windows(2).all(|w| w[0].1 < w[1].1));
        let latest = core.store.first_outgoing(device).await?.unwrap().1;
        assert_eq!(latest.transfer_id.as_str(), timestamps.last().unwrap().0);
        assert_eq!(latest.created_at, timestamps.last().unwrap().1);
        assert!(ids.contains(&latest.transfer_id));
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn failed_unpair_restores_lifecycle_even_when_caller_is_cancelled() -> Result<()> {
        for cancel in [false, true] {
            let (dir, core, _) = fixture(Arc::new(NoActions)).await?;
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?.to_string();
            let server = tokio::spawn(core.clone().serve(listener, false));
            let phone = crate::client::Phone::new(dir.path().join("phone"))?;
            let invitation = core.start_pairing_with_addresses(vec![address]).await?;
            let mut events = core.subscribe();
            let mut original = phone.pair(&invitation).await?;
            while !matches!(
                events.recv().await?,
                Event::ConnectionChanged {
                    connected: true,
                    ..
                }
            ) {}
            core.event(Event::TransferStarted {
                device: phone.identity.device_id.clone(),
                metadata: test_begin(b"partial"),
            });
            assert!(core.ui_snapshot().receiving);
            let listening = tokio::spawn(async move { original.listen(60).await });
            let device = phone.identity.device_id.clone();
            let old = core
                .action_scope
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .1
                .clone();
            let epoch = core.store.with_current_pairing();
            core.store
                .call(|c| {
                    c.execute_batch(
                        "CREATE TEMP TRIGGER reject_unpair BEFORE DELETE ON peers
                    BEGIN SELECT RAISE(ABORT,'injected persistence failure'); END;",
                    )?;
                    Ok(())
                })
                .await?;
            // Wait until the blocking transaction has started, then optionally
            // cancel its caller while SQLite is still held by another job.
            let (entered, wait) = tokio::sync::oneshot::channel();
            let (release, blocking) = std::sync::mpsc::channel();
            let held = core.store.clone();
            let holder = tokio::spawn(async move {
                held.call(move |_| {
                    let _ = entered.send(());
                    blocking.recv()?;
                    Ok(())
                })
                .await
            });
            wait.await?;
            let unpair_core = core.clone();
            let peer = device.clone();
            let task = tokio::spawn(async move { unpair_core.unpair_device(peer).await });
            while !core.sessions.lock().unwrap().unpairing || core.store.active_jobs() < 2 {
                tokio::task::yield_now().await;
            }
            if cancel {
                task.abort();
            }
            release.send(())?;
            holder.await??;
            if cancel {
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                assert!(task.await?.is_err());
            }
            core.store.drain().await;
            assert!(!core.ui_snapshot().receiving);
            assert!(core.ui.lock().unwrap().receiving_until.is_none());
            let phone_metadata: crate::ui::Phone =
                serde_json::from_slice(&std::fs::read(core.paths.data.join("phone.json"))?)?;
            assert_eq!(phone_metadata.last_seen, core.ui_snapshot().phone.last_seen);
            let error = tokio::time::timeout(Duration::from_secs(2), listening)
                .await??
                .unwrap_err();
            // A concurrent heartbeat write may be interrupted by revocation.
            // READY -> CLOSED permits EOF without ERROR; appending Closing to
            // a partial frame would corrupt the stream. Trust checks below must
            // hold for either valid close outcome.
            assert!(
                error.to_string().contains("Closing") || error.to_string() == "PC disconnected",
                "unexpected phone result: {error}"
            );
            assert!(
                phone.directory.join("binding.json").exists(),
                "failed unpair told phone to forget trust"
            );
            assert!(!core.sessions.lock().unwrap().unpairing);
            assert!(core.sessions.lock().unwrap().session.is_none());
            assert_eq!(core.paired_devices(), vec![device.clone()]);
            assert_eq!(
                core.store.peers().await?.get(&phone.identity.fingerprint),
                Some(&device)
            );
            assert!(
                epoch.first_outgoing(device.clone()).await.is_ok(),
                "failed unpair changed generation"
            );
            assert!(*old.cancelled.borrow());
            let fresh = core
                .action_scope
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .1
                .clone();
            assert!(!*fresh.cancelled.borrow());
            assert!(!Arc::ptr_eq(&old.active, &fresh.active));
            let mut persistence_error = false;
            while let Ok(event) = events.try_recv() {
                assert!(!matches!(event, Event::Unpaired { .. }));
                persistence_error |= matches!(event, Event::Error { .. });
            }
            assert!(persistence_error);
            let replacement = phone.reconnect().await?;
            tokio::time::timeout(Duration::from_secs(2), async {
                while !matches!(
                    events.recv().await?,
                    Event::ConnectionChanged {
                        connected: true,
                        ..
                    }
                ) {}
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            core.store
                .call(|c| {
                    c.execute_batch("DROP TRIGGER reject_unpair;")?;
                    Ok(())
                })
                .await?;
            core.unpair_device(device).await?;
            assert!(core.paired_devices().is_empty());
            drop(replacement);
            core.request_shutdown();
            tokio::time::timeout(Duration::from_secs(3), server).await???;
            core.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn failed_repair_preserves_trust_scope_connection_and_invitation() -> Result<()> {
        for cancel in [false, true] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let bindings = core.store.peers().await?;
            core.admission.lock().unwrap().peers = bindings.clone();
            let scope = ActionScope::new();
            *core.action_scope.lock().unwrap() = Some((device.clone(), scope.clone()));
            let (close, closed) = watch::channel(None);
            core.sessions.lock().unwrap().session = Some(Session {
                device: device.clone(),
                id: 1,
                close,
                wake: Arc::new(Notify::new()),
                paused: false,
            });
            let invitation = core
                .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
                .await?;
            let fp = Hash::new(crate::random_hex(32))?;
            let epoch = core.store.with_current_pairing();
            core.store
                .call(|c| {
                    // Fail after DELETE, proving replacement and generation roll
                    // back together before publishing lifecycle changes.
                    c.execute_batch(
                        "CREATE TEMP TRIGGER reject_pair BEFORE INSERT ON peers
                    BEGIN SELECT RAISE(ABORT,'injected persistence failure'); END;",
                    )?;
                    Ok(())
                })
                .await?;
            let (entered, wait) = tokio::sync::oneshot::channel();
            let (release, blocking) = std::sync::mpsc::channel();
            let held = core.store.clone();
            let holder = tokio::spawn(async move {
                held.call(move |_| {
                    let _ = entered.send(());
                    blocking.recv()?;
                    Ok(())
                })
                .await
            });
            wait.await?;
            let pairing = core.clone();
            let peer = device.clone();
            let pin = fp.clone();
            let token = invitation.token.clone();
            let task =
                tokio::spawn(async move { pairing.finish_pairing(&peer, &pin, &token).await });
            while core.store.active_jobs() < 2 {
                tokio::task::yield_now().await;
            }
            if cancel {
                task.abort();
            }
            release.send(())?;
            holder.await??;
            if cancel {
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                assert_eq!(task.await?, Err(PairCode::StorageFailed));
            }
            core.store.drain().await;
            assert_eq!(core.store.peers().await?, bindings);
            assert_eq!(core.admission.lock().unwrap().peers, bindings);
            assert_eq!(
                core.admission
                    .lock()
                    .unwrap()
                    .invitation
                    .as_ref()
                    .unwrap()
                    .token,
                invitation.token
            );
            assert!(!core.sessions.lock().unwrap().unpairing);
            assert_eq!(
                core.sessions.lock().unwrap().session.as_ref().unwrap().id,
                1
            );
            assert!(closed.borrow().is_none());
            assert!(!*scope.cancelled.borrow());
            assert!(Arc::ptr_eq(
                &scope.active,
                &core.action_scope.lock().unwrap().as_ref().unwrap().1.active
            ));
            assert!(epoch.first_outgoing(device.clone()).await.is_ok());
            core.store
                .call(|c| {
                    c.execute_batch("DROP TRIGGER reject_pair;")?;
                    Ok(())
                })
                .await?;
            core.finish_pairing(&device, &fp, &invitation.token)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            assert_eq!(core.store.peers().await?.get(&fp), Some(&device));
            assert!(*scope.cancelled.borrow());
            assert!(epoch.first_outgoing(device).await.is_err());
            core.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn canceled_pairing_still_finishes_admission_and_consumes_invitation() -> Result<()> {
        let (_dir, core, _device) = fixture(Arc::new(NoActions)).await?;
        let invitation = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        let device = Id::new(crate::random_hex(16))?;
        let fp = Hash::new(crate::random_hex(32))?;
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocking) = std::sync::mpsc::channel();
        let held = core.store.clone();
        let holder = tokio::spawn(async move {
            held.call(move |_| {
                let _ = entered.send(());
                blocking.recv()?;
                Ok(())
            })
            .await
        });
        wait.await?;
        let pairing = core.clone();
        let peer = device.clone();
        let pin = fp.clone();
        let task =
            tokio::spawn(
                async move { pairing.finish_pairing(&peer, &pin, &invitation.token).await },
            );
        while core.store.active_jobs() < 2 {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        release.send(())?;
        holder.await??;
        core.store.drain().await;
        assert_eq!(core.admission.lock().unwrap().peers.get(&fp), Some(&device));
        assert!(core.admission.lock().unwrap().invitation.is_none());
        assert_eq!(core.store.peers().await?.get(&fp), Some(&device));
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn pairing_failure_order_and_one_time_consumption() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let paths = Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: dir.path().join("socket"),
        };
        let core = Core::new(
            paths,
            Config {
                save_path: dir.path().join("saved"),
                ..Config::default()
            },
            Arc::new(NoActions),
        )
        .await?;
        let device = Id::new(crate::random_hex(16))?;
        let fp = Hash::new(crate::random_hex(32))?;
        let bad = Hash::new(crate::random_hex(32))?;
        assert_eq!(
            core.finish_pairing(&device, &fp, &bad).await,
            Err(PairCode::PairingClosed)
        );
        let uri = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        core.admission
            .lock()
            .unwrap()
            .invitation
            .as_mut()
            .unwrap()
            .expires_at = crate::now_ms() - 1;
        assert_eq!(
            core.finish_pairing(&device, &fp, &bad).await,
            Err(PairCode::ExpiredToken)
        );
        let uri2 = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        assert_eq!(
            core.finish_pairing(&device, &fp, &uri.token).await,
            Err(PairCode::InvalidToken)
        );
        core.finish_pairing(&device, &fp, &uri2.token)
            .await
            .unwrap();
        assert_eq!(
            core.finish_pairing(&device, &fp, &uri2.token).await,
            Err(PairCode::PairingClosed)
        );
        let uri3 = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        assert_eq!(
            core.finish_pairing(&device, &Hash::new(crate::random_hex(32))?, &uri3.token)
                .await,
            Ok(())
        );
        assert!(core.admission.lock().unwrap().invitation.is_none());
        assert_eq!(core.paired_devices(), vec![device]);
        Ok(())
    }
    #[tokio::test]
    async fn status_traffic_reuses_one_blocked_outgoing_query() -> Result<()> {
        let (dir, core, _) = fixture(Arc::new(NoActions)).await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let server = tokio::spawn(core.clone().serve(listener, false));
        let phone = crate::client::Phone::new(dir.path().join("phone"))?;
        let invitation = core.start_pairing_with_addresses(vec![address]).await?;
        let mut events = core.subscribe();
        let mut connection = phone.pair(&invitation).await?;
        while !matches!(
            events.recv().await?,
            Event::ConnectionChanged {
                connected: true,
                ..
            }
        ) {}
        core.store.drain().await;
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocking) = std::sync::mpsc::channel();
        let held = core.store.clone();
        let holder = tokio::spawn(async move {
            held.call(move |_| {
                let _ = entered.send(());
                blocking.recv()?;
                Ok(())
            })
            .await
        });
        wait.await?;
        let outcome: Result<()> = async {
            for _ in 0..32 {
                connection
                    .write(&Message::Status(Status { paused: false }))
                    .await?;
                tokio::time::timeout(Duration::from_secs(2), async {
                    while !matches!(
                        events.recv().await?,
                        Event::PhonePaused { paused: false, .. }
                    ) {}
                    Ok::<_, anyhow::Error>(())
                })
                .await??;
                tokio::task::yield_now().await;
                ensure!(
                    core.store.active_jobs() <= 2,
                    "STATUS spawned duplicate blocking queries"
                );
            }
            Ok(())
        }
        .await;
        release.send(())?;
        holder.await??;
        core.store.drain().await;
        drop(connection);
        core.request_shutdown();
        tokio::time::timeout(Duration::from_secs(3), server).await???;
        outcome
    }

    #[tokio::test]
    async fn unpair_marks_lifecycle_before_blocked_store_and_refuses_replacement() -> Result<()> {
        let (dir, core, _) = fixture(Arc::new(NoActions)).await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let server = tokio::spawn(core.clone().serve(listener, false));
        let phone = crate::client::Phone::new(dir.path().join("phone"))?;
        let invitation = core.start_pairing_with_addresses(vec![address]).await?;
        let mut original = phone.pair(&invitation).await?;
        let mut listening = tokio::spawn(async move { original.listen(60).await });
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocking) = std::sync::mpsc::channel();
        let held = core.store.clone();
        let holder = tokio::spawn(async move {
            held.call(move |_| {
                let _ = entered.send(());
                blocking.recv()?;
                Ok(())
            })
            .await
        });
        wait.await?;
        let unpair_core = core.clone();
        let device = phone.identity.device_id.clone();
        let unpair = tokio::spawn(async move { unpair_core.unpair_device(device).await });
        while !core.sessions.lock().unwrap().unpairing {
            tokio::task::yield_now().await;
        }
        let still_connected = core.phone_paused(&phone.identity.device_id) == Some(false);
        let premature = tokio::time::timeout(Duration::from_millis(20), &mut listening)
            .await
            .is_ok();
        let retained = phone.directory.join("binding.json").exists();
        let mut replacement = phone.reconnect().await?;
        let refused = replacement.read(Duration::from_secs(2)).await;
        release.send(())?;
        holder.await??;
        tokio::time::timeout(Duration::from_secs(2), unpair).await???;
        assert!(matches!(
            refused?,
            Message::Error(ErrorMessage {
                code: ErrorCode::Unauthorized,
                ..
            })
        ));
        assert!(core.paired_devices().is_empty());
        assert!(still_connected && !premature && retained);
        let phone_result = tokio::time::timeout(Duration::from_secs(2), listening).await??;
        assert!(
            phone_result
                .unwrap_err()
                .to_string()
                .contains("PC unpaired")
        );
        assert!(!phone.directory.join("binding.json").exists());
        drop(replacement);
        core.request_shutdown();
        tokio::time::timeout(Duration::from_secs(3), server).await???;
        Ok(())
    }
    #[tokio::test]
    async fn idle_file_worker_has_no_deadline_after_more_than_a_day() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let (frames, messages) = mpsc::channel(4);
        let (writer, _receipts) = mpsc::channel(4);
        let (close, _) = watch::channel(None);
        let worker = tokio::spawn(core.clone().file_loop_test(
            core.store.clone(),
            device,
            messages,
            writer,
            close,
        ));
        tokio::task::yield_now().await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(2 * 86400)).await;
        tokio::task::yield_now().await;
        let idle = !worker.is_finished();
        drop(frames);
        worker.await??;
        tokio::time::resume();
        core.shutdown_actions().await?;
        assert!(
            idle,
            "idle file worker closed a healthy text/heartbeat session"
        );
        Ok(())
    }
    #[tokio::test]
    async fn remote_unpair_rechecks_authority_after_gate_wait_and_reports_noop() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _close = admit_ui_test_phone(&core, &device, 1)?;
        let mut events = core.subscribe();
        let guard = core.pairing_gate.lock().await;
        let worker = core.clone();
        let peer = device.clone();
        let task = tokio::spawn(async move {
            worker
                .forget_pairing(
                    peer,
                    CloseReason::Error(ErrorCode::Closing),
                    Some(1),
                    Some("old phone".into()),
                )
                .await
        });
        tokio::task::yield_now().await;
        core.sessions.lock().unwrap().session.as_mut().unwrap().id = 2;
        drop(guard);
        assert!(!task.await??);
        assert!(events.try_recv().is_err());
        assert_eq!(core.phone_paused(&device), Some(false));
        assert!(!core.store.peers().await?.is_empty());
        assert!(!core.sessions.lock().unwrap().unpairing);
        assert!(
            !core
                .forget_pairing(
                    Id::new(crate::random_hex(16))?,
                    CloseReason::Unpair,
                    None,
                    Some("absent phone".into())
                )
                .await?
        );
        assert!(events.try_recv().is_err());
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn remote_unpair_notice_requires_committed_removal_even_if_caller_is_cancelled()
    -> Result<()> {
        for mode in ["success", "failure", "cancelled"] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let _close = admit_ui_test_phone(&core, &device, 1)?;
            let mut events = core.subscribe();
            if mode == "failure" {
                core.store.call(|c| {
                    c.execute_batch("CREATE TEMP TRIGGER reject_unpair BEFORE DELETE ON peers BEGIN SELECT RAISE(ABORT,'injected failure'); END;")?;
                    Ok(())
                }).await?;
            }
            let (entered, wait) = tokio::sync::oneshot::channel();
            let (release, blocking) = std::sync::mpsc::channel();
            let held = core.store.clone();
            let holder = tokio::spawn(async move {
                held.call(move |_| {
                    let _ = entered.send(());
                    blocking.recv()?;
                    Ok(())
                })
                .await
            });
            wait.await?;
            let worker = core.clone();
            let peer = device.clone();
            let task = tokio::spawn(async move {
                worker
                    .forget_pairing(
                        peer,
                        CloseReason::Error(ErrorCode::Closing),
                        Some(1),
                        Some("Phone".into()),
                    )
                    .await
            });
            while !core.sessions.lock().unwrap().unpairing || core.store.active_jobs() < 2 {
                tokio::task::yield_now().await;
            }
            if mode == "cancelled" {
                task.abort();
            }
            release.send(())?;
            holder.await??;
            if mode == "cancelled" {
                assert!(task.await.unwrap_err().is_cancelled());
            } else if mode == "failure" {
                assert!(task.await?.is_err());
            } else {
                assert!(task.await??);
            }
            core.store.drain().await;
            let mut lifecycle = Vec::new();
            while let Ok(event) = events.try_recv() {
                match event {
                    Event::Unpaired { .. } => lifecycle.push("unpaired"),
                    Event::RemoteUnpaired { name } => {
                        assert_eq!(name, "Phone");
                        lifecycle.push("notice");
                    }
                    Event::ConnectionChanged {
                        connected: false, ..
                    } => lifecycle.push("disconnected"),
                    _ => {}
                }
            }
            if mode == "failure" {
                assert_eq!(lifecycle, ["disconnected"]);
                assert!(!core.store.peers().await?.is_empty());
            } else {
                assert_eq!(lifecycle, ["unpaired", "notice", "disconnected"]);
                assert!(core.store.peers().await?.is_empty());
            }
            core.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn replaced_connection_cannot_publish_disconnect_status_progress_or_unpair() -> Result<()>
    {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let (close, _rx) = watch::channel(None);
        core.admission
            .lock()
            .unwrap()
            .peers
            .insert(Hash::new(crate::random_hex(32))?, device.clone());
        core.sessions.lock().unwrap().session = Some(Session {
            device: device.clone(),
            id: 2,
            close,
            wake: Arc::new(Notify::new()),
            paused: false,
        });
        let mut events = core.subscribe();
        core.unpair_device(Id::new(crate::random_hex(16))?).await?;
        assert!(!core.sessions.lock().unwrap().unpairing);
        let mut old = core.store.clone();
        old.connection_id = Some(1);
        core.connection_event(
            &old,
            &device,
            Event::TransferProgress {
                device: device.clone(),
                transfer_id: Id::new(crate::random_hex(16))?,
                received: 1,
                total: 1,
            },
        );
        core.set_phone_paused(&device, 1, true);
        assert!(!core.finish_connection(&device, 1));
        core.forget_pairing(
            device.clone(),
            CloseReason::Error(ErrorCode::Closing),
            Some(1),
            Some("obsolete phone".into()),
        )
        .await?;
        assert!(events.try_recv().is_err());
        assert_eq!(core.phone_paused(&device), Some(false));
        assert!(core.finish_connection(&device, 2));
        assert!(matches!(
            events.try_recv()?,
            Event::ConnectionChanged {
                connected: false,
                ..
            }
        ));
        core.shutdown_actions().await?;
        Ok(())
    }
    #[derive(Default)]
    struct TimeoutImageActions {
        blocked: Notify,
        opened: Mutex<Vec<PathBuf>>,
        changed: Notify,
    }
    #[async_trait::async_trait]
    impl PcActions for TimeoutImageActions {
        async fn clipboard_text(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn clipboard_image(&self, path: &std::path::Path, _: &str) -> Result<()> {
            if path.file_name().is_some_and(|n| n == "latest.png") {
                self.blocked.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(())
        }
        async fn notify(&self, _: &std::path::Path, _: &str) -> Result<()> {
            Ok(())
        }
        async fn open(&self, path: &std::path::Path) -> Result<()> {
            self.opened.lock().unwrap().push(path.to_owned());
            self.changed.notify_one();
            Ok(())
        }
    }
    #[tokio::test]
    async fn latest_image_wins_even_when_its_optional_action_times_out() -> Result<()> {
        let actions = Arc::new(TimeoutImageActions::default());
        let (dir, core, device) = fixture(actions.clone()).await?;
        let scope = ActionScope::new();
        *core.action_scope.lock().unwrap() = Some((device.clone(), scope.clone()));
        let mut config = core.config();
        config.open_in_viewer = true;
        config.copy_image_to_clipboard = true;
        for name in ["first.png", "latest.png"] {
            let path = dir.path().join(name);
            let bytes = b"\x89PNG\r\n\x1a\nimage";
            std::fs::write(&path, bytes)?;
            let mut meta = test_begin(bytes);
            meta.kind = FileKind::Screenshot;
            meta.mime = "image/png".into();
            meta.file_name = name.into();
            core.queue_file_actions(
                &scope,
                ActionOrigin::captured(&core.store, &device),
                ReceivedFile::open(&path)?,
                meta,
                config.clone(),
                None,
            );
        }
        tokio::time::timeout(Duration::from_secs(2), actions.blocked.notified()).await?;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(16)).await;
        tokio::time::timeout(Duration::from_secs(1), actions.changed.notified()).await?;
        tokio::time::resume();
        assert_eq!(
            &*actions.opened.lock().unwrap(),
            &[dir.path().join("latest.png")]
        );
        core.shutdown_actions().await?;
        Ok(())
    }
}
