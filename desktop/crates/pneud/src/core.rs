#[path = "clipboard_actor.rs"]
pub(crate) mod clipboard_actor;
#[path = "phone_notifications.rs"]
mod phone_notifications;
use crate::{
    Config, Paths,
    actions::{PcActions, ReceivedFile},
    identity::Identity,
    storage::{Stage, VerifiedFile, cache_basename, local_name},
    store::{Metadata, Store, normalize_legacy_hello},
    tls::{Admission, ClientVerifier, Invitation},
};
use anyhow::{Context, Result, bail, ensure};
use clipboard_actor::{
    Event as ClipboardEvent, Operation as ClipboardOperation, driver as clipboard_driver,
};
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use pneu_proto::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
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
use tokio_rustls::{TlsAcceptor, TlsStream};
#[path = "file_sender.rs"]
mod file_sender;
#[path = "mesh.rs"]
pub mod mesh;
#[path = "peer_settings.rs"]
mod peer_settings;
#[path = "phone_controls.rs"]
mod phone_controls;
#[path = "replacement.rs"]
mod replacement;
#[path = "setup.rs"]
mod setup;
use file_sender::{FileWrite, transmit_file};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    #[cfg(windows)]
    WindowClosed {
        request: u64,
    },
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
    ComputerPaired {
        device: Id,
        name: String,
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
        fingerprint: Hash,
    },
    RemoteUnpaired {
        name: String,
    },
}
#[derive(Clone, Copy)]
pub(crate) enum CloseReason {
    Error(ErrorCode),
    Unpair,
    MembershipAck([u8; 16]),
}
impl CloseReason {
    fn message(self) -> Message {
        match self {
            Self::Error(code) => error_message(code),
            Self::Unpair => Message::Unpair(Unpair {}),
            Self::MembershipAck(id) => Message::Ack(Ack {
                transfer_id: Id::from_bytes(id),
                deduplicated: false,
            }),
        }
    }
}
#[derive(Clone)]
struct ActionScope {
    cancelled: watch::Sender<bool>,
    active: Arc<tokio::sync::Mutex<()>>,
    // Retained across disconnects; only admitting a newer connection advances it.
    connection_generation: Arc<AtomicU64>,
    registration_generation: Arc<AtomicU64>,
    candidate_generation: Arc<AtomicU64>,
    generation_changed: watch::Sender<u64>,
}
impl ActionScope {
    fn new() -> Self {
        Self {
            cancelled: watch::channel(false).0,
            active: Arc::new(tokio::sync::Mutex::new(())),
            connection_generation: Arc::new(AtomicU64::new(0)),
            registration_generation: Arc::new(AtomicU64::new(0)),
            candidate_generation: Arc::new(AtomicU64::new(0)),
            generation_changed: watch::channel(0).0,
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
        let mut generation_changed = self.generation_changed.subscribe();
        if *cancelled.borrow() {
            return None;
        }
        let _active = tokio::select! { biased;
            _ = cancelled.changed() => return None,
            _ = generation_changed.changed() => return None,
            guard = self.active.lock() => guard,
        };
        if *cancelled.borrow() {
            return None;
        }
        tokio::select! { biased;
            _ = cancelled.changed() => None,
            _ = generation_changed.changed() => None,
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
    /// Native notification submission is gated; its acknowledgement is not.
    async fn notification<F, Fut, Ack, T>(
        &self,
        origin: &ActionOrigin,
        issue: F,
    ) -> Option<Result<T>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Ack>>,
        Ack: std::future::Future<Output = Result<T>>,
    {
        let mut cancelled = self.cancelled.subscribe();
        let mut generation = self.generation_changed.subscribe();
        let wait = match self.effect(origin, issue).await? {
            Ok(wait) => wait,
            Err(error) => return Some(Err(error)),
        };
        if !self.allows(origin) {
            return None;
        }
        tokio::select! { biased;
            _ = cancelled.changed() => None,
            _ = generation.changed() => None,
            result = wait => Some(result),
        }
    }
}
#[derive(Clone)]
struct ActionOrigin {
    mesh: bool,
    device: Id,
    connection_id: Option<u64>,
}
impl ActionOrigin {
    fn captured(store: &Store, device: &Id) -> Self {
        Self {
            mesh: store.mesh,
            device: device.clone(),
            connection_id: store.action_connection_id.or(store.connection_id),
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
        metadata: Box<FileBegin>,
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
    File(Box<ActionJob>, DesktopAction),
    Receipt(ReceiptJob),
    Delivery(Box<crate::outbox::Transfer>, String),
}
impl NotificationJob {
    fn allows(&self) -> bool {
        match self {
            Self::File(job, _) => job.scope.allows(&job.origin),
            Self::Receipt(job) => job.scope.allows(&job.origin),
            Self::Delivery(..) => true,
        }
    }
}
// Established peer slots plus bounded ingress headroom let a known peer
// replace its session even when every established slot is occupied.
const SESSION_LIMIT: usize = 16;
const ADMISSION_LIMIT: usize = 16;
// Outbound candidate handshakes never borrow the inbound reserve. Account for
// their own headroom in the total socket cap as well as the admission cap.
const OUTBOUND_ADMISSION_LIMIT: usize = 4;
const TRANSPORT_LIMIT: usize = SESSION_LIMIT + ADMISSION_LIMIT + OUTBOUND_ADMISSION_LIMIT;
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
        Self::push_notice(pending, NotificationJob::File(Box::new(job), action));
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
    fn delivery(&self, transfer: crate::outbox::Transfer, name: String) {
        let mut pending = self.pending.lock().expect("pending actions");
        Self::push_notice(
            &mut pending,
            NotificationJob::Delivery(Box::new(transfer), name),
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
    let mut ui = ui.lock().expect("UI state");
    // Metadata authority outlives the socket. A different live peer cannot
    // revoke it; replacement/re-pair of this device can.
    if !ui.phone_generations.values().any(|id| *id == generation) {
        return Ok(());
    }
    if ui.phone_generation == Some(generation)
        && let Some(device) = ui.snapshot.phone.device.clone()
    {
        let phone = ui.snapshot.phone.clone();
        ui.phones.insert(device, phone);
    }
    // The per-device cache preserves ended peers when phone.json moves to the
    // next displayed device. Both files use private atomic publication.
    crate::config::atomic_private(
        &path.with_file_name("phones.json"),
        &serde_json::to_vec(&ui.phones)?,
    )?;
    crate::config::atomic_private(path, &serde_json::to_vec(&ui.snapshot.phone)?)?;
    Ok(())
}

/// Called under sessions -> admission -> UI locks by snapshots and lifecycle
/// writers alike, so identity, metadata, generation and status move together.
fn select_display(state: &Connections, admission: &Admission, ui: &mut crate::ui::State) {
    let mut paired: Vec<_> = admission.peers.values().cloned().collect();
    paired.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    paired.dedup();
    let mut connected: Vec<_> = state
        .sessions
        .values()
        .filter(|s| {
            paired.contains(&s.device) && !s.close.is_closed() && s.close.borrow().is_none()
        })
        .map(|s| s.device.clone())
        .collect();
    connected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let previous = ui.snapshot.phone.device.clone();
    let device = previous
        .as_ref()
        .filter(|id| connected.contains(id))
        .cloned()
        .or_else(|| connected.first().cloned())
        .or_else(|| previous.filter(|id| paired.contains(id)))
        .or_else(|| paired.first().cloned());
    let phone = device
        .as_ref()
        .and_then(|id| ui.phones.get(id))
        .cloned()
        .or_else(|| {
            device
                .as_ref()
                .and_then(|id| state.sessions.get(id))
                .filter(|s| s.phone.device == device)
                .map(|s| s.phone.clone())
        })
        .unwrap_or_else(|| crate::ui::Phone {
            device: device.clone(),
            name: if device.is_some() {
                "Phone".into()
            } else {
                String::new()
            },
            ..Default::default()
        });
    let session = device
        .as_ref()
        .filter(|id| connected.contains(id))
        .and_then(|id| state.sessions.get(id));
    ui.phone_generation = device
        .as_ref()
        .and_then(|id| ui.phone_generations.get(id))
        .copied();
    ui.receiving_until
        .retain(|_, until| *until > std::time::Instant::now());
    ui.snapshot.receiving = session.is_some()
        && device.as_ref().is_some_and(|id| {
            ui.active_transfers.iter().any(|(d, _)| d == id)
                || ui.receiving_until.keys().any(|(d, _)| d == id)
        });
    ui.snapshot.phone = phone;
    ui.snapshot.device = device;
    ui.snapshot.generation = ui.phone_generation;
    ui.snapshot.paired = ui.snapshot.device.is_some();
    ui.snapshot.connected = session.is_some();
    ui.snapshot.paused = session.is_some_and(|s| s.paused);
    ui.snapshot.paused_devices = connected
        .iter()
        .filter(|id| state.sessions.get(*id).is_some_and(|s| s.paused))
        .cloned()
        .collect();
    ui.snapshot.paired_devices = paired;
    ui.snapshot.connected_devices = connected;
}

fn update_ui_state(ui: &Mutex<crate::ui::State>, event: &Event) {
    let mut ui = ui.lock().expect("UI state");
    match event {
        Event::ConnectionChanged { device, connected } => {
            if *connected {
                ui.scope_reconnect_finished(device);
            }
            let now = crate::now_ms();
            if let Some(phone) = ui.phones.get_mut(device) {
                phone.last_seen = now;
            }
            if ui.snapshot.phone.device.as_ref() == Some(device) {
                ui.snapshot.connected = *connected;
                ui.snapshot.phone.last_seen = now;
            }
            ui.clear_transfers(device);
        }
        Event::PhonePaused { device, paused }
            if ui.snapshot.phone.device.as_ref() == Some(device) =>
        {
            ui.snapshot.paused = *paused;
        }
        Event::PairingProgress { name } => ui.snapshot.pairing = name.clone(),
        Event::TransferStarted { device, metadata } => {
            let key = (device.clone(), metadata.transfer_id.clone());
            if !ui
                .incoming
                .iter()
                .any(|t| t.device == *device && t.transfer_id == metadata.transfer_id)
            {
                ui.incoming
                    .push_back(crate::outbox::Transfer::received(device.clone(), metadata));
            }
            ui.active_transfers.insert(key.clone());
            ui.receiving_until
                .retain(|_, until| *until > std::time::Instant::now());
            ui.receiving_until
                .insert(key, std::time::Instant::now() + Duration::from_millis(1500));
        }
        Event::TransferProgress {
            device,
            transfer_id,
            received,
            ..
        } => {
            if let Some(t) = ui
                .incoming
                .iter_mut()
                .find(|t| t.device == *device && t.transfer_id == *transfer_id)
            {
                t.sent = *received;
            }
        }
        Event::TransferFinished {
            device,
            transfer_id,
            path,
            deduplicated,
        } => {
            ui.active_transfers
                .remove(&(device.clone(), transfer_id.clone()));
            if *deduplicated {
                ui.incoming.retain(|t| {
                    t.device != *device || t.transfer_id != *transfer_id || t.state != "sending"
                });
            } else if let Some(t) = ui
                .incoming
                .iter_mut()
                .find(|t| t.device == *device && t.transfer_id == *transfer_id)
            {
                t.state = "sent".into();
                t.sent = t.size;
                t.finished_at = Some(crate::now_ms());
                t.path = path.clone();
            }
            ui.prune_incoming();
        }
        Event::Unpaired { device } | Event::Paired { device } => {
            ui.scope_refresh.remove(device);
            ui.phones.remove(device);
            ui.phone_generations.remove(device);
            ui.clear_transfers(device);
            if ui.snapshot.phone.device.as_ref() == Some(device) {
                ui.snapshot.phone = crate::ui::Phone::default();
                ui.snapshot.connected = false;
                ui.snapshot.paused = false;
                ui.phone_generation = None;
            }
        }
        _ => {}
    }
}

fn emit_event(ui: &Mutex<crate::ui::State>, events: &broadcast::Sender<Event>, event: Event) {
    update_ui_state(ui, &event);
    let _ = events.send(event);
}

#[derive(Clone)]
enum OutgoingSelection {
    Text(Clipboard),
    Image(FileBegin, Arc<Vec<u8>>),
}
impl OutgoingSelection {
    fn id(&self) -> &Id {
        match self {
            Self::Text(text) => &text.transfer_id,
            Self::Image(begin, _) => &begin.transfer_id,
        }
    }
    /// The same recipient-specific frame is validated at publication and used
    /// by the writer. In particular, mesh clock fields count toward JSON size.
    fn first_frame(&self, mesh: bool) -> Message {
        match self {
            Self::Text(text) => {
                let mut text = text.clone();
                if !mesh {
                    text.hlc = None;
                    text.source_device = None;
                }
                Message::Clipboard(text)
            }
            Self::Image(begin, _) => {
                let mut begin = begin.clone();
                if !mesh {
                    begin.hlc = None;
                    begin.source_device = None;
                }
                Message::FileBegin(begin)
            }
        }
    }
}
#[derive(Clone)]
struct AutomaticWrite {
    target: ClipboardTarget,
    connection: u64,
    selection: OutgoingSelection,
}
enum WriterWork {
    Frame(Message),
    Automatic(AutomaticWrite),
    File(FileWrite),
}
/// Authority is checked at the first accepted byte, including after a socket
/// returned Pending. Once a frame starts, keep completing the in-flight operation.
struct DeliveryWriter<'a, W> {
    core: &'a Core,
    writer: &'a mut W,
    target: ClipboardTarget,
    operation_id: Id,
    connection: u64,
    started: bool,
    canceled: bool,
}
impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for DeliveryWriter<'_, W> {
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
        if sessions
            .sessions
            .get(&this.target.device)
            .is_none_or(|s| s.id != this.connection)
            || automatic.latest.get(&this.target.device) != Some(&this.operation_id)
            || !this
                .core
                .automatic_target_current_locked(&sessions, &this.target)
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
    core: &Core,
    writer: &mut W,
    priority: &mut mpsc::Receiver<Message>,
    normal: &mut phone_controls::FrameQueue,
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
        normal.send(core, writer, &message).await?;
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
    normal: &mut phone_controls::FrameQueue,
    mut text: Option<&mut mpsc::Receiver<AutomaticWrite>>,
    reports: Option<&mpsc::Sender<WriteReport>>,
) -> Result<bool> {
    let mut writer = DeliveryWriter {
        core,
        writer,
        target: work.target,
        operation_id: work.selection.id().clone(),
        connection: work.connection,
        started: false,
        canceled: false,
    };
    let frame = work
        .selection
        .first_frame(core.mesh_link(&writer.target.device, writer.connection));
    let result = async {
        match work.selection {
            OutgoingSelection::Text(_) => send(&mut writer, &frame).await?,
            OutgoingSelection::Image(begin, bytes) => {
                let id = begin.transfer_id;
                send(&mut writer, &frame).await?;
                for (index, chunk) in bytes.chunks(CHUNK_SIZE).enumerate() {
                    // Receipts and heartbeats stay prompt between image frames.
                    send_pending_messages(core, &mut *writer.writer, priority, normal, reports)
                        .await?;
                    if let Some(text) = &mut text {
                        send_pending_clipboard_text(
                            core,
                            &mut *writer.writer,
                            priority,
                            normal,
                            text,
                            reports,
                        )
                        .await?;
                    }
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
                send_pending_messages(core, &mut *writer.writer, priority, normal, reports).await?;
                send(&mut writer, &Message::FileEnd(FileEnd { transfer_id: id })).await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if !writer.started && (writer.canceled || !core.automatic_recipient_current(&writer.target)) {
        return Ok(false);
    }
    result?;
    Ok(true)
}
async fn send_pending_clipboard_text<W: tokio::io::AsyncWrite + Unpin>(
    core: &Core,
    writer: &mut W,
    priority: &mut mpsc::Receiver<Message>,
    normal: &mut phone_controls::FrameQueue,
    text: &mut mpsc::Receiver<AutomaticWrite>,
    reports: Option<&mpsc::Sender<WriteReport>>,
) -> Result<()> {
    if let Ok(work) = text.try_recv() {
        ensure!(
            matches!(work.selection, OutgoingSelection::Text(_)),
            "image on text lane"
        );
        let id = work.selection.id().clone();
        // Box the recursive edge: this invocation can only emit one text frame.
        let sent = Box::pin(transmit_automatic(
            core, writer, work, priority, normal, None, reports,
        ))
        .await?;
        if let Some(reports) = reports {
            reports
                .send(if sent {
                    WriteReport::Sent {
                        id,
                        at: Instant::now(),
                    }
                } else {
                    WriteReport::Canceled(id)
                })
                .await?;
        }
    }
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardTarget {
    pub device: Id,
    pub connection: u64,
    pub binding: u64,
    pub policy: u64,
    pub config_epoch: u64,
}
/// Frozen recipients of a physical clipboard offer. Later connections are never
/// added; a revoked recipient can be skipped without suppressing another one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardOffer {
    revision: u64,

    observed: bool,
    recipients: Vec<ClipboardRecipient>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct ClipboardRecipient {
    target: ClipboardTarget,
    text: bool,
    image: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipboardFormat {
    Text,
    Image,
}
impl ClipboardOffer {
    /// Plan only from observation-time recipients, capabilities and image policy.
    /// Availability may narrow the payloads; send-time checks may remove targets.
    pub(crate) fn payload_plans(
        &self,
        text: bool,
        image: bool,
    ) -> Vec<(ClipboardTarget, ClipboardFormat)> {
        self.recipients
            .iter()
            .filter_map(|recipient| {
                let format = if image && recipient.image {
                    ClipboardFormat::Image
                } else if text && recipient.text {
                    ClipboardFormat::Text
                } else {
                    return None;
                };
                Some((recipient.target.clone(), format))
            })
            .collect()
    }
}
#[cfg(all(test, target_os = "linux"))]
impl ClipboardOffer {
    pub(crate) fn test_offer() -> Self {
        Self {
            revision: 0,

            observed: false,
            recipients: vec![ClipboardRecipient {
                target: ClipboardTarget {
                    device: Id::from_bytes([1; 16]),
                    connection: 1,
                    binding: 0,
                    policy: 0,
                    config_epoch: 0,
                },
                text: true,
                image: true,
            }],
        }
    }
}
#[derive(Default)]
struct ClipboardDelivery {
    selections: HashMap<Id, AutomaticWrite>,
    in_flight: HashMap<(Id, bool), AutomaticWrite>,
    latest: HashMap<Id, Id>,
}
impl ClipboardDelivery {
    fn invalidate(&mut self) {
        self.latest.clear();
        self.selections.clear();
        self.in_flight.clear();
    }
    fn invalidate_peer(&mut self, device: &Id) {
        self.latest.remove(device);
        self.selections.remove(device);
        self.in_flight.retain(|(peer, _), _| peer != device);
    }
    fn replace_link(&mut self, device: &Id, connection: u64) {
        // Replacement retains the captured recipient and operation; ordinary
        // disconnect still invalidates both maps through invalidate_peer.
        for text in [false, true] {
            if let Some(work) = self.in_flight.remove(&(device.clone(), text))
                && self.latest.get(device) == Some(work.selection.id())
            {
                self.selections.entry(device.clone()).or_insert(work);
            }
        }
        if let Some(work) = self.selections.get_mut(device) {
            work.connection = connection;
        }
    }
    fn receipt(&mut self, device: &Id, connection: u64, id: &Id) {
        self.in_flight.retain(|(peer, _), work| {
            !(peer == device && work.connection == connection && work.selection.id() == id)
        });
    }
}
struct ApplicationMessage {
    message: Message,
    refusal: Option<NackCode>,
    resume_cancel: Option<Arc<watch::Sender<bool>>>,
}
impl From<Message> for ApplicationMessage {
    fn from(message: Message) -> Self {
        Self {
            message,
            refusal: None,
            resume_cancel: None,
        }
    }
}
struct ReadyState {
    setup_sent: Option<(SetupHello, u64)>,
    store: Store,
    allow_recovery: bool,
    action_scope: Option<ActionScope>,
    buffered: Vec<Message>,
}
struct Session {
    device: Id,
    id: u64,
    close: watch::Sender<Option<CloseReason>>,
    wake: Arc<Notify>,
    paused: bool,
    capabilities: Vec<String>,
    outbound: bool,
    mesh: bool,
    role: String,
    phone: crate::ui::Phone,
}
#[derive(Default)]
struct Connections {
    sessions: HashMap<Id, Session>,
    unpairing: HashSet<Id>,
}
type PhoneWrite = Shared<BoxFuture<'static, std::result::Result<(), Arc<String>>>>;
#[derive(Default)]
struct PhoneWrites {
    pending: HashMap<Id, u64>,
    completion: Option<PhoneWrite>,
}
/// Clone the Arc<Core> into a tray/window. Commands below are async; subscribe()
/// returns a bounded broadcast stream (lagging UI consumers must resnapshot).
pub struct Core {
    phone_controls: Mutex<phone_controls::Controls>,
    #[cfg(target_os = "macos")]
    pub(crate) ui_ready: Mutex<Arc<std::sync::atomic::AtomicBool>>,
    mesh: Arc<mesh::Mesh>,
    transports: Arc<Semaphore>,
    ingress: Arc<Semaphore>,
    outbound_ingress: Arc<Semaphore>,
    pub identity: Identity,
    pub paths: Paths,
    ui: Arc<Mutex<crate::ui::State>>,
    phone_writes: Arc<Mutex<PhoneWrites>>,
    automatic: Arc<Mutex<ClipboardDelivery>>,
    clipboard_config_epoch: Arc<AtomicU64>,
    outbox: crate::outbox::Outbox,
    file_admissions: Arc<crate::work::Work>,
    config: watch::Sender<Config>,
    events: broadcast::Sender<Event>,
    store: Store,
    admission: Arc<Mutex<Admission>>,
    invitation_changes: watch::Sender<u64>,
    setup_invitation_scope: AtomicBool,
    #[cfg(test)]
    setup_test_capability: AtomicBool,
    setup_incoming: Arc<Mutex<HashMap<u64, setup::IncomingTarget>>>,
    readmission_errors: Mutex<Vec<(crate::store::members::ReadmissionKey, String)>>,
    provisional: Arc<AtomicBool>,
    pairing_gate: Arc<tokio::sync::Mutex<()>>,
    pub(crate) setup_decisions: tokio::sync::Mutex<()>,
    config_gate: Arc<tokio::sync::Mutex<()>>,
    clipboard: clipboard_driver::Handle,
    clipboard_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    settings_gate: tokio::sync::Mutex<()>,
    settings_ready: Mutex<HashMap<Id, u64>>,
    next_connection: AtomicU64,
    next_candidate: AtomicU64,
    sessions: Arc<Mutex<Connections>>,
    actions: Arc<dyn PcActions>,
    action_queue: Arc<ActionQueue>,
    action_scope: Arc<Mutex<HashMap<Id, ActionScope>>>,
    action_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    action_stop: watch::Sender<bool>,
    shutdown: watch::Sender<bool>,
    port: AtomicU16,
    window_request: Mutex<(u64, Option<String>, bool)>,
    window_history: AtomicU64,
    // The window request whose device page opens with Remove from all devices.
    window_remove: Mutex<Option<(u64, Id)>>,
    pending_sends: Mutex<std::collections::VecDeque<crate::ui::window::SendRequest>>,
    window_focus: Mutex<Option<(u64, Instant)>>,
    window_visible: Mutex<Option<(u64, Instant)>>,
    #[cfg(test)]
    file_write_delay: Mutex<Duration>,
}
impl Drop for Core {
    fn drop(&mut self) {
        self.action_stop.send_replace(true);
        let (reply, _) = tokio::sync::oneshot::channel();
        let _ = self
            .clipboard
            .tx
            .send(clipboard_driver::Message::Stop(reply));
    }
}
impl Core {
    pub async fn new(
        paths: Paths,
        mut config: Config,
        actions: Arc<dyn PcActions>,
    ) -> Result<Arc<Self>> {
        if !config.clipboard_sync_to_phone
            && config.clipboard_mode == crate::config::ClipboardMode::Automatic
        {
            config.clipboard_mode = crate::config::ClipboardMode::Off;
        }
        config.clipboard_sync_to_phone = config.clipboard_mode != crate::config::ClipboardMode::Off;
        config.validate_with_cache(&paths.cache)?;
        paths.prepare()?;
        if let Err(error) = phone_notifications::prepare_icons(&paths.cache) {
            eprintln!("pneud: phone notification icon cleanup deferred: {error}");
        }
        let store = Store::open(&paths.data.join("state.sqlite3"))?;
        let identity = Identity::load_or_create(&paths.data)?;
        if let Some(own) = store.setup_state().roster.iter().find(|m| {
            m.device_id == identity.device_id
                && m.fingerprint == identity.fingerprint
                && m.status == MemberStatus::Member
        }) && config.name != own.name
        {
            config.name = own.name.clone();
            config.save(&paths.config)?;
        }
        let outbox = crate::outbox::Outbox::open(store.clone(), &paths.data.join("outbox"))?;
        outbox.recover_startup(config.queue_offline).await?;
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
            .map(|id| (id.clone(), ActionScope::new()))
            .collect();
        let current_metadata = |device: &Id, phone: &crate::ui::Phone| {
            phone.device.as_ref() == Some(device)
                && phone.pairing_generation.is_some()
                && phone.pairing_generation == store.pairing_generation(device)
                && admission
                    .lock()
                    .expect("admission")
                    .peers
                    .values()
                    .any(|id| id == device)
        };
        #[cfg(target_os = "macos")]
        for name in ["phone.json", "phones.json"] {
            let path = paths.data.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => crate::platform::private::check_path(&path)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let phone: crate::ui::Phone = std::fs::read(paths.data.join("phone.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<crate::ui::Phone>(&bytes).ok())
            .filter(|phone| {
                phone
                    .device
                    .as_ref()
                    .is_some_and(|id| current_metadata(id, phone))
            })
            .unwrap_or_default();
        let mut phones: HashMap<Id, crate::ui::Phone> =
            std::fs::read(paths.data.join("phones.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
        // Validate each file independently before merging: a stale per-device
        // entry must not shadow a current phone.json entry for the same ID.
        phones.retain(|device, phone| current_metadata(device, phone));
        if let Some(device) = phone.device.clone() {
            phones.entry(device).or_insert_with(|| phone.clone());
        }
        let mesh = mesh::Mesh::new(store.peer_info().await?);
        let (clipboard, clipboard_rx) = clipboard_driver::Handle::channel();
        let history_enabled = config.borrow().clipboard_history;
        store.history_configure(history_enabled, true).await?;
        let (register, legacy, history) = store.clipboard_snapshot().await?;
        let mut clipboard_state = clipboard_actor::State::load(
            register,
            legacy,
            history,
            store.clipboard_fingerprint_key.clone(),
        );
        clipboard_state.seed_tickets(rand::random());
        let core = Arc::new(Self {
            phone_controls: Mutex::default(),
            setup_invitation_scope: AtomicBool::new(true),
            #[cfg(test)]
            setup_test_capability: AtomicBool::new(true),
            setup_incoming: Default::default(),
            readmission_errors: Default::default(),
            mesh: Arc::new(mesh),
            outbox,
            transports: Arc::new(Semaphore::new(TRANSPORT_LIMIT)),
            ingress: Arc::new(Semaphore::new(ADMISSION_LIMIT)),
            outbound_ingress: Arc::new(Semaphore::new(OUTBOUND_ADMISSION_LIMIT)),
            phone_writes: Arc::new(Mutex::new(PhoneWrites::default())),
            automatic: Arc::new(Mutex::new(ClipboardDelivery::default())),
            clipboard_config_epoch: Arc::new(AtomicU64::new(0)),
            ui: Arc::new(Mutex::new(crate::ui::State {
                phones,
                snapshot: crate::ui::Snapshot {
                    phone,
                    ..Default::default()
                },
                ..Default::default()
            })),
            identity,
            file_admissions: Arc::default(),
            #[cfg(target_os = "macos")]
            ui_ready: Mutex::new(Arc::new(std::sync::atomic::AtomicBool::new(false))),
            paths,
            config,
            events,
            store,
            admission,
            invitation_changes,
            provisional: Arc::new(AtomicBool::new(false)),
            pairing_gate: Arc::new(tokio::sync::Mutex::new(())),
            setup_decisions: tokio::sync::Mutex::new(()),
            config_gate: Arc::new(tokio::sync::Mutex::new(())),
            sessions: Arc::new(Mutex::new(Connections::default())),
            clipboard,
            clipboard_worker: Mutex::new(None),
            settings_gate: tokio::sync::Mutex::new(()),
            settings_ready: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(1),
            next_candidate: AtomicU64::new(1),
            actions,
            action_queue: action_queue.clone(),
            action_scope: Arc::new(Mutex::new(action_scope)),
            action_worker: Mutex::new(None),
            action_stop,
            shutdown,
            port: AtomicU16::new(port),
            window_request: Mutex::new((0, None, false)),
            window_history: AtomicU64::new(0),
            window_remove: Mutex::new(None),
            pending_sends: Mutex::default(),
            window_focus: Mutex::new(None),
            window_visible: Mutex::new(None),
            #[cfg(test)]
            file_write_delay: Mutex::new(Duration::ZERO),
        });
        encode_frame(&Message::Hello(core.computer_hello()))?;
        if let Err(error) = core.reconcile_setup_candidates().await {
            core.error(None, format!("membership reconciliation deferred: {error}"));
        }
        let weak = Arc::downgrade(&core);
        let stop = core.action_stop.subscribe();
        *core.action_worker.lock().expect("actions") = Some(tokio::spawn(Box::pin(action_loop(
            weak,
            action_queue,
            stop,
        ))));
        *core.clipboard_worker.lock().expect("clipboard worker") =
            Some(tokio::spawn(clipboard_driver::run(
                Arc::downgrade(&core),
                core.clipboard.clone(),
                clipboard_rx,
                clipboard_state,
            )));
        Ok(core)
    }
    #[cfg(target_os = "macos")]
    pub(crate) async fn ui_test_notification(&self, transient: bool) -> Result<u32> {
        self.actions.ui_test_notification(transient).await
    }
    #[cfg(target_os = "macos")]
    pub(crate) async fn ui_test_open(&self) -> Result<()> {
        let file = ReceivedFile::open(&self.paths.cache.join("Pneu test viewer.png"))?;
        self.actions.open_received(&file).await
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
            .sessions
            .values()
            .filter(|s| !s.close.is_closed() && s.close.borrow().is_none())
            .map(|s| s.device.clone())
            .collect()
    }
    pub fn phone_paused(&self, device: &Id) -> Option<bool> {
        self.sessions
            .lock()
            .expect("sessions")
            .sessions
            .get(device)
            .filter(|s| &s.device == device && !s.close.is_closed() && s.close.borrow().is_none())
            .map(|s| s.paused)
    }
    fn set_phone_paused(&self, device: &Id, id: u64, paused: bool) {
        let mut state = self.sessions.lock().expect("sessions");
        if let Some(s) = state
            .sessions
            .get_mut(device)
            .filter(|s| s.id == id && &s.device == device)
        {
            let changed = s.paused != paused;
            s.paused = paused;
            if paused {
                self.clear_phone_control(device, id);
            }
            if changed {
                self.clipboard.event(ClipboardEvent::Narrow(device.clone()));
                self.automatic
                    .lock()
                    .expect("automatic clipboard")
                    .invalidate_peer(device);
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
                .sessions
                .get(device)
                .is_some_and(|s| &s.device == device && Some(s.id) == store.connection_id)
        {
            self.event(event);
        }
    }
    fn finish_connection(&self, device: &Id, id: u64) -> bool {
        let mut state = self.sessions.lock().expect("sessions");
        if state
            .sessions
            .get(device)
            .is_some_and(|s| s.id == id && &s.device == device)
        {
            state.sessions.remove(device);
            self.end_phone_control(device, id);
            self.clipboard.event(ClipboardEvent::Narrow(device.clone()));
            self.mesh.changed.notify_waiters();
            self.automatic
                .lock()
                .expect("automatic clipboard")
                .invalidate_peer(device);
            self.event(Event::ConnectionChanged {
                device: device.clone(),
                connected: false,
            });
            select_display(
                &state,
                &self.admission.lock().expect("admission"),
                &mut self.ui.lock().expect("UI state"),
            );
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
            || state.sessions.get(device).is_some_and(|s| {
                &s.device == device && Some(s.id) == connection_id && s.close.borrow().is_none()
            })
    }
    pub fn bound_port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }
    pub(crate) fn request_history_window(&self) {
        if !self.config().clipboard_history {
            return;
        }
        let mut request = self.window_request.lock().expect("window request");
        request.0 = ((request.0 >> 1) + 1) * 2;
        request.1 = None;
        request.2 = false;
        self.window_history.store(request.0, Ordering::SeqCst);
    }
    pub(crate) fn window_history(&self, request: u64) -> bool {
        request != 0
            && self.window_history.load(Ordering::SeqCst) == request
            && self.config().clipboard_history
    }
    pub(crate) async fn disable_empty_clipboard_history(&self) -> Result<usize> {
        let guard = self.config_gate.clone().lock_owned().await;
        let mut config = self.config();
        if !config.clipboard_history {
            return Ok(0);
        }
        let count = self.store.history_disable_if_empty().await;
        if count == 0 {
            config.clipboard_history = false;
            if let Err(error) = self.persist_config(config, guard).await {
                self.store.history_configure(true, true).await?;
                return Err(error);
            }
        }
        Ok(count)
    }
    pub(crate) async fn pin_clipboard_history(&self, id: String, pinned: bool) -> Result<()> {
        self.store.history_pin(id, pinned).await
    }
    pub(crate) async fn delete_clipboard_history(&self, id: Option<String>) -> Result<()> {
        self.store.history_delete(id).await
    }
    pub(crate) async fn copy_clipboard_history(&self, id: String) -> Result<()> {
        ensure!(self.config().clipboard_history, "clipboard history is off");
        let (input, capture) = self.store.history_input(id.clone()).await?;
        let content = self
            .outbox
            .file_io({
                let input = input.clone();
                move || {
                    Ok(match input {
                        crate::actions::ClipboardInput::Text(text) => {
                            crate::ui::clipboard::Content::Text(text)
                        }
                        crate::actions::ClipboardInput::Image(file, _) => {
                            crate::ui::clipboard::Content::Image(
                                file.read_bounded(crate::store::history::MAX_IMAGE)?,
                            )
                        }
                        _ => anyhow::bail!("invalid history input"),
                    })
                }
            })
            .await?;
        let token = self.clipboard.token();
        let offer = if self.config().clipboard_mode == crate::config::ClipboardMode::Automatic {
            self.automatic_target()
        } else {
            None
        };
        let mut operation = ClipboardOperation {
            token,
            id: Id::new(crate::random_hex(16))?,
            source: self.identity.device_id.clone(),
            key: None,
            created_at: 0,
            hashes: clipboard_actor::hashes(&content),
            content: Some(content),
            payload: None,
            offer,
            received: false,
            legacy: false,
            restore: true,
            repair: false,
            observation: None,
            sensitive: false,
            history: Some(capture),
            history_id: Some(id),
        };
        // Restoring is local even when peers cannot accept this representation.
        // The largest clock also accounts for JSON framing overhead.
        if operation.offer.is_some()
            && self
                .clipboard_frames(&operation, pneu_proto::MAX_TIMESTAMP)
                .is_err()
        {
            operation.offer = None;
        }
        let (reply, _) = tokio::sync::oneshot::channel();
        let outcome = self
            .clipboard
            .submit(
                operation,
                clipboard_driver::Request {
                    input,
                    claim: None,
                    store: self.store.clone(),
                    authority: None,
                    kind: None,
                    policy: Vec::new(),
                    reply,
                },
            )
            .await?;
        ensure!(outcome.applied, "clipboard copy was superseded");
        Ok(())
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
        request.2 = false;
    }
    pub fn request_add_device(&self) {
        self.request_add_device_with_activation(None);
    }
    pub(crate) fn request_add_device_with_activation(&self, token: Option<String>) {
        let mut request = self.window_request.lock().expect("window request");
        request.0 = ((request.0 >> 1) + 1) * 2;
        request.1 = token;
        request.2 = true;
    }
    /// A joined notification's Remove…: that device's page and its confirmation.
    pub(crate) fn request_remove_window(&self, device: Id, token: Option<String>) {
        let mut request = self.window_request.lock().expect("window request");
        request.0 = ((request.0 >> 1) + 1) * 2;
        request.1 = token;
        request.2 = false;
        *self.window_remove.lock().expect("window remove") = Some((request.0, device));
    }
    pub fn window_remove_device(&self, id: u64) -> Option<Id> {
        self.window_remove
            .lock()
            .expect("window remove")
            .as_ref()
            .filter(|(request, _)| *request == id)
            .map(|(_, device)| device.clone())
    }
    pub fn window_add_device(&self, id: u64) -> bool {
        let request = self.window_request.lock().expect("window request");
        request.0 == id && request.2
    }
    pub fn window_request(&self) -> u64 {
        self.window_request.lock().expect("window request").0
    }
    pub(crate) fn window_request_with_activation(&self) -> (u64, Option<String>) {
        let request = self.window_request.lock().expect("window request");
        (request.0, request.1.clone())
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
        let mut ui = self.ui.lock().expect("UI state");
        select_display(&sessions, &admission, &mut ui);
        let mut snapshot = ui.snapshot.clone();
        snapshot.phone_controls = self.phone_control_snapshot(&sessions, &admission);
        snapshot.clipboard_history = self.store.history_items();
        snapshot.replacements = self
            .store
            .replacements
            .lock()
            .expect("replacement offers")
            .clone();
        snapshot.setup_refusal_since = self.setup_refusal_since();
        snapshot.devices = self.device_snapshots(&sessions, &admission, &ui);
        snapshot.send_targets = self.file_targets(&snapshot.devices, &sessions, &admission);
        ui.scope_refresh.retain(|id, until| {
            *until > Instant::now() && admission.peers.values().any(|p| p == id)
        });
        snapshot.refreshing_devices = ui.scope_refresh.keys().cloned().collect();
        for device in &mut snapshot.devices {
            if snapshot.refreshing_devices.contains(&device.device) && device.route.is_none() {
                device.route = ui.phones.get(&device.device).map(|p| p.route.clone());
            }
        }
        snapshot.transfers = self.outbox.snapshot(&snapshot.connected_devices);
        let mut incoming: Vec<_> = ui.incoming.iter().cloned().collect();
        self.store.attribute_finished_transfers(&mut incoming);
        snapshot.transfers.extend(incoming);
        snapshot
    }
    fn arrival_source(&self, device: &Id) -> crate::actions::ArrivalSource {
        let snapshot = self.ui_snapshot();
        crate::actions::ArrivalSource {
            device: device.clone(),
            name: snapshot
                .devices
                .iter()
                .find(|d| &d.device == device)
                .map(|d| d.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "device".into()),
            several: snapshot.devices.len() > 1,
        }
    }
    fn delivery_finished(&self, delivery: &crate::outbox::Delivery) {
        if !self.config().notification_preview {
            return;
        }
        let snapshot = self.ui_snapshot();
        if let Some(transfer) = snapshot.transfers.iter().find(|t| {
            t.direction == "outgoing"
                && t.transfer_id == delivery.begin.transfer_id
                && t.device == delivery.authority.device
                && matches!(t.state.as_str(), "sent" | "failed")
        }) {
            let name = snapshot
                .devices
                .iter()
                .find(|d| d.device == transfer.device)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "device".into());
            self.action_queue.delivery(transfer.clone(), name);
        }
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
                .sessions
                .get(device)
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
                .sessions
                .get(device)
                .is_some_and(|s| &s.device == device && Some(s.id) == store.connection_id)
        {
            let mut ui = self.ui.lock().expect("UI state");
            ui.active_transfers.remove(&(device.clone(), id.clone()));
            ui.incoming
                .retain(|t| t.device != *device || t.transfer_id != *id || t.state != "sending");
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
    fn clipboard_rejected(&self, peer: &Hello, id: &Id, kind: &str, nack: &Nack) {
        self.error(Some(peer.device_id.clone()), format!(
            "clipboard delivery rejected: peer_name={:?} transfer={id} kind={kind} phase=receipt code={:?} retryable={} action=dropped",
            peer.name, nack.code, nack.retryable,
        ));
    }
    fn clipboard_refused(&self, peer: &Id, id: &Id, kind: &str, phase: &str, code: NackCode) {
        self.error(
            Some(peer.clone()),
            format!(
                "clipboard receive refused: transfer={id} kind={kind} phase={phase} code={code:?}",
            ),
        );
    }
    fn image_refused(&self, peer: &Id, begin: &FileBegin, phase: &str, code: NackCode) {
        if begin.kind == FileKind::ClipboardImage {
            self.clipboard_refused(peer, &begin.transfer_id, "image", phase, code);
        }
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
        self.setup_invitation_scope.store(true, Ordering::SeqCst);
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
        state.last_unknown = None;
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
        self.forget_pairing_generation(device, reason, expected, remote_name, None)
            .await
    }
    async fn forget_pairing_generation(
        &self,
        device: Id,
        reason: CloseReason,
        expected: Option<u64>,
        remote_name: Option<String>,
        generation: Option<u64>,
    ) -> Result<bool> {
        self.forget_pairing_generation_checked(
            device,
            reason,
            expected,
            remote_name,
            generation,
            None,
        )
        .await
    }
    async fn forget_pairing_generation_checked(
        &self,
        device: Id,
        reason: CloseReason,
        expected: Option<u64>,
        remote_name: Option<String>,
        generation: Option<u64>,
        registration: Option<mesh::LinkEpoch>,
    ) -> Result<bool> {
        let guard = self.pairing_gate.clone().lock_owned().await;
        if matches!(reason, CloseReason::Unpair) {
            ensure!(
                !self
                    .store
                    .setup_state()
                    .roster
                    .iter()
                    .any(|m| m.device_id == device && m.status == MemberStatus::Member),
                "setup membership changed; confirm Remove from all devices"
            );
        }
        if generation.is_some() && self.store.pairing_generation(&device) != generation {
            return Ok(false);
        }
        // A stale local command for the phone replaced by a new pairing must
        // not mark the current phone unpairing or disturb its connection.
        if !self.paired_devices().contains(&device) {
            return Ok(false);
        }
        // Cancellation while waiting for capacity must leave trust and its
        // action scope intact. From here through submission there is no await.
        let reservation = self.store.reserve().await;
        {
            let mut state = self.sessions.lock().expect("sessions");
            if registration.is_some_and(|r| self.link_epoch_locked(&device) != r) {
                return Ok(false);
            }
            if expected.is_some()
                && state
                    .sessions
                    .get(&device)
                    .is_none_or(|s| Some(s.id) != expected || s.device != device)
            {
                return Ok(false);
            }
            state.unpairing.insert(device.clone());
        }
        let scope = self
            .action_scope
            .lock()
            .expect("actions")
            .get(&device)
            .cloned();
        self.clipboard.event(ClipboardEvent::Revoke(device.clone()));
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
        let clipboard = self.clipboard.clone();
        let phone_path = self.paths.data.join("phone.json");
        self.store
            .unpair_reserved(reservation, device.clone(), move |persisted| {
                let _guard = guard;
                let mut scope = actions.lock().expect("actions");
                if persisted {
                    clipboard.event(ClipboardEvent::BindingReset(device.clone()));
                    scope.remove(&device);
                    admission
                        .lock()
                        .expect("admission")
                        .peers
                        .retain(|_, id| *id != device);
                } else {
                    // Old work remains cancelled; restored trust gets a fresh
                    // action scope and can reconnect once storage recovers.
                    scope.insert(device.clone(), ActionScope::new());
                }
                drop(scope);
                let mut state = sessions.lock().expect("sessions");
                let ended = state.sessions.remove(&device);
                if let Some(session) = &ended {
                    session.close.send_replace(Some(if persisted {
                        reason
                    } else {
                        CloseReason::Error(ErrorCode::Closing)
                    }));
                }
                state.unpairing.remove(&device);
                automatic
                    .lock()
                    .expect("automatic clipboard")
                    .invalidate_peer(&device);
                let disconnected = Event::ConnectionChanged {
                    device: device.clone(),
                    connected: false,
                };
                // Capture/persist the ended timestamp before Unpaired invalidates
                // metadata authority, while preserving public Unpaired -> disconnected order.
                update_ui_state(&ui, &disconnected);
                select_display(
                    &state,
                    &admission.lock().expect("admission"),
                    &mut ui.lock().expect("UI state"),
                );
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
                select_display(
                    &state,
                    &admission.lock().expect("admission"),
                    &mut ui.lock().expect("UI state"),
                );
                let _ = events.send(disconnected);
            })
            .await?;
        if let Some(scope) = scope {
            let _idle = scope.active.lock().await;
        }
        if let Err(error) = self.outbox.reconcile().await {
            self.error(None, format!("outbox cleanup deferred: {error}"));
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
        mut config: Config,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<()> {
        let previous = self.config();
        if config.clipboard_mode != previous.clipboard_mode {
            config.clipboard_sync_to_phone =
                config.clipboard_mode != crate::config::ClipboardMode::Off;
        } else if config.clipboard_sync_to_phone != previous.clipboard_sync_to_phone {
            config.clipboard_mode = if config.clipboard_sync_to_phone {
                crate::config::ClipboardMode::Automatic
            } else {
                crate::config::ClipboardMode::Off
            };
        }
        config.validate_with_cache(&self.paths.cache)?;
        let mut hello = self.computer_hello();
        hello.name = config.name.clone();
        encode_frame(&Message::Hello(hello))?;
        ensure!(
            config.port == self.config().port,
            "changing the port takes effect after restarting pneud"
        );
        let path = self.paths.config.clone();
        let clear_error = config.save_path != self.config().save_path || !config.save_to_folder;
        let cancel_automatic = config.clipboard_sync_to_phone
            != self.config().clipboard_sync_to_phone
            || config.clipboard_include_images != self.config().clipboard_include_images
            || config.clipboard_mode != self.config().clipboard_mode;
        let automatic = self.automatic.clone();
        let config_epoch = self.clipboard_config_epoch.clone();
        let clipboard = self.clipboard.clone();
        let ui = self.ui.clone();
        let settings = self.config.clone();
        let retry_disconnected = self.outbox.retry_disconnected.clone();
        let events = self.events.clone();
        let mut member = self.self_member();
        member.name = config.name.clone();
        let saved = config.clone();
        #[cfg(test)]
        let save_store = self.store.clone();
        let history_change = previous.clipboard_history != config.clipboard_history;
        let history_enabled = config.clipboard_history;
        let history_store = self.store.clone();
        let runtime = tokio::runtime::Handle::current();
        let (complete, completed) = tokio::sync::oneshot::channel();
        let publish = move || {
            let _guard = guard;
            let mut automatic = automatic.lock().expect("automatic clipboard");
            if cancel_automatic {
                // Fence effects computed before this change, even if their
                // publication runs after the existing queue is invalidated.
                config_epoch.fetch_add(1, Ordering::SeqCst);
                automatic.invalidate();
                clipboard.event(ClipboardEvent::Policy {
                    allowed: Vec::new(),
                });
            }
            retry_disconnected.store(config.queue_offline, Ordering::SeqCst);
            settings.send_replace(config);
            clipboard.event(ClipboardEvent::Mode(
                if settings.borrow().clipboard_sync_to_phone {
                    settings.borrow().clipboard_mode
                } else {
                    crate::config::ClipboardMode::Off
                },
            ));
            drop(automatic);
            if clear_error {
                let mut ui = ui.lock().expect("UI state");
                ui.snapshot
                    .clear_destination(crate::ui::ReceiveDestination::SaveFolder, None);
            }
            let _ = events.send(Event::ConfigChanged);
        };
        self.store
            .setup_refresh_self_then(
                member,
                move || {
                    #[cfg(test)]
                    save_store.fault("setup_config_after_commit")?;
                    saved.save(&path)
                },
                move || {
                    if history_change {
                        runtime.spawn(async move {
                            let result = history_store
                                .history_configure(history_enabled, false)
                                .await;
                            if result.is_ok() {
                                publish();
                            }
                            let _ = complete.send(result);
                        });
                    } else {
                        publish();
                        let _ = complete.send(Ok(()));
                    }
                },
            )
            .await?;
        completed
            .await
            .context("configuration publication stopped")??;
        Ok(())
    }

    pub async fn send_clipboard(&self, device: Id, text: String) -> Result<Id> {
        self.send_observed_formats(self.explicit_clipboard_offer_for(device)?, Some(text), None)
            .await
    }
    pub(crate) async fn notify_clipboard_too_large(&self) -> Result<()> {
        self.actions.notify_clipboard_too_large().await
    }
    pub async fn share_clipboard(&self, targets: crate::outbox::Targets) -> Result<Id> {
        let mut offer = {
            // Match the displayed pairing as well as the frozen offer's binding.
            // The actor retains that binding through reading, persistence and
            // first-byte authorization; a later pairing can never be substituted.
            let sessions = self.sessions.lock().expect("sessions");
            let admission = self.admission.lock().expect("admission");
            let mut offer = self
                .automatic_target_locked(&sessions)
                .context("no connected clipboard recipients")?;
            match &targets {
                crate::outbox::Targets::Captured(captured) => offer.recipients.retain(|r| {
                    captured.iter().any(|a| {
                        a.device == r.target.device
                            && a.generation == r.target.binding
                            && admission.peers.get(&a.fingerprint) == Some(&a.device)
                    })
                }),
                targets => {
                    if let Some(ids) = targets.explicit()? {
                        offer.recipients.retain(|r| ids.contains(&r.target.device));
                    }
                }
            }
            offer
        };
        ensure!(
            !offer.recipients.is_empty(),
            "no connected clipboard recipients"
        );
        offer.observed = false;
        let id = Id::new(crate::random_hex(16))?;
        self.clipboard
            .share(ClipboardEvent::Share {
                token: self.clipboard.token(),
                id: id.clone(),
                source: self.identity.device_id.clone(),
                offer,
            })
            .await?;
        Ok(id)
    }
    pub async fn set_setting(&self, key: String, value: serde_json::Value) -> Result<()> {
        let guard = self.config_gate.clone().lock_owned().await;
        let mut config = self.config();
        match key.as_str() {
            "clipboard_history" => config.clipboard_history = serde_json::from_value(value)?,
            "clipboard_mode" => config.clipboard_mode = serde_json::from_value(value)?,
            "choose_devices_each_time" => {
                config.choose_devices_each_time = serde_json::from_value(value)?
            }
            "queue_offline" => config.queue_offline = serde_json::from_value(value)?,
            "name" => config.name = serde_json::from_value(value)?,
            "save_to_folder" => config.save_to_folder = serde_json::from_value(value)?,
            "save_path" => config.save_path = serde_json::from_value(value)?,
            "notification_preview" => config.notification_preview = serde_json::from_value(value)?,
            "copy_image_to_clipboard" => {
                config.copy_image_to_clipboard = serde_json::from_value(value)?
            }
            "open_in_viewer" => config.open_in_viewer = serde_json::from_value(value)?,
            "clipboard_include_images" => {
                config.clipboard_include_images = serde_json::from_value(value)?
            }
            "clipboard_sync_to_phone" => {
                config.clipboard_sync_to_phone = serde_json::from_value(value)?
            }
            "cache_retention_days" => config.cache_retention_days = serde_json::from_value(value)?,
            "max_file_bytes" => config.max_file_bytes = serde_json::from_value(value)?,
            "port" => config.port = serde_json::from_value(value)?,
            _ => bail!("unknown setting"),
        }
        self.persist_config(config, guard).await
    }
    pub async fn rename(&self, name: String) -> Result<()> {
        let guard = self.config_gate.clone().lock_owned().await;
        let mut config = self.config();
        config.name = name;
        self.persist_config(config, guard).await
    }
    /// Physical connection for callers starting a new offer. Frozen offers
    /// capture the existing action scope, which survives only mesh duplicates.
    pub fn clipboard_target(&self) -> Option<(Id, u64)> {
        let device = self
            .automatic_target()?
            .recipients
            .first()?
            .target
            .device
            .clone();
        let connection = self
            .sessions
            .lock()
            .expect("sessions")
            .sessions
            .get(&device)?
            .id;
        Some((device, connection))
    }
    fn clipboard_connection_scope(&self, device: &Id, connection: u64) -> u64 {
        self.action_scope
            .lock()
            .expect("actions")
            .get(device)
            .map(|scope| scope.connection_generation.load(Ordering::SeqCst))
            .filter(|generation| *generation != 0)
            .unwrap_or(connection)
    }
    fn automatic_target_current_locked(
        &self,
        sessions: &Connections,
        target: &ClipboardTarget,
    ) -> bool {
        target.config_epoch == self.clipboard_config_epoch.load(Ordering::SeqCst)
            && self.config().clipboard_sync_to_phone
            && self.config().clipboard_mode != crate::config::ClipboardMode::Off
            && !self.store.replacement_pending(&target.device)
            && !sessions.unpairing.contains(&target.device)
            && target.binding == self.store.pairing_generation(&target.device).unwrap_or(0)
            && sessions.sessions.get(&target.device).is_some_and(|s| {
                self.clipboard_connection_scope(&target.device, s.id) == target.connection
                    && (!s.mesh
                        || self.pair_allows(
                            &target.device,
                            &self.identity.device_id,
                            &target.device,
                            "clipboard",
                        ))
                    && !s.paused
                    && !s.close.is_closed()
                    && s.close.borrow().is_none()
            })
            && target.policy == self.store.settings_generation(&target.device)
    }
    fn automatic_recipient_current(&self, target: &ClipboardTarget) -> bool {
        let sessions = self.sessions.lock().expect("sessions");
        self.automatic_target_current_locked(&sessions, target)
    }
    fn automatic_target_locked(&self, sessions: &Connections) -> Option<ClipboardOffer> {
        // Capture the setting values and their epoch atomically with config
        // publication; otherwise old capabilities could acquire the new epoch.
        let _automatic = self.automatic.lock().expect("automatic clipboard");
        let include_images = self.config().clipboard_include_images;
        let mut recipients: Vec<_> = sessions
            .sessions
            .values()
            .map(|s| ClipboardRecipient {
                target: ClipboardTarget {
                    device: s.device.clone(),
                    connection: self.clipboard_connection_scope(&s.device, s.id),
                    binding: self.store.pairing_generation(&s.device).unwrap_or(0),
                    policy: self.store.settings_generation(&s.device),
                    config_epoch: self.clipboard_config_epoch.load(Ordering::SeqCst),
                },
                text: s.capabilities.iter().any(|c| c == "clipboard_text"),
                image: include_images && s.capabilities.iter().any(|c| c == "clipboard_image"),
            })
            .filter(|r| {
                (r.text || r.image) && self.automatic_target_current_locked(sessions, &r.target)
            })
            .collect();
        recipients.sort_by(|a, b| a.target.device.as_str().cmp(b.target.device.as_str()));
        (!recipients.is_empty()).then_some(ClipboardOffer {
            revision: self.clipboard.token(),

            observed: false,
            recipients,
        })
    }
    pub fn automatic_target(&self) -> Option<ClipboardOffer> {
        let sessions = self.sessions.lock().expect("sessions");
        self.automatic_target_locked(&sessions)
    }
    pub fn automatic_offer_current(&self, offer: &ClipboardOffer) -> bool {
        let sessions = self.sessions.lock().expect("sessions");
        offer
            .recipients
            .iter()
            .any(|r| self.automatic_target_current_locked(&sessions, &r.target))
    }
    /// Freeze recipients before asynchronous clipboard reading/debouncing.
    /// Every offer, including clear/secret/unsupported, invalidates prepared work.
    pub fn observe_clipboard_offer(&self) -> Option<ClipboardOffer> {
        let (offer, _) = self.observe_watched_clipboard(false);
        offer
    }
    pub(crate) fn observe_watched_clipboard(&self, origin: bool) -> (Option<ClipboardOffer>, u64) {
        self.observe_watched_clipboard_kind(origin, false, false)
    }
    pub(crate) fn observe_watched_clipboard_kind(
        &self,
        origin: bool,
        clear: bool,
        initial: bool,
    ) -> (Option<ClipboardOffer>, u64) {
        self.observe_watched_clipboard_metadata(origin, clear, initial, false)
    }
    pub(crate) fn observe_watched_clipboard_metadata(
        &self,
        origin: bool,
        clear: bool,
        initial: bool,
        owned: bool,
    ) -> (Option<ClipboardOffer>, u64) {
        let token = self.clipboard.token();
        // A watcher's first event describes existing content, not a new copy.
        // Classify it for explicit sharing, but never freeze automatic recipients.
        let offer = (!initial)
            .then(|| self.automatic_target())
            .flatten()
            .map(|mut offer| {
                offer.revision = token;
                offer.observed = true;
                offer
            });
        self.clipboard.event(ClipboardEvent::Observe {
            history: (!initial && !owned)
                .then(|| self.store.history_capture())
                .flatten(),
            token,
            id: Id::new(crate::random_hex(16)).expect("random id"),
            offer: offer.clone(),
            origin,
            kind: if owned {
                clipboard_actor::ObservationKind::Marker
            } else if initial {
                clipboard_actor::ObservationKind::Initial
            } else {
                clipboard_actor::ObservationKind::Local
            },
            clear,
        });
        (offer, token)
    }
    pub(crate) fn register_clipboard_read(&self, token: u64, cancel: Arc<AtomicBool>) {
        let _ = self
            .clipboard
            .tx
            .send(clipboard_driver::Message::ReadHandle(token, cancel));
    }
    #[cfg(any(target_os = "macos", windows))]
    pub(crate) fn retire_clipboard_observation(&self, token: u64) {
        self.clipboard.event(ClipboardEvent::AbandonRead(token));
    }
    pub(crate) fn clipboard_image_too_large(&self, token: u64, fingerprint: [u8; 32]) {
        self.clipboard
            .event(ClipboardEvent::ImageTooLarge { token, fingerprint });
    }
    pub(crate) fn clipboard_observed(
        &self,
        token: u64,
        content: Option<crate::ui::clipboard::Content>,
        debounce: bool,
    ) {
        let fingerprints = content
            .as_ref()
            .map(clipboard_actor::hashes)
            .unwrap_or_default();
        self.clipboard_observed_hashes(token, content, fingerprints, debounce);
    }
    pub(crate) fn clipboard_observed_hashes(
        &self,
        token: u64,
        content: Option<crate::ui::clipboard::Content>,
        fingerprints: Vec<[u8; 32]>,
        debounce: bool,
    ) {
        self.clipboard.event(ClipboardEvent::Read {
            token,
            content,
            fingerprints,
            debounce,
        });
    }
    /// The watcher runs on its own OS thread. Classify the marker on the actor
    /// before deciding whether the offered bytes need reading. This preserves
    /// the marker's FIFO control ordering without coalescing confirmations.
    pub(crate) fn clipboard_install_marker(&self, ticket: u128) -> bool {
        let (reply, result) = tokio::sync::oneshot::channel();
        if self
            .clipboard
            .tx
            .send(clipboard_driver::Message::InstallMarker(ticket, reply))
            .is_err()
        {
            return false;
        }
        result.blocking_recv().unwrap_or(false)
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn clipboard_test_echo(&self, token: u64) {
        self.clipboard.event(ClipboardEvent::TestEcho(token));
    }
    #[cfg(test)]
    pub(crate) async fn clipboard_test_idle(&self) {
        self.clipboard.barrier().await;
    }
    #[cfg(all(test, unix))]
    pub(crate) async fn clipboard_test_read(
        &self,
        token: u64,
        content: Option<crate::ui::clipboard::Content>,
    ) {
        let reply = self.clipboard.waiter(token);
        self.clipboard_observed(token, content, false);
        let _ = reply.await;
    }
    #[cfg(all(test, target_os = "linux"))]
    async fn wait_clipboard_observation(&self) {
        self.clipboard.barrier().await;
    }
    #[cfg(all(test, unix))]
    pub(crate) async fn observe_local_clipboard_offer(&self) -> Result<Option<ClipboardOffer>> {
        let (offer, token) = self.observe_watched_clipboard(false);
        let reply = self.clipboard.waiter(token);
        self.clipboard_observed(token, None, false);
        reply.await??;
        Ok(offer)
    }
    #[cfg(windows)]
    pub(crate) fn window_closed(&self, request: u64) {
        let _ = self.events.send(Event::WindowClosed { request });
    }
    #[cfg(windows)]
    pub(crate) fn window_close_current(&self, request: u64) -> bool {
        self.window_request() == request && !*self.shutdown.borrow()
    }
    pub(crate) fn set_window_focus(&self, request: u64, focused: bool) {
        if self.window_request.lock().expect("window request").0 == request {
            *self.window_focus.lock().expect("window focus") =
                focused.then_some((request, Instant::now()));
        }
    }
    pub(crate) fn set_window_visible(&self, request: u64, visible: bool) {
        if self.window_request() == request {
            *self.window_visible.lock().expect("window visibility") =
                visible.then_some((request, Instant::now()));
        }
    }
    pub(crate) fn window_visible(&self) -> bool {
        let request = self.window_request();
        self.window_visible
            .lock()
            .expect("window visibility")
            .is_some_and(|(r, at)| r == request && at.elapsed() < Duration::from_secs(2))
    }
    pub(crate) fn window_focused(&self) -> bool {
        let request = self.window_request.lock().expect("window request").0;
        self.window_focus
            .lock()
            .expect("window focus")
            .is_some_and(|(r, at)| r == request && at.elapsed() < Duration::from_secs(2))
    }
    fn clipboard_receive_policy(&self, peer: &Id, kind: Option<FileKind>) -> Vec<PeerSetting> {
        let clipboard = format!("{peer}/{}/clipboard", self.identity.device_id);
        let media = format!("{peer}/{}/automatic_media", self.identity.device_id);
        let automatic = matches!(
            kind,
            Some(FileKind::Screenshot | FileKind::Photo | FileKind::Recording)
        );
        self.store
            .pair_settings
            .lock()
            .expect("pair settings")
            .get(peer)
            .into_iter()
            .flatten()
            .filter(|field| field.key == clipboard || (automatic && field.key == media))
            .cloned()
            .collect()
    }
    fn peer_is_computer(&self, device: &Id) -> bool {
        self.sessions
            .lock()
            .expect("sessions")
            .sessions
            .get(device)
            .is_some_and(|s| s.role == "computer")
    }
    fn mesh_link(&self, device: &Id, connection: u64) -> bool {
        self.sessions
            .lock()
            .expect("sessions")
            .sessions
            .get(device)
            .is_some_and(|s| s.id == connection && s.mesh)
    }
    pub fn cancel_automatic_clipboard(&self, device: &Id) {
        self.clipboard.event(ClipboardEvent::Narrow(device.clone()));
        self.automatic
            .lock()
            .expect("automatic clipboard")
            .invalidate_peer(device);
    }
    #[cfg(all(test, unix))]
    fn take_automatic(&self, device: &Id, connection: u64) -> Option<AutomaticWrite> {
        self.take_automatic_lanes(device, connection, true, true)
    }
    fn take_automatic_lanes(
        &self,
        device: &Id,
        connection: u64,
        text_free: bool,
        file_free: bool,
    ) -> Option<AutomaticWrite> {
        let sessions = self.sessions.lock().expect("sessions");
        let mut automatic = self.automatic.lock().expect("automatic clipboard");
        let work = automatic.selections.get(device)?;
        if !match work.selection {
            OutgoingSelection::Text(_) => text_free,
            OutgoingSelection::Image(_, _) => file_free,
        } {
            return None;
        }
        if sessions.sessions.get(device).is_some_and(|s| s.mesh)
            && self
                .settings_ready
                .lock()
                .expect("settings ready")
                .get(device)
                != Some(&connection)
        {
            return None;
        }
        if work.connection != connection
            || !self.automatic_target_current_locked(&sessions, &work.target)
        {
            return None;
        }
        let work = automatic.selections.remove(device)?;
        automatic.in_flight.insert(
            (
                device.clone(),
                matches!(work.selection, OutgoingSelection::Text(_)),
            ),
            work.clone(),
        );
        Some(work)
    }
    fn publish_automatic(
        &self,
        offer: ClipboardOffer,
        text: Option<OutgoingSelection>,
        image: Option<OutgoingSelection>,
    ) -> Result<()> {
        let sessions = self.sessions.lock().expect("sessions");
        let mut automatic = self.automatic.lock().expect("automatic clipboard");
        ensure!(
            !offer.observed
                || self.config().clipboard_mode == crate::config::ClipboardMode::Automatic,
            "automatic clipboard mode changed"
        );
        let mut recipients = Vec::new();
        for recipient in offer.recipients {
            let target = recipient.target;
            if !self.automatic_target_current_locked(&sessions, &target) {
                continue;
            }
            let session = sessions
                .sessions
                .get(&target.device)
                .expect("current recipient");
            // Reject only the representation for this recipient. A usable
            // fallback or another recipient must survive an encoding failure.
            let selection = [
                (recipient.image, image.as_ref()),
                (recipient.text, text.as_ref()),
            ]
            .into_iter()
            .filter_map(|(supported, selection)| supported.then_some(selection).flatten())
            .find(|selection| encode_frame(&selection.first_frame(session.mesh)).is_ok())
            .cloned();
            if let Some(selection) = selection {
                recipients.push(target.device.clone());
                automatic
                    .latest
                    .insert(target.device.clone(), selection.id().clone());
                automatic.selections.insert(
                    target.device.clone(),
                    AutomaticWrite {
                        target,
                        connection: session.id,
                        selection,
                    },
                );
            }
        }
        ensure!(!recipients.is_empty(), "clipboard offer or policy changed");
        drop(automatic);
        drop(sessions);
        for device in recipients {
            self.wake(&device);
        }
        Ok(())
    }
    fn automatic_offer_for(&self, device: Id, connection: Option<u64>) -> Result<ClipboardOffer> {
        let mut offer = self
            .automatic_target()
            .context("phone is disconnected or paused")?;
        offer.recipients.retain(|r| {
            r.target.device == device
                && connection.is_none_or(|c| {
                    self.sessions
                        .lock()
                        .expect("sessions")
                        .sessions
                        .get(&device)
                        .is_some_and(|s| s.id == c)
                })
        });
        ensure!(!offer.recipients.is_empty(), "connection changed");
        Ok(offer)
    }
    // The actor narrows older work for this peer when it accepts the request.
    fn explicit_clipboard_offer_for(&self, device: Id) -> Result<ClipboardOffer> {
        let sessions = self.sessions.lock().expect("sessions");
        let mut offer = self
            .automatic_target_locked(&sessions)
            .context("phone is disconnected or paused")?;
        offer.recipients.retain(|r| r.target.device == device);
        ensure!(!offer.recipients.is_empty(), "connection changed");
        Ok(offer)
    }
    pub async fn send_automatic_clipboard(
        &self,
        device: Id,
        connection: u64,
        text: String,
    ) -> Result<Id> {
        self.send_observed_clipboard(self.automatic_offer_for(device, Some(connection))?, text)
            .await
    }
    pub async fn send_observed_clipboard(&self, offer: ClipboardOffer, text: String) -> Result<Id> {
        self.send_observed_formats(offer, Some(text), None).await
    }
    pub async fn send_automatic_image(
        &self,
        device: Id,
        connection: u64,
        bytes: Vec<u8>,
    ) -> Result<Id> {
        self.send_observed_image(self.automatic_offer_for(device, Some(connection))?, bytes)
            .await
    }
    pub async fn send_clipboard_image(&self, device: Id, bytes: Vec<u8>) -> Result<Id> {
        self.send_observed_formats(
            self.explicit_clipboard_offer_for(device)?,
            None,
            Some(bytes),
        )
        .await
    }
    pub async fn send_observed_image(&self, offer: ClipboardOffer, bytes: Vec<u8>) -> Result<Id> {
        self.send_observed_formats(offer, None, Some(bytes)).await
    }
    pub(crate) async fn send_observed_formats(
        &self,
        offer: ClipboardOffer,
        text: Option<String>,
        image: Option<Vec<u8>>,
    ) -> Result<Id> {
        use crate::ui::clipboard::Content;
        let text = text.filter(|text| text.len() <= MAX_CLIPBOARD_BYTES && !text.contains('\0'));
        let image = image.filter(|bytes| {
            bytes.len() <= crate::ui::clipboard::MAX_IMAGE
                && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        });
        ensure!(
            !offer
                .payload_plans(text.is_some(), image.is_some())
                .is_empty(),
            "clipboard representation rejected"
        );
        ensure!(
            self.automatic_offer_current(&offer),
            "clipboard offer or policy changed"
        );
        let content = match (text, image) {
            (Some(text), Some(image)) => Content::Formats(text, image),
            (Some(text), None) => Content::Text(text),
            (None, Some(image)) => Content::Image(image),
            _ => anyhow::bail!("clipboard representation rejected"),
        };
        let explicit = !offer.observed;
        let token = self.clipboard.token();
        let id = Id::new(crate::random_hex(16))?;
        let input = self.clipboard_input(Some(content.clone()))?;
        let operation = ClipboardOperation {
            sensitive: false,
            history: None,
            history_id: None,
            token,
            id: id.clone(),
            source: self.identity.device_id.clone(),
            key: None,
            created_at: 0,
            payload: None,
            hashes: clipboard_actor::hashes(&content),
            content: Some(content),
            observation: offer.observed.then_some(offer.revision),
            offer: Some(offer),
            received: false,
            legacy: false,
            restore: false,
            repair: false,
        };
        self.clipboard_frames(&operation, crate::now_ms())?;
        let (reply, _) = tokio::sync::oneshot::channel();
        let outcome = self
            .clipboard
            .submit(
                operation,
                clipboard_driver::Request {
                    input,
                    claim: None,
                    store: self.store.clone(),
                    authority: None,
                    kind: None,
                    policy: Vec::new(),
                    reply,
                },
            )
            .await?;
        // Automatic observations may be classified as echoes. Explicit IPC/UI
        // shares must not return a success ID for a superseded operation.
        ensure!(
            !explicit || outcome.applied,
            "clipboard share was superseded"
        );
        Ok(id)
    }
    fn clipboard_input(
        &self,
        content: Option<crate::ui::clipboard::Content>,
    ) -> Result<crate::actions::ClipboardInput> {
        use crate::{actions::ClipboardInput, ui::clipboard::Content};
        match content.context("clipboard has no readable content")? {
            Content::Text(text) => Ok(ClipboardInput::Text(text)),
            Content::Image(bytes) => Ok(ClipboardInput::Bytes {
                bytes,
                mime: "image/png".into(),
                cache: self.paths.cache.clone(),
            }),
            Content::Formats(text, png) => Ok(ClipboardInput::Formats {
                text,
                png,
                cache: self.paths.cache.clone(),
            }),
        }
    }
    fn clipboard_frames(
        &self,
        operation: &ClipboardOperation,
        hlc: u64,
    ) -> Result<(Option<OutgoingSelection>, Option<OutgoingSelection>)> {
        use crate::ui::clipboard::Content;
        let (text, image) = match operation.content.as_ref() {
            Some(Content::Text(text)) => (Some(text.clone()), None),
            Some(Content::Image(bytes)) => (None, Some(bytes.clone())),
            Some(Content::Formats(text, bytes)) => (Some(text.clone()), Some(bytes.clone())),
            None => (None, None),
        };
        let text = text.filter(|t| t.len() <= MAX_CLIPBOARD_BYTES).map(|text| {
            OutgoingSelection::Text(Clipboard {
                sensitive: false,
                transfer_id: operation.id.clone(),
                text,
                created_at: hlc,
                origin: ClipboardOrigin::Clipboard,
                hlc: Some(hlc),
                source_device: Some(self.identity.device_id.clone()),
            })
        });
        let image = image.map(|bytes| {
            OutgoingSelection::Image(
                FileBegin {
                    resume: None,
                    folder: None,
                    sensitive: false,
                    transfer_id: operation.id.clone(),
                    file_name: "clipboard.png".into(),
                    mime: "image/png".into(),
                    size: bytes.len() as u64,
                    sha256: Hash::new(hex::encode(Sha256::digest(&bytes))).expect("SHA-256"),
                    kind: FileKind::ClipboardImage,
                    created_at: hlc,
                    source_media_id: None,
                    hlc: Some(hlc),
                    source_device: Some(self.identity.device_id.clone()),
                },
                Arc::new(bytes),
            )
        });
        if let Some(offer) = &operation.offer {
            let sessions = self.sessions.lock().expect("sessions");
            ensure!(
                offer.recipients.iter().any(|r| {
                    let mesh = sessions
                        .sessions
                        .get(&r.target.device)
                        .is_some_and(|s| s.mesh);
                    (r.text
                        && text
                            .as_ref()
                            .is_some_and(|t| encode_frame(&t.first_frame(mesh)).is_ok()))
                        || (r.image
                            && image
                                .as_ref()
                                .is_some_and(|i| encode_frame(&i.first_frame(mesh)).is_ok()))
                }),
                "clipboard representation rejected"
            );
        }
        Ok((text, image))
    }
    /// Explicit IPC discard is authorized against the current pairing first.
    pub async fn discard_clipboard(&self, device: Id) -> Result<()> {
        ensure!(
            self.paired_devices().contains(&device),
            "device is not paired"
        );
        self.cancel_automatic_clipboard(&device);
        self.wake(&device);
        Ok(())
    }
    fn wake(&self, device: &Id) {
        if let Some(s) = self.sessions.lock().expect("sessions").sessions.get(device) {
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
        self.outbox.stopping.store(true, Ordering::SeqCst);
        self.shutdown.send_replace(true);
        self.action_stop.send_replace(true);
    }
    pub async fn shutdown_actions(&self) -> Result<()> {
        self.action_stop.send_replace(true);
        self.file_admissions.drain().await;
        self.clipboard.stop().await;
        let clipboard_task = self
            .clipboard_worker
            .lock()
            .expect("clipboard worker")
            .take();
        if let Some(task) = clipboard_task {
            let _ = task.await;
        }
        let task = self.action_worker.lock().expect("actions").take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.actions.shutdown().await
    }
    /// After stopping and joining producers (IPC/UI), wait for detached blocking
    /// saves too. Cancelling their async caller does not cancel Store work.
    pub async fn finish_shutdown(&self) -> Result<()> {
        let actions = self.shutdown_actions().await;
        self.store.drain().await;
        actions
    }
    /// Listener injection supports in-process ephemeral-port tests. The daemon
    /// uses net::listen for [::] (explicitly dual stack on Windows) and enables mDNS.
    pub async fn serve(self: Arc<Self>, listener: TcpListener, advertise: bool) -> Result<()> {
        let listener = Arc::new(listener);
        self.port
            .store(listener.local_addr()?.port(), Ordering::SeqCst);
        #[cfg(test)]
        eprintln!(
            "test Core serving PID={} port={} directory={} mDNS={advertise}",
            std::process::id(),
            self.bound_port(),
            self.paths.data.display()
        );
        let mut mdns = if advertise {
            self.optional(Advertisement::new(&self), "mDNS advertisement")
        } else {
            None
        };
        let semaphore = self.ingress.clone();
        let transports = self.transports.clone();
        let supervisor = mesh::Task::spawn(self.clone().mesh_supervise());
        let mut tasks = JoinSet::new();
        let mut shutdown = self.shutdown.subscribe();
        let mut prune = tokio::time::interval(Duration::from_secs(3600));
        let mut discovery = tokio::time::interval(Duration::from_secs(60));
        let mut recovery_tick = tokio::time::interval(Duration::from_secs(1));
        let mut partial_tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = partial_tick.tick()=>{if let Err(error)=self.store.maintain_partials().await {self.error(None,format!("resume maintenance deferred: {error}"));}},
                _ = recovery_tick.tick() => {if let Err(error)=self.outbox.retry_recovery().await {self.error(None,format!("outbox recovery deferred: {error}"));}},
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
                    if let (Ok(permit), Ok(transport)) = (semaphore.clone().try_acquire_owned(), transports.clone().try_acquire_owned()) {
                        let core = self.clone();
                        tasks.spawn(Box::pin(async move { let _transport = transport; let mut stop = core.shutdown.subscribe(); if *stop.borrow() { return; } let outcome = tokio::select! { biased; _ = stop.changed() => Ok(()), outcome = Box::pin(core.clone().connection(stream, permit)) => outcome }; if let Err(e) = outcome { let reason = if let Some(protocol) = e.downcast_ref::<ProtocolError>() { format!("protocol {:?}",decode_code(protocol)) } else if let Some(io) = e.downcast_ref::<std::io::Error>() { format!("transport {:?}",io.kind()) } else { "transport or local state failure".into() }; core.error(None,format!("connection closed: {reason}")); } }));
                    }
                }
            }
        }
        drop(mdns);
        let ended = {
            let state = self.sessions.lock().expect("sessions");
            state
                .sessions
                .values()
                .map(|session| {
                    session
                        .close
                        .send_replace(Some(CloseReason::Error(ErrorCode::Closing)));
                    (session.device.clone(), session.id)
                })
                .collect::<Vec<_>>()
        };
        for (device, id) in ended {
            if self.finish_connection(&device, id) {
                self.optional(self.persist_phone(id).await, "shutdown last-seen metadata");
            }
        }
        while tasks.join_next().await.is_some() {}
        supervisor.await??;
        self.finish_shutdown().await?;
        #[cfg(test)]
        eprintln!(
            "test Core stopped PID={} port={}",
            std::process::id(),
            self.bound_port()
        );
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
    async fn connection(
        self: Arc<Self>,
        stream: TcpStream,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<()> {
        Box::pin(async move {
            // Capture before TLS/HELLO without waiting behind lifecycle disk
            // work. Registration rechecks this authority under the pairing gate.
            let mut captured = self.store.with_current_pairing();
            let registrations = self.link_epochs();
            let candidate = self.next_candidate.fetch_add(1, Ordering::SeqCst);
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
            let mut tls: TlsStream<TcpStream> = handshake??.into();
            let fp = certificate_fingerprint(
                tls.get_ref()
                    .1
                    .peer_certificates()
                    .context("missing client cert")?[0]
                    .as_ref(),
            );
            let _refresh = self
                .admission
                .lock()
                .expect("admission")
                .peers
                .get(&fp)
                .cloned()
                .map(|device| crate::ui::ScopeReconnect::new(self.ui.clone(), device));
            let already_known = self
                .admission
                .lock()
                .expect("admission")
                .peers
                .contains_key(&fp);
            let (admitted, _setup_incoming) = if already_known {
                (true, None)
            } else {
                let _gate = self.pairing_gate.lock().await;
                let admitted = verifier.admit(&fp);
                let target = if admitted
                    && !self
                        .admission
                        .lock()
                        .expect("admission")
                        .peers
                        .contains_key(&fp)
                {
                    Some(self.track_setup_incoming(candidate, fp.clone()))
                } else {
                    None
                };
                (admitted, target)
            };
            if !admitted {
                if !self.store.setup_quiet_reconnect(fp.clone()).await? {
                    self.event(Event::Blocked {
                        address: peer_address.ip().to_canonical().to_string(),
                        fingerprint: fp.clone(),
                    });
                }
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
            let mut hello = match read_handshake(&mut tls).await? {
                Message::Hello(h) => h,
                Message::Error(error) => {
                    if error.code == ErrorCode::VersionMismatch {
                        let device = self
                            .admission
                            .lock()
                            .expect("admission")
                            .peers
                            .get(&fp)
                            .cloned();
                        if let Some(device) = device
                            && let Some(generation) = captured.pairing_generation(&device)
                        {
                            self.set_link_problem_if(
                                &device,
                                generation,
                                registrations.get(&device).copied().unwrap_or_default(),
                                Some(mesh::LinkProblem::UpdateRequired),
                            );
                        }
                    }
                    return Ok(());
                }
                _ => {
                    send_error(&mut tls, ErrorCode::UnexpectedMessage).await?;
                    return Ok(());
                }
            };
            if hello.device_id == self.identity.device_id {
                send_error(&mut tls, ErrorCode::Unauthorized).await?;
                return Ok(());
            }
            if hello.version != 1 {
                // A certificate alone cannot label a claimed identity. Bind the
                // authenticated fingerprint and captured generation first.
                if self.admission.lock().expect("admission").peers.get(&fp)
                    == Some(&hello.device_id)
                    && let Some(generation) = captured.pairing_generation(&hello.device_id)
                {
                    self.set_link_problem_if(
                        &hello.device_id,
                        generation,
                        registrations
                            .get(&hello.device_id)
                            .copied()
                            .unwrap_or_default(),
                        Some(mesh::LinkProblem::UpdateRequired),
                    );
                }
                send_error(&mut tls, ErrorCode::VersionMismatch).await?;
                return Ok(());
            }
            if hello.capabilities.iter().any(|c| c == "mesh") && hello.role.is_none() {
                send_error(&mut tls, ErrorCode::MalformedMessage).await?;
                return Ok(());
            }
            normalize_legacy_hello(&mut hello, false);
            if _setup_incoming.is_some() {
                let _gate = self.pairing_gate.lock().await;
                if let Some(target) = self
                    .setup_incoming
                    .lock()
                    .expect("setup incoming")
                    .get_mut(&candidate)
                {
                    target.peer = Some(hello.device_id.clone());
                }
            }
            let (mut local_hello, setup_revision) = {
                (
                    self.computer_hello_for(&fp, Some(&hello.device_id), false),
                    self.store.setup_state().generation,
                )
            };
            if let Some(target) = self
                .setup_incoming
                .lock()
                .expect("setup incoming")
                .get(&candidate)
            {
                local_hello.setup = Some(self.store.setup_hello(
                    None,
                    if target.scope {
                        SetupScope::Enabled
                    } else {
                        SetupScope::Disabled
                    },
                ));
            }
            let setup_sent = local_hello.setup.clone().map(|h| (h, setup_revision));
            crate::transport::send(
                &mut tls,
                &Message::Hello(local_hello),
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
                    Message::Members(_) => {
                        send_error(&mut tls, ErrorCode::UnexpectedMessage).await?;
                        return Ok(());
                    }
                    Message::Error(_) => return Ok(()),
                    _ => {
                        send_error(&mut tls, ErrorCode::Unauthorized).await?;
                        return Ok(());
                    }
                };
                self.event(Event::PairingProgress {
                    name: Some(hello.name.clone()),
                });
                let incoming_target = self
                    .setup_incoming
                    .lock()
                    .expect("setup incoming")
                    .get(&candidate)
                    .cloned();
                let result = self
                    .finish_pairing_hello_target(
                        &hello.device_id,
                        &fp,
                        &token,
                        Some(hello.clone()),
                        incoming_target,
                    )
                    .await;
                if let Ok(store) = &result {
                    captured = store.clone();
                }
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
                if hello.role == Some(DeviceRole::Computer)
                    && hello.capabilities.iter().any(|c| c == "mesh")
                {
                    self.event(Event::ComputerPaired {
                        device: hello.device_id.clone(),
                        name: hello.name.clone(),
                    });
                }
                crate::transport::send(&mut tls, &reply, Duration::from_secs(10)).await?;
            }
            verifier.release_provisional();
            self.clone()
                .register_buffered_setup(
                    tls,
                    hello,
                    fp,
                    peer_address,
                    permit,
                    captured,
                    false,
                    allow_recovery,
                    Vec::new(),
                    Some(candidate),
                    setup_sent,
                )
                .await
        })
        .await
    }
    #[cfg(all(test, unix))]
    #[allow(clippy::too_many_arguments)]
    async fn register(
        self: Arc<Self>,
        tls: TlsStream<TcpStream>,
        hello: Hello,
        fp: Hash,
        peer_address: SocketAddr,
        permit: tokio::sync::OwnedSemaphorePermit,
        captured: Store,
        outbound: bool,
        allow_recovery: bool,
    ) -> Result<()> {
        self.register_buffered(
            tls,
            hello,
            fp,
            peer_address,
            permit,
            captured,
            outbound,
            allow_recovery,
            Vec::new(),
            None,
        )
        .await
    }
    #[cfg(all(test, unix))]
    #[allow(clippy::too_many_arguments)]
    async fn register_buffered(
        self: Arc<Self>,
        tls: TlsStream<TcpStream>,
        hello: Hello,
        fp: Hash,
        peer_address: SocketAddr,
        permit: tokio::sync::OwnedSemaphorePermit,
        captured: Store,
        outbound: bool,
        allow_recovery: bool,
        buffered: Vec<Message>,
        started: Option<u64>,
    ) -> Result<()> {
        let sent = Some((
            self.store
                .setup_hello(Some(&hello.device_id), SetupScope::Enabled),
            self.store.setup_state().generation,
        ));
        self.register_buffered_setup(
            tls,
            hello,
            fp,
            peer_address,
            permit,
            captured,
            outbound,
            allow_recovery,
            buffered,
            started,
            sent,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn register_buffered_setup(
        self: Arc<Self>,
        mut tls: TlsStream<TcpStream>,
        mut hello: Hello,
        fp: Hash,
        peer_address: SocketAddr,
        permit: tokio::sync::OwnedSemaphorePermit,
        captured: Store,
        outbound: bool,
        allow_recovery: bool,
        buffered: Vec<Message>,
        started: Option<u64>,
        setup_sent: Option<(SetupHello, u64)>,
    ) -> Result<()> {
        if hello.device_id == self.identity.device_id {
            send_error(&mut tls, ErrorCode::Unauthorized).await?;
            return Ok(());
        }
        if hello.capabilities.iter().any(|c| c == "mesh") && hello.role.is_none() {
            send_error(&mut tls, ErrorCode::MalformedMessage).await?;
            return Ok(());
        }
        normalize_legacy_hello(&mut hello, outbound);
        // Explicit unpairing could occur while HELLO/pairing was underway.
        if self.admission.lock().expect("admission").peers.get(&fp) != Some(&hello.device_id) {
            send_error(&mut tls, ErrorCode::Unauthorized).await?;
            return Ok(());
        }
        if *self.shutdown.borrow() {
            send_error(&mut tls, ErrorCode::Closing).await?;
            return Ok(());
        }
        let (close_tx, mut close_rx) = watch::channel(None);
        let wake = Arc::new(Notify::new());
        // Fail fast while unpair persistence is pending, then serialize the
        // binding recheck and generation capture with pair/unpair callbacks.
        if self
            .sessions
            .lock()
            .expect("sessions")
            .unpairing
            .contains(&hello.device_id)
        {
            send_error(&mut tls, ErrorCode::Unauthorized).await?;
            return Ok(());
        }
        let (registration_error, mut store, action_scope, connection_id) = {
            let _guard = self.pairing_gate.lock().await;
            // Registration and generation-scoped events share this mutex.
            let mut state = self.sessions.lock().expect("sessions");
            // Allocate in admission order, rather than TLS-handshake order.
            let connection_id = self.next_connection.fetch_add(1, Ordering::SeqCst);
            let scope = self
                .action_scope
                .lock()
                .expect("actions")
                .get(&hello.device_id)
                .cloned();
            let registration_error = if state.unpairing.contains(&hello.device_id)
                || captured.pairing_generation(&hello.device_id)
                    != self.store.pairing_generation(&hello.device_id)
                || self.admission.lock().expect("admission").peers.get(&fp)
                    != Some(&hello.device_id)
            {
                Some(ErrorCode::Unauthorized)
            } else if started.is_some_and(|started| {
                scope.as_ref().is_some_and(|scope| {
                    scope.candidate_generation.load(Ordering::SeqCst) > started
                })
            }) && !state
                .sessions
                .get(&hello.device_id)
                .is_some_and(|previous| {
                    previous.mesh
                        && hello.capabilities.iter().any(|c| c == "mesh")
                        && previous.outbound != outbound
                })
            {
                // A newer candidate already registered (possibly disconnected).
                // Only live opposite-direction mesh duplicates still need the
                // deterministic dialer ranking below.
                Some(ErrorCode::ConnectionReplaced)
            } else if state.sessions.len() >= SESSION_LIMIT
                && !state.sessions.contains_key(&hello.device_id)
            {
                Some(ErrorCode::Busy)
            } else if state
                .sessions
                .get(&hello.device_id)
                .is_some_and(|previous| {
                    previous.mesh
                        && hello.capabilities.iter().any(|c| c == "mesh")
                        && previous.outbound != outbound
                        && previous.outbound
                            == (self.identity.device_id.as_str() < hello.device_id.as_str())
                })
            {
                Some(ErrorCode::ConnectionReplaced)
            } else {
                self.record_link_registration(&hello.device_id, connection_id);
                let duplicate_mesh = state.sessions.get(&hello.device_id).is_some_and(|s| s.mesh)
                    && hello.capabilities.iter().any(|c| c == "mesh");
                if let Some(scope) = &scope {
                    if let Some(candidate) = started {
                        scope
                            .candidate_generation
                            .fetch_max(candidate, Ordering::SeqCst);
                    }
                    scope
                        .registration_generation
                        .store(connection_id, Ordering::SeqCst);
                    if !duplicate_mesh {
                        scope
                            .connection_generation
                            .store(connection_id, Ordering::SeqCst);
                        scope.generation_changed.send_replace(connection_id);
                        self.action_queue.discard_invalid();
                    }
                }
                if let Some(previous) = state.sessions.remove(&hello.device_id) {
                    previous
                        .close
                        .send_replace(Some(CloseReason::Error(ErrorCode::ConnectionReplaced)));
                }
                state.sessions.insert(
                    hello.device_id.clone(),
                    Session {
                        role: if hello.role == Some(DeviceRole::Computer) {
                            "computer"
                        } else {
                            "phone"
                        }
                        .into(),
                        outbound,
                        mesh: hello.capabilities.iter().any(|c| c == "mesh"),
                        device: hello.device_id.clone(),
                        id: connection_id,
                        close: close_tx.clone(),
                        wake: wake.clone(),
                        paused: hello.paused.unwrap_or(false),
                        capabilities: hello.capabilities.clone(),
                        phone: crate::ui::Phone {
                            device: Some(hello.device_id.clone()),
                            pairing_generation: self.store.pairing_generation(&hello.device_id),
                            name: hello.name.clone(),
                            route: crate::ui::route(peer_address.ip()).into(),
                            last_seen: crate::now_ms(),
                        },
                    },
                );
                {
                    let mut automatic = self.automatic.lock().expect("automatic clipboard");
                    if duplicate_mesh {
                        automatic.replace_link(&hello.device_id, connection_id);
                    } else {
                        automatic.invalidate_peer(&hello.device_id);
                    }
                }
                self.outbox.note(
                    hello.device_id.clone(),
                    self.store
                        .pairing_generation(&hello.device_id)
                        .expect("paired generation"),
                    hello.capabilities.iter().any(|c| c == "files"),
                );
                self.outbox.note_folders(
                    hello.device_id.clone(),
                    self.store
                        .pairing_generation(&hello.device_id)
                        .expect("paired generation"),
                    hello.capabilities.iter().any(|c| c == "folder_v1")
                        && hello.capabilities.iter().any(|c| c == "file_resume_v1"),
                );
                self.publish_phone(&state, connection_id, &hello, peer_address.ip());
                self.event(Event::PhonePaused {
                    device: hello.device_id.clone(),
                    paused: hello.paused.unwrap_or(false),
                });
                self.event(Event::ConnectionChanged {
                    device: hello.device_id.clone(),
                    connected: true,
                });
                None
            };
            (registration_error, captured, scope, connection_id)
        };
        if let Some(code) = registration_error {
            send_error(&mut tls, code).await?;
            return Ok(());
        }
        if let Some(generation) = store.pairing_generation(&hello.device_id) {
            self.set_link_problem(&hello.device_id, generation, None);
        }
        let _registration = mesh::Registration {
            core: self.clone(),
            device: hello.device_id.clone(),
            id: connection_id,
        };
        // Retain ingress and total transport permits through publication.
        // A replaced socket must not wait for the blocking pool or disk.
        let _metadata = action_scope.as_ref().map(|scope| {
            let store = store.clone();
            let hello = hello.clone();
            let current = scope.registration_generation.clone();
            mesh::Task::spawn(async move {
                while current.load(Ordering::SeqCst) == connection_id {
                    if store
                        .remember_peer_hello(
                            hello.clone(),
                            current.clone(),
                            connection_id,
                            outbound,
                        )
                        .await
                        .is_ok()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Ok(())
            })
        });
        let phone_persistence = self.persist_phone(connection_id);
        let persisted = tokio::select! { biased;
            _ = close_rx.changed() => false,
            result = phone_persistence => {
                self.optional(result, "phone metadata");
                true
            }
        };
        store.connection_id = Some(connection_id);
        store.action_connection_id = action_scope
            .as_ref()
            .map(|scope| scope.connection_generation.load(Ordering::SeqCst));
        store.mesh = hello.capabilities.iter().any(|c| c == "mesh");
        store.file_resume = hello.capabilities.iter().any(|c| c == "file_resume_v1")
            && hello.capabilities.iter().any(|c| c == "files");
        store.local_device = Some(self.identity.device_id.clone());
        store.setup_generation = Some(self.store.setup_state().generation);
        let result = if persisted {
            // The task's total transport permit remains held through READY
            // and teardown. Only the bounded ingress reserve is released.
            drop(permit);
            self.clone()
                .ready(
                    tls,
                    &hello,
                    wake,
                    ReadyState {
                        setup_sent,
                        store,
                        allow_recovery,
                        action_scope,
                        buffered,
                    },
                    outbound,
                    close_tx,
                    close_rx,
                )
                .await
        } else {
            let reason = close_rx
                .borrow()
                .unwrap_or(CloseReason::Error(ErrorCode::Closing));
            close_transport(&mut tls, reason).await
        };
        let current = self.finish_connection(&hello.device_id, connection_id);
        if current {
            self.optional(
                self.persist_phone(connection_id).await,
                "last-seen metadata",
            );
        }
        if current { result } else { Ok(()) }
    }
    fn publish_phone(
        &self,
        state: &Connections,
        connection: u64,
        hello: &Hello,
        address: std::net::IpAddr,
    ) -> bool {
        if state
            .sessions
            .get(&hello.device_id)
            .is_none_or(|s| s.id != connection)
        {
            return false;
        }
        let admission = self.admission.lock().expect("admission");
        let mut ui = self.ui.lock().expect("UI state");
        ui.phone_generations
            .insert(hello.device_id.clone(), connection);
        ui.phones.insert(
            hello.device_id.clone(),
            crate::ui::Phone {
                device: Some(hello.device_id.clone()),
                pairing_generation: self.store.pairing_generation(&hello.device_id),
                name: hello.name.clone(),
                route: crate::ui::route(address).into(),
                last_seen: crate::now_ms(),
            },
        );
        select_display(state, &admission, &mut ui);
        true
    }
    fn persist_phone(
        &self,
        connection: u64,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        // Lifecycle publication already happened. Schedule its follow-up before
        // returning a future, and coalesce storms instead of queuing one job per
        // canceled connection. The writer still rechecks generation at the disk
        // boundary; each device retains only its latest pending generation.
        let device = self
            .ui
            .lock()
            .expect("UI state")
            .phone_generations
            .iter()
            .find_map(|(device, id)| (*id == connection).then(|| device.clone()));
        let completion = device.map(|device| {
            let mut writes = self.phone_writes.lock().expect("phone writes");
            writes.pending.insert(device, connection);
            if let Some(completion) = &writes.completion {
                return completion.clone();
            }
            let pending = self.phone_writes.clone();
            let ui = self.ui.clone();
            let path = self.paths.data.join("phone.json");
            let job = self.store.start_completion(move |_store| {
                let mut outcome = Ok(());
                loop {
                    let batch = {
                        let mut writes = pending.lock().expect("phone writes");
                        if writes.pending.is_empty() {
                            writes.completion.take();
                            return outcome;
                        }
                        std::mem::take(&mut writes.pending)
                    };
                    for generation in batch.into_values() {
                        #[cfg(test)]
                        if let Err(error) = _store.fault("persist_phone") {
                            outcome = Err(error);
                            continue;
                        }

                        if let Err(error) = persist_phone_generation(&ui, &path, generation) {
                            outcome = Err(error);
                        }
                    }
                }
            });
            let completion = async move {
                job.await
                    .map_err(|error| Arc::new(error.to_string()))?
                    .map_err(|error| Arc::new(error.to_string()))
            }
            .boxed()
            .shared();
            writes.completion = Some(completion.clone());
            completion
        });
        async move {
            if let Some(completion) = completion {
                return completion
                    .await
                    .map_err(|error| anyhow::Error::msg((*error).clone()));
            }
            Ok(())
        }
    }
    #[cfg(all(test, unix))]
    async fn finish_pairing(
        &self,
        device: &Id,
        fp: &Hash,
        token: &Hash,
    ) -> std::result::Result<(), PairCode> {
        self.finish_pairing_captured(device, fp, token)
            .await
            .map(|_| ())
    }
    #[cfg(all(test, unix))]
    async fn finish_pairing_captured(
        &self,
        device: &Id,
        fp: &Hash,
        token: &Hash,
    ) -> std::result::Result<Store, PairCode> {
        self.finish_pairing_hello(device, fp, token, None).await
    }
    #[cfg(test)]
    async fn finish_pairing_hello(
        &self,
        device: &Id,
        fp: &Hash,
        token: &Hash,
        hello: Option<Hello>,
    ) -> std::result::Result<Store, PairCode> {
        self.finish_pairing_hello_target(device, fp, token, hello, None)
            .await
    }
    async fn finish_pairing_hello_target(
        &self,
        device: &Id,
        fp: &Hash,
        token: &Hash,
        hello: Option<Hello>,
        target: Option<setup::IncomingTarget>,
    ) -> std::result::Result<Store, PairCode> {
        // PAIR_REQUEST carries only a token; its remote ID comes from HELLO.
        // Refuse it before consuming the invitation or allocating a binding.
        if device == &self.identity.device_id {
            return Err(PairCode::InvalidToken);
        }
        let guard = self.pairing_gate.clone().lock_owned().await;
        if target
            .as_ref()
            .is_some_and(|v| v.cancelled.load(Ordering::SeqCst))
        {
            return Err(PairCode::InvalidToken);
        }
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
        let hello = hello.map(|mut hello| {
            normalize_legacy_hello(&mut hello, false);
            hello
        });
        self.persist_pairing_hello_scoped(
            device,
            fp,
            guard,
            true,
            hello,
            None,
            target.map(|t| t.scope),
        )
        .await
        .map_err(|_| PairCode::StorageFailed)
    }
    #[cfg(all(test, unix))]
    async fn persist_pairing(
        &self,
        device: &Id,
        fp: &Hash,
        guard: tokio::sync::OwnedMutexGuard<()>,
        consume_invitation: bool,
    ) -> Result<Store> {
        self.persist_pairing_hello(device, fp, guard, consume_invitation, None, None)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn persist_pairing_hello(
        &self,
        device: &Id,
        fp: &Hash,
        guard: tokio::sync::OwnedMutexGuard<()>,
        consume_invitation: bool,
        hello: Option<Hello>,
        installed: Option<Arc<AtomicU64>>,
    ) -> Result<Store> {
        self.persist_pairing_hello_scoped(
            device,
            fp,
            guard,
            consume_invitation,
            hello,
            installed,
            None,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn persist_pairing_hello_scoped(
        &self,
        device: &Id,
        fp: &Hash,
        guard: tokio::sync::OwnedMutexGuard<()>,
        consume_invitation: bool,
        hello: Option<Hello>,
        installed: Option<Arc<AtomicU64>>,
        scope: Option<bool>,
    ) -> Result<Store> {
        ensure!(
            device != &self.identity.device_id,
            "remote claimed local device ID"
        );
        let captured = self.store.for_next_pairing(device);
        let installed_generation = captured
            .pairing_generation(device)
            .context("missing install generation")?;
        let admission = self.admission.clone();
        let events = self.events.clone();
        let actions = self.action_scope.clone();
        let pending = self.action_queue.clone();
        let sessions = self.sessions.clone();
        let ui = self.ui.clone();
        let automatic = self.automatic.clone();
        let clipboard = self.clipboard.clone();
        let device = device.clone();
        let fp = fp.clone();
        let invitation_changes = self.invitation_changes.clone();
        let local_scope = scope.unwrap_or_else(|| {
            if consume_invitation {
                self.setup_invitation_scope.load(Ordering::SeqCst)
            } else {
                self.mesh.join_scope(&fp)
            }
        });
        let route_proven = !consume_invitation
            && hello
                .as_ref()
                .is_some_and(|h| h.addresses.as_ref().is_some_and(|v| !v.is_empty()));
        let old_pin = self
            .admission
            .lock()
            .expect("admission")
            .peers
            .iter()
            .find(|(pin, id)| **id == device && **pin != fp)
            .map(|(pin, _)| pin.clone());
        let mesh = self.mesh.clone();
        let incoming = self.setup_incoming.clone();
        let lifecycle_store = self.store.clone();
        let setup_generation = self.store.setup_state().generation;
        let offline = self
            .ui_snapshot()
            .devices
            .into_iter()
            .filter(|d| !self.connected_devices().contains(&d.device))
            .map(|d| (d.device, d.last_seen))
            .collect();
        self.store
            .pair_then_hello_replacement(
                device.clone(),
                fp.clone(),
                hello,
                Some(self.self_member()),
                if local_scope {
                    SetupScope::Enabled
                } else {
                    SetupScope::Disabled
                },
                route_proven,
                offline,
                move || {
                    let _guard = guard;
                    if let Some(pin) = old_pin {
                        mesh.cancel_removed(&[(device.clone(), pin.clone())]);
                        for target in incoming.lock().expect("setup incoming").values() {
                            if target.pin == pin
                                && target.peer.as_ref().is_none_or(|p| *p == device)
                            {
                                target.cancelled.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    let mut connections = sessions.lock().expect("sessions");
                    connections.unpairing.remove(&device);
                    automatic
                        .lock()
                        .expect("automatic clipboard")
                        .invalidate_peer(&device);
                    if let Some(old) = connections.sessions.remove(&device) {
                        old.close
                            .send_replace(Some(CloseReason::Error(ErrorCode::Unauthorized)));
                    }
                    clipboard.event(ClipboardEvent::BindingReset(device.clone()));
                    let mut scopes = actions.lock().expect("actions");
                    if let Some(old) = scopes.insert(device.clone(), ActionScope::new()) {
                        old.cancelled.send_replace(true);
                    }
                    pending.discard_invalid();
                    let mut state = admission.lock().expect("admission");
                    state.peers.retain(|_, id| *id != device);
                    state.peers.insert(fp, device.clone());
                    // The attempt owns this marker. Publish in the uncancelable
                    // commit callback so retry cannot mistake its own install for
                    // a newer local decision, even if its waiter disappears.
                    if let Some(installed) = installed {
                        installed.store(installed_generation, Ordering::SeqCst);
                    }
                    if consume_invitation && let Some(invitation) = state.invitation.take() {
                        ui.lock().expect("UI state").snapshot.pairing_completion =
                            Some(crate::ui::PairingCompletion {
                                token: invitation.token,
                                device: device.clone(),
                            });
                    }
                    invitation_changes.send_modify(|r| *r = r.wrapping_add(1));
                    drop(state);
                    let setup = lifecycle_store.setup_state();
                    if setup.generation != setup_generation {
                        for session in connections.sessions.values() {
                            if session.capabilities.iter().any(|c| c == "setup")
                                && setup.links.iter().any(|l| {
                                    l.device == session.device && l.scope != SetupScope::Disabled
                                })
                            {
                                ui.lock()
                                    .expect("UI state")
                                    .refreshing_scope(&session.device);
                                session
                                    .close
                                    .send_replace(Some(CloseReason::Error(ErrorCode::Closing)));
                            }
                        }
                    }
                    emit_event(&ui, &events, Event::Paired { device });
                    select_display(
                        &connections,
                        &admission.lock().expect("admission"),
                        &mut ui.lock().expect("UI state"),
                    );
                },
            )
            .await?;
        Ok(captured)
    }
    #[allow(clippy::too_many_arguments)]
    async fn ready(
        self: Arc<Self>,
        tls: TlsStream<TcpStream>,
        hello: &Hello,
        wake: Arc<Notify>,
        state: ReadyState,
        outbound: bool,
        close_tx: watch::Sender<Option<CloseReason>>,
        mut close: watch::Receiver<Option<CloseReason>>,
    ) -> Result<()> {
        Box::pin(async move {

        let ReadyState {
            setup_sent,
            store,
            allow_recovery,
            action_scope,
            buffered,
        } = state;
        let mut config = self.config.subscribe();
        let mut settings_changes=store.settings_changed.subscribe();
        let mut setup_changes=store.setup_changed.subscribe();
        let (mut reader, writer) = tokio::io::split(tls);
        let activity = Arc::new(Mutex::new(Instant::now()));
        let mut writer = crate::transport::ActivityWriter { writer, sent: activity.clone() };
        let (write_tx, write_rx) = mpsc::channel::<Message>(32);
        self.attach_phone_control(hello, &store, write_tx.clone());
        let write_rx = phone_controls::FrameQueue::new(write_rx, hello, &store);
        let mut phone_tick = tokio::time::interval(Duration::from_secs(1));
        phone_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let (priority_tx, priority_rx) = mpsc::channel::<Message>(8);
        let (automatic_tx, automatic_rx) = mpsc::channel::<AutomaticWrite>(1);
        let (automatic_text_tx, automatic_text_rx) = mpsc::channel::<AutomaticWrite>(1);
        let (outbox_tx, outbox_rx) = mpsc::channel::<FileWrite>(1);
        let (reports_tx, mut reports_rx) = mpsc::channel::<WriteReport>(4);
        let (read_tx, mut read_rx) = mpsc::channel::<Message>(8);
        let (file_tx, file_rx) = mpsc::channel::<ApplicationMessage>(4);
        let (text_tx, mut text_rx) = mpsc::channel::<Message>(2);
        let writer_core = self.clone();
        let writer_close = close.clone();
        let writer_task = tokio::spawn(Box::pin(async move {
            writer_loop(writer_core, &mut writer, WriterQueues {
                normal: write_rx, priority: priority_rx, automatic: automatic_rx, text: automatic_text_rx, files: outbox_rx,
                reports: reports_tx, close: writer_close,
                #[cfg(test)]
                before_queue_poll: None,
            }).await
        }));
        let reader_signal = close_tx.clone();
        let ping=Arc::new(Mutex::new(None));
        let heartbeat_task=tokio::spawn(crate::transport::heartbeat(outbound,activity,priority_tx.clone(),ping.clone(),close_tx.clone()));
        let mut reader_close = close.clone();
        let readiness = Arc::new(Notify::new());
        let needs_evidence = !outbound && hello.role == Some(DeviceRole::Computer);
        let _readiness = needs_evidence.then(|| self.persist_readiness(
            store.clone(), hello.device_id.clone(), if allow_recovery {None}else{Some(readiness.clone())},
        ));
        let reader_task = tokio::spawn(Box::pin(async move {
            for message in buffered {
                if !enqueue(&read_tx, message, &mut reader_close, None).await? { return Ok(()); }
            }
            loop {
                let frame=tokio::select! {biased;
                    _=reader_close.changed()=>return Ok(()),
                    frame=read_ready_frame(&mut reader,outbound)=>frame,
                };
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
                    Message::Pong(p) if outbound => {
                        {
                            let mut outstanding=ping.lock().expect("heartbeat");
                            ensure!(outstanding.as_ref().is_some_and(|(nonce, _)| *nonce == p.nonce), "unsolicited PONG");
                            *outstanding = None;
                        }
                        true
                    }
                    Message::Ping(p) => {
                        // Only the dialer's READY-loop traffic proves it has
                        // persisted trust. PONG on a pairing link does not.
                        if needs_evidence { readiness.notify_one(); }
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
        let mirror_scope = action_scope.clone();
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
            heartbeat: heartbeat_task,
            writer: writer_task,
            file: file_task,
            text: text_task,
        };
        let mut writer_finished = false;
        let mut file_pending: Option<crate::outbox::Delivery> = None;
        let mut file_ready: Option<(Id,u64,tokio::sync::oneshot::Sender<Option<u64>>)> = None;
        let mut cancelled_files=HashMap::<Id,(bool,bool)>::new();
        let mut file_receipt_ready:Option<Arc<AtomicBool>>=None;
        let mut queued_cancels=HashSet::new();
        let mut file_preparations = JoinSet::new();
        let mut file_poll = Instant::now();
        let generation = store.pairing_generation(&hello.device_id).context("pairing changed")?;
        let mut recovery = None;
        let mut pending: Option<PendingOutgoing> = None;
        let mut text_pending: Option<PendingOutgoing> = None;
        let mut recovered_pairing = !allow_recovery;
        let mut settings_pending: Option<PendingOutgoing> = None;
        let mut settings_snapshot: Option<PeerSettings> = None;
        let mut settings_confirmed: Option<Vec<PeerSetting>> = None;
        let mut settings_received = !store.mesh;
        let mut settings_retry: Option<Instant> = None;
        let mut members_pending: Option<PendingOutgoing> = None;
        let mut members_snapshot: Option<Members> = None;
        let mut members_confirmed: Option<Vec<Member>> = None;
        let mut members_retry: Option<Instant> = None;
        let mut members_blocked=false;
        let mut members_init_retry: Option<Instant> = None;
        let mut phone_notices = phone_notifications::Worker::start(self.clone(), store.clone(), hello.device_id.clone(), mirror_scope, close.clone());
        let mut notice_stream = phone_notifications::Stream::default();
        let setup_negotiated=hello.capabilities.iter().any(|c|c=="setup");
        // Legacy READY evidence is bookkeeping, not a prerequisite for control
        // traffic. Poll one bounded store job alongside the control loop; drop
        // cancels its admission wait and admitted work remains store-tracked.
        let legacy_ready=async {
            loop {
                match self.setup_ready(&store,hello,setup_sent.clone()).await {
                    Ok(_)=>break,
                    Err(error)=>self.error(Some(hello.device_id.clone()),format!("membership initialization deferred: {error}")),
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        };
        tokio::pin!(legacy_ready);
        let mut legacy_ready_pending=!setup_negotiated;
        let mut members_fence=if setup_negotiated {match self.setup_ready(&store,hello,setup_sent.clone()).await {
            Ok(fence)=>fence,
            Err(error)=>{self.error(Some(hello.device_id.clone()),format!("membership initialization deferred: {error}"));members_init_retry=Some(Instant::now()+Duration::from_secs(60));None},
        }} else {None};
        let result:Result<()>=async {
            if let Err(error)=self.initialize_settings(&store,hello).await {
                self.error(Some(hello.device_id.clone()),format!("settings initialization deferred: {error}"));
                settings_retry=Some(Instant::now()+Duration::from_secs(60));
            }
            loop {
                if close.borrow().is_some() || !self.connection_current(&store, &hello.device_id) {break;}
                if members_init_retry.is_some_and(|at|at<=Instant::now()) {
                    match self.setup_ready(&store,hello,setup_sent.clone()).await {
                        Ok(fence)=>{members_fence=fence;members_init_retry=None;},
                        Err(error)=>{self.error(Some(hello.device_id.clone()),format!("membership initialization deferred: {error}"));members_init_retry=Some(Instant::now()+Duration::from_secs(60));},
                    }
                }
                if let Some(fence)=&members_fence {
                    let state=store.setup_state();
                    if state.generation!=fence.generation {break;}
                    if !members_blocked && members_pending.is_none() && members_retry.is_none_or(|at|at<=Instant::now()) && members_confirmed.as_ref()!=Some(&state.roster) {
                        match store.members_outgoing(fence.clone()).await {
                            Ok(snapshot)=>{
                                if !enqueue(&write_tx,Message::Members(snapshot.clone()),&mut close,Some(Instant::now()+Duration::from_secs(60))).await? {break;}
                                members_pending=Some(PendingOutgoing::queued(snapshot.transfer_id.clone()));members_snapshot=Some(snapshot);members_retry=None;
                            },
                            Err(error)=>{self.error(Some(hello.device_id.clone()),format!("membership persistence failed: {error}"));members_retry=Some(Instant::now()+Duration::from_secs(60));},
                        }
                    }
                }
                if store.mesh && !self.store.replacement_pending(&hello.device_id) {
                    let fields=self.store.pair_settings.lock().expect("pair settings").get(&hello.device_id).cloned().unwrap_or_default();
                    if settings_pending.is_none() && settings_retry.is_none_or(|at|at<=Instant::now()) && settings_confirmed.as_ref()!=Some(&fields) {
                        match store.settings_outgoing(self.identity.device_id.clone(),hello.device_id.clone(),hello.role==Some(DeviceRole::Phone)).await {
                            Ok(snapshot)=>{
                                if !enqueue(&write_tx,Message::PeerSettings(snapshot.clone()),&mut close,Some(Instant::now()+Duration::from_secs(60))).await? {break;}
                                settings_pending=Some(PendingOutgoing::queued(snapshot.transfer_id.clone()));
                                settings_snapshot=Some(snapshot);
                                settings_retry=None;
                            },
                            Err(error)=>{self.error(Some(hello.device_id.clone()),format!("settings persistence failed: {error}"));settings_retry=Some(Instant::now()+Duration::from_secs(60));},
                        }
                    }
                    let synchronized=settings_received && settings_pending.is_none() && settings_confirmed.as_ref()==Some(&fields);
                    self.publish_settings_ready(&store, &hello.device_id, synchronized);
                }
                if file_pending.as_ref().is_some_and(|d|!self.outbox.current(d)) {
                    let delivery=file_pending.take().expect("pending file");
                    if !store.file_resume {break;}
                    cancelled_files.insert(delivery.begin.transfer_id.clone(),(file_ready.is_some(),file_receipt_ready.as_ref().is_some_and(|p|p.load(Ordering::SeqCst))));
                    if let Some((_,_,ready))=file_ready.take(){let _=ready.send(None);}
                    pending=None;
                }
                // Reserve one writer slot in select! below, alongside reports.
                // Filling the queue here can deadlock against the writer's
                // bounded report queue during a bulk reconnect cancellation.
                let next_cancel=if store.file_resume {
                    self.outbox.pending_cancels(hello.device_id.clone(),generation).await?
                        .into_iter().find(|id|!queued_cancels.contains(id))
                } else {None};
                if pending.is_none() && self.outbox.has_work(&hello.device_id) && Instant::now()>=file_poll && self.content_ready(&store,&hello.device_id)
                    && hello.capabilities.iter().any(|c|c=="files") {
                    file_poll=Instant::now()+Duration::from_secs(1);
                    recovery.get_or_insert_with(||self.arm_file_recovery(hello.device_id.clone(),generation,store.connection_id.expect("connection id")));
                    let claim_core=self.clone();let claim_store=store.clone();let claim_device=hello.device_id.clone();
                    match self.outbox.claim_while(hello.device_id.clone(),generation,store.connection_id.expect("connection id"),move ||claim_core.connection_current(&claim_store,&claim_device)).await {
                        Ok(Some(delivery))=>{
                            if delivery.begin.folder.is_some() && (!store.file_resume || !hello.capabilities.iter().any(|c|c=="folder_v1")) {
                                let reason = if hello.role == Some(pneu_proto::DeviceRole::Phone) { "Can't receive folders yet" } else { "Needs a Pneu update" };
                                self.outbox.fail(delivery.clone(),reason.into()).await?;
                                self.delivery_finished(&delivery);
                                continue;
                            }
                            let id=delivery.begin.transfer_id.clone();
                            // Hashing belongs to a preparation job. The transport
                            // writer continues servicing heartbeat and text.
                            let core=self.clone();let preparing=delivery.clone();let connection=store.connection_id.expect("connection id");let mesh=store.mesh;
                            file_preparations.spawn(async move {
                                let payload=if preparing.begin.folder.is_some() {core.outbox.folder_reader(preparing.clone()).await.map(file_sender::Payload::from)} else {core.outbox.reader(preparing.clone()).await.map(file_sender::Payload::from)};
                                let result=payload.map(|file|FileWrite {delivery:preparing.clone(),connection,mesh,file:Some(file),ready:None,receipt_ready:Arc::new(AtomicBool::new(false))});
                                (preparing,result)
                            });
                            file_receipt_ready=None;
                            file_pending=Some(delivery);pending=Some(PendingOutgoing::queued(id));
                        },
                        Ok(None)=>{},
                        Err(error)=>{self.error(Some(hello.device_id.clone()),format!("outbox storage deferred: {error}"));file_poll=Instant::now()+Duration::from_secs(60);},
                    }
                }
                if file_poll<=Instant::now() {file_poll=Instant::now()+Duration::from_secs(1);}
                if self.content_ready(&store,&hello.device_id)
                    && let Some(work) = self.take_automatic_lanes(&hello.device_id, store.connection_id.expect("connection id"),text_pending.is_none(),pending.is_none()) {
                    let id = work.selection.id().clone();
                    let is_text=matches!(work.selection,OutgoingSelection::Text(_));
                    let supported = match &work.selection {
                        OutgoingSelection::Text(_) => hello.capabilities.iter().any(|c|c=="clipboard_text"),
                        OutgoingSelection::Image(_, _) => self.config().clipboard_include_images && hello.capabilities.iter().any(|c|c=="clipboard_image"),
                    };
                    if supported {
                        let deadline = Instant::now()+Duration::from_secs(60);
                        let queued = tokio::select! { biased;
                            _=close.changed()=>false,
                            sent=tokio::time::timeout_at(deadline, async {if is_text {automatic_text_tx.send(work).await} else {automatic_tx.send(work).await}})=>{sent??;true},
                        };
                        if !queued {break;}
                        if is_text {text_pending=Some(PendingOutgoing::queued(id));} else {pending=Some(PendingOutgoing::queued(id));}
                    }
                }
                let content_deadline=pending.as_ref().and_then(|p|p.deadline).into_iter().chain(text_pending.as_ref().and_then(|p|p.deadline)).min();
                let settings_deadline=settings_pending.as_ref().and_then(|p|p.deadline);
                let members_deadline=members_pending.as_ref().and_then(|p|p.deadline);
                let receipt_deadline=content_deadline.into_iter().chain(settings_deadline).chain(members_deadline).min().unwrap_or_else(||Instant::now()+Duration::from_secs(86400));
                tokio::select! {
                    biased;
                    _=close.changed()=>break,
                    _=&mut legacy_ready,if legacy_ready_pending=>{legacy_ready_pending=false;},
                    Some(report)=reports_rx.recv()=>{
                        match report {
                            WriteReport::CancelSent(id) => {self.outbox.cancel_sent(hello.device_id.clone(),id,generation).await?;}
                            WriteReport::Canceled(id) => {
                                if pending.as_ref().is_some_and(|p| p.id == id) { pending=None; }
                                if text_pending.as_ref().is_some_and(|p| p.id == id) { text_pending=None; }
                            }
                            WriteReport::Sent { id, at } => {
                                if let Some(pending) = pending.as_mut() { pending.sent(&id, at); }
                                if let Some(pending) = settings_pending.as_mut() { pending.sent(&id, at); }
                                if let Some(pending) = members_pending.as_mut() { pending.sent(&id, at); }
                                if let Some(pending) = text_pending.as_mut() { pending.sent(&id, at); }
                            }
                        }
                    },
                    permit=write_tx.reserve(),if next_cancel.is_some()=>{
                        let id=next_cancel.expect("pending cancel");
                        queued_cancels.insert(id.clone());
                        cancelled_files.entry(id.clone()).or_insert((false,false));
                        permit?.send(Message::Cancel(Cancel{transfer_id:id}));
                    },
                    prepared=file_preparations.join_next(),if !file_preparations.is_empty()=>{
                        let (delivery,prepared)=prepared.context("file preparation missing")??;
                        let id=delivery.begin.transfer_id.clone();
                        let mut work=match self.outbox.preparation_result(&delivery,prepared) {
                            Ok(Some(work))=>work,
                            Ok(None)=>{
                                self.delivery_finished(&delivery);
                                if pending.as_ref().is_some_and(|p|p.id==id) {pending=None;}
                                if file_pending.as_ref().is_some_and(|p|p.begin.transfer_id==id) {file_pending=None;}
                                continue;
                            }
                            Err(error) if error.downcast_ref::<crate::folders::SourceChanged>().is_some()=>{
                                // A changed folder fails only this delivery; the
                                // session and other work continue.
                                self.outbox.fail_changed(delivery.clone()).await?;
                                self.delivery_finished(&delivery);
                                if pending.as_ref().is_some_and(|p|p.id==id) {pending=None;}
                                if file_pending.as_ref().is_some_and(|p|p.begin.transfer_id==id) {file_pending=None;}
                                continue;
                            }
                            Err(error)=>{
                                if let Err(storage)=self.outbox.fail(delivery.clone(),error.to_string()).await {self.error(None,format!("outbox failure persistence deferred: {storage}"));}
                                self.delivery_finished(&delivery);
                                return Err(error);
                            }
                        };
                        if !self.outbox.current(&work.delivery) {continue;}
                        self.outbox.negotiate_resume(work.delivery.clone(),store.file_resume).await?;
                        file_receipt_ready=Some(work.receipt_ready.clone());
                        if store.file_resume {
                            let (tx,rx)=tokio::sync::oneshot::channel();
                            file_ready=Some((work.delivery.begin.transfer_id.clone(),work.delivery.begin.size,tx));work.ready=Some(rx);
                        }
                        if !enqueue(&outbox_tx,work,&mut close,Some(Instant::now()+Duration::from_secs(60))).await? {break;}
                    },
                    writer=&mut workers.writer=>{writer_finished=true;if close.borrow().is_none() {writer??;}break;},
                    _=tokio::time::sleep_until(receipt_deadline),if content_deadline.is_some() || settings_deadline.is_some() || members_deadline.is_some()=>{let _=close_tx.send(Some(CloseReason::Error(ErrorCode::Timeout)));bail!("outgoing receipt timeout");},
                    _=retry_timer(members_init_retry),if members_init_retry.is_some()=>{},
                    _=retry_timer(members_retry),if members_retry.is_some()=>{members_retry=None;},
                    _=retry_timer(settings_retry),if settings_retry.is_some()=>{settings_retry=None;},
                    _=tokio::time::sleep_until(file_poll),if pending.is_none()=>{},
                    _=wake.notified()=>{file_poll=Instant::now();},
                    _=phone_tick.tick()=>{self.expire_phone_control(hello, &store).await;},
                    _=config.changed()=>{},
                    _=settings_changes.changed()=>{},
                    _=setup_changes.changed()=>{},
                    message=read_rx.recv()=>{
                        let Some(message)=message else {break;};
                        if !self.connection_current(&store, &hello.device_id) {break;}
                        let message = match self.validate_clipboard_message(&store, &hello.device_id, message).await {
                            Ok(message) => message,
                            Err(error) => {
                                self.error(Some(hello.device_id.clone()), error.to_string());
                                close_tx.send_replace(Some(CloseReason::Error(ErrorCode::MalformedMessage)));
                                break;
                            }
                        };
                        let stamp = match &message {
                            Message::Clipboard(text) => text.hlc,
                            Message::FileBegin(begin) => begin.hlc,
                            _ => None,
                        };
                        let mut refusal = None;
                        if let Some(hlc) = stamp && store.observe_hlc(hello.device_id.clone(),hlc).await.is_err() {
                            match &message {
                                Message::Clipboard(text) => self.clipboard_refused(&hello.device_id,&text.transfer_id,"text","observe_hlc",NackCode::StorageFailed),
                                Message::FileBegin(begin) => self.clipboard_refused(&hello.device_id,&begin.transfer_id,if begin.kind==FileKind::ClipboardImage {"image"} else {"file"},"observe_hlc",NackCode::StorageFailed),
                                _ => {},
                            }
                            if let Message::Clipboard(text) = &message {
                                if !enqueue(&write_tx,nack(text.transfer_id.clone(),NackCode::StorageFailed),&mut close,Some(receipt_deadline)).await? {break;}
                                continue;
                            }
                            refusal = Some(NackCode::StorageFailed);
                        }
                        match message {
                            Message::FileReady(ready)=>{
                                ensure!(store.file_resume,"unnegotiated FILE_READY");
                                if let Some((allowed,_))=cancelled_files.get_mut(&ready.transfer_id) {ensure!(*allowed,"duplicate or unsolicited cancelled FILE_READY");*allowed=false;continue;}
                                let (id,size,reply)=file_ready.take().context("unsolicited FILE_READY")?;
                                ensure!(id==ready.transfer_id,"wrong FILE_READY ID");
                                if ready.offset>size {close_tx.send_replace(Some(CloseReason::Error(ErrorCode::MalformedMessage)));bail!("FILE_READY offset exceeds size");}
                                let _=reply.send(Some(ready.offset));
                            },
                            Message::BatteryState(sample) if phone_controls::eligible(hello, CAP_BATTERY) => { self.receive_phone_battery(&store, hello, sample); },
                            Message::RingState(state) if phone_controls::eligible(hello, CAP_RING) => { self.receive_phone_ring(&store, hello, state); },
                            Message::Error(error)=>{
                                if error.code == ErrorCode::Closing && hello.capabilities.iter().any(|c| c == "setup") && self.connection_current(&store, &hello.device_id) {
                                    self.ui.lock().expect("UI state").refreshing_scope(&hello.device_id);
                                }
                                if let Some(problem) = match error.code {
                                    ErrorCode::VersionMismatch => Some(mesh::LinkProblem::UpdateRequired),
                                    ErrorCode::Unauthorized => Some(mesh::LinkProblem::Unauthorized),
                                    _ => None,
                                } && let Some(generation) = store.pairing_generation(&hello.device_id) {
                                    let sessions = self.sessions.lock().expect("sessions");
                                    if sessions.sessions.get(&hello.device_id).is_some_and(|s| Some(s.id) == store.connection_id) {
                                        self.set_link_problem(&hello.device_id, generation, Some(problem));
                                    }
                                }
                                if outbound && error.code == ErrorCode::Unauthorized && !self.store.setup_protected(&hello.device_id) {
                                    self.forget_pairing(hello.device_id.clone(), CloseReason::Error(ErrorCode::Closing), store.connection_id, Some(hello.name.clone())).await?;
                                }
                                break;
                            },
                            Message::NotificationEvent(event)=>{
                                if !store.mesh || hello.role != Some(DeviceRole::Phone)
                                    || notice_stream.accept(&event).is_err()
                                    || phone_notices.send(event).is_err() {
                                    close_tx.send_replace(Some(CloseReason::Error(ErrorCode::UnexpectedMessage)));
                                    break;
                                }
                            },
                            Message::Status(status)=>{self.set_phone_paused(&hello.device_id,store.connection_id.expect("connection id"),status.paused);},
                            Message::Unpair(_) if hello.capabilities.iter().any(|c|c=="setup") && self.withdraw_setup(&store,hello).await?=>{break;},
                            Message::Unpair(_)=>{
                                // Admission precedes worker teardown too. The
                                // pairing epoch fences any in-flight commit;
                                // common teardown below joins both workers.
                                self.forget_pairing(hello.device_id.clone(), CloseReason::Error(ErrorCode::Closing), store.connection_id, Some(hello.name.clone())).await?;
                                break;
                            },
                            Message::PairRequest(request) if !recovered_pairing=>{
                                // TLS + HELLO binding were checked before READY.
                                // A consumed/expired token is irrelevant to this recovery.
                                self.consume_repair_invitation(&store,hello,&request.token).await?;
                                recovered_pairing=true;
                                if !enqueue(&write_tx,Message::PairResult(PairResult {accepted:true,code:None,message:None}),&mut close,Some(receipt_deadline.min(Instant::now()+Duration::from_secs(60)))).await? {break;}
                            },
                            Message::Members(snapshot) if hello.capabilities.iter().any(|c|c=="setup")=>{
                                if members_init_retry.is_some()&&self.store.setup_hello(Some(&hello.device_id),SetupScope::Disabled).scope==SetupScope::Enabled&&self.store.setup_state().setup_id.as_ref()==Some(&snapshot.setup_id)&&hello.setup.as_ref().is_some_and(|h|h.scope==SetupScope::Enabled&&h.setup_id==self.store.setup_state().setup_id) {
                                    if !enqueue(&write_tx,nack(snapshot.transfer_id,NackCode::StorageFailed),&mut close,Some(Instant::now()+Duration::from_secs(60))).await? {break;}
                                    continue;
                                }
                                match self.receive_members(&store,hello,members_fence.as_ref(),snapshot).await {
                                    Ok(response)=>{if !enqueue(&write_tx,response,&mut close,Some(Instant::now()+Duration::from_secs(60))).await? {break;}},
                                    Err(error)=>{close_tx.send_replace(Some(CloseReason::Error(if error.is::<crate::store::SettingsMalformed>() {ErrorCode::MalformedMessage}else{ErrorCode::UnexpectedMessage})));break;},
                                }
                            },
                            Message::PeerSettings(snapshot) if store.mesh=>{
                                let result={
                                    let _gate=self.settings_gate.lock().await;
                                    match store.settings_merge(self.identity.device_id.clone(),hello.device_id.clone(),hello.role==Some(DeviceRole::Phone),snapshot.clone()).await {
                                        Ok((deduplicated,fields))=>{
                                            let mut incoming=snapshot.settings.clone();incoming.sort_by(|a,b|a.key.cmp(&b.key));
                                            if fields!=incoming {settings_confirmed=None;}
                                            Ok(deduplicated)
                                        },Err(error)=>Err(error),
                                    }
                                };
                                let response=match result {
                                    Ok(deduplicated)=>{settings_received=true;Message::Ack(Ack {transfer_id:snapshot.transfer_id,deduplicated})},
                                    Err(error) if error.is::<crate::store::SettingsUnexpected>()=>{close_tx.send_replace(Some(CloseReason::Error(ErrorCode::UnexpectedMessage)));break;},
                                    Err(error) if error.is::<crate::store::SettingsMalformed>()=>{close_tx.send_replace(Some(CloseReason::Error(ErrorCode::MalformedMessage)));break;},
                                    Err(error)=>{settings_received=false;nack(snapshot.transfer_id,crate::store::save_failure_code(&error))},
                                };
                                if !enqueue(&write_tx,response,&mut close,Some(receipt_deadline.min(Instant::now()+Duration::from_secs(60)))).await? {break;}
                            },
                            Message::Ack(ack)=>{
                                if let Some((_,allowed))=cancelled_files.get_mut(&ack.transfer_id){ensure!(*allowed,"unsolicited cancelled ACK");*allowed=false;continue;}
                                if file_pending.as_ref().is_some_and(|d|d.begin.transfer_id==ack.transfer_id){ensure!(file_receipt_ready.as_ref().is_some_and(|p|p.load(Ordering::SeqCst)),"ACK before END");}
                                ensure!(!file_ready.as_ref().is_some_and(|(id,_,_)|id==&ack.transfer_id),"ACK before FILE_READY");
                                if members_pending.as_ref().is_some_and(|p|p.id==ack.transfer_id) {
                                    members_pending=None;
                                    let fence=members_fence.as_ref().context("missing membership fence")?;
                                    match store.members_ack(fence.clone(),ack.transfer_id).await {
                                        Ok(())=>members_confirmed=Some(members_snapshot.take().context("missing membership snapshot")?.members),
                                        Err(error)=>{members_snapshot=None;members_retry=Some(Instant::now()+Duration::from_secs(60));self.error(Some(hello.device_id.clone()),format!("membership ACK persistence deferred: {error}"));},
                                    }
                                    continue;
                                }
                                if settings_pending.as_ref().is_some_and(|p|p.id==ack.transfer_id) {
                                    let deadline=settings_pending.take().and_then(|p|p.deadline).unwrap_or_else(||Instant::now()+Duration::from_secs(120));
                                    match tokio::time::timeout_at(deadline,store.settings_ack(hello.device_id.clone(),ack.transfer_id)).await.map_err(anyhow::Error::from).and_then(|result|result) {
                                        Ok(())=>settings_confirmed=Some(settings_snapshot.take().context("missing settings snapshot")?.settings),
                                        Err(error)=>{
                                            settings_snapshot=None;settings_confirmed=None;
                                            settings_retry=Some(Instant::now()+Duration::from_secs(60));
                                            self.error(Some(hello.device_id.clone()),format!("settings ACK persistence deferred: {error}"));
                                        },
                                    }
                                    continue;
                                }
                                if text_pending.as_ref().is_some_and(|p|p.id==ack.transfer_id) {
                                    let id=text_pending.take().unwrap().id;
                                    self.automatic.lock().expect("automatic clipboard").receipt(&hello.device_id,store.connection_id.expect("connection id"),&id);
                                    self.connection_event(&store,&hello.device_id,Event::ClipboardSent {device:hello.device_id.clone(),transfer_id:id});continue;
                                }
                                ensure!(pending.as_ref().is_some_and(|p|p.id==ack.transfer_id),"wrong or unsolicited ACK ID");
                                let id=pending.take().unwrap().id;
                                if let Some(delivery)=file_pending.take() {
                                    if let Err(error)=self.outbox.receipt(delivery.clone(),None).await {self.error(Some(hello.device_id.clone()),format!("file ACK persistence deferred: {error}"));break;}
                                    self.delivery_finished(&delivery);
                                    let _=self.events.send(Event::ConfigChanged);continue;
                                }
                                self.automatic.lock().expect("automatic clipboard").receipt(&hello.device_id,store.connection_id.expect("connection id"),&id);
                                self.connection_event(&store,&hello.device_id,Event::ClipboardSent {device:hello.device_id.clone(),transfer_id:id});
                            },
                            Message::Nack(nack)=>{
                                let early_file_nack=file_ready.as_ref().is_some_and(|(id,_,_)|id==&nack.transfer_id);
                                if let Some((ready,receipt))=cancelled_files.get_mut(&nack.transfer_id){ensure!(*ready||*receipt,"unsolicited cancelled NACK");*ready=false;*receipt=false;continue;}
                                if file_pending.as_ref().is_some_and(|d|d.begin.transfer_id==nack.transfer_id) && file_ready.is_none(){ensure!(file_receipt_ready.as_ref().is_some_and(|p|p.load(Ordering::SeqCst)),"NACK before END");}
                                if file_ready.as_ref().is_some_and(|(id,_,_)|id==&nack.transfer_id) && let Some((_,_,reply))=file_ready.take(){let _=reply.send(None);}
                                if members_pending.as_ref().is_some_and(|p|p.id==nack.transfer_id) {
                                    members_pending=None;members_snapshot=None;members_confirmed=None;
                                    members_blocked = !nack.retryable;
                                    members_retry=nack.retryable.then(||Instant::now()+Duration::from_secs(60));
                                    self.error(Some(hello.device_id.clone()),format!("membership synchronization rejected: {:?}",nack.code));continue;
                                }
                                if settings_pending.as_ref().is_some_and(|p|p.id==nack.transfer_id) {
                                    settings_pending=None;
                                    settings_snapshot=None;
                                    settings_confirmed=None;
                                    settings_retry=Some(Instant::now()+Duration::from_secs(60));
                                    self.error(Some(hello.device_id.clone()),format!("settings synchronization rejected: {:?}",nack.code));
                                    continue;
                                }
                                if text_pending.as_ref().is_some_and(|p|p.id==nack.transfer_id) {
                                    let id=text_pending.take().unwrap().id;
                                    self.automatic.lock().expect("automatic clipboard").receipt(&hello.device_id,store.connection_id.expect("connection id"),&id);
                                    self.clipboard_rejected(hello,&id,"text",&nack);continue;
                                }
                                ensure!(pending.as_ref().is_some_and(|p|p.id==nack.transfer_id),"wrong or unsolicited NACK ID");
                                let id=pending.take().unwrap().id;
                                if let Some(delivery)=file_pending.take() {
                                    if let Err(error)=self.outbox.receipt_phase(delivery.clone(),Some(nack.code),early_file_nack).await {self.error(Some(hello.device_id.clone()),format!("file NACK persistence deferred: {error}"));break;}
                                    self.delivery_finished(&delivery);
                                    let _=self.events.send(Event::ConfigChanged);continue;
                                }
                                self.automatic.lock().expect("automatic clipboard").receipt(&hello.device_id,store.connection_id.expect("connection id"),&id);

                                self.clipboard_rejected(hello,&id,"image",&nack);
                            },
                            Message::Cancel(cancel)=>{
                                ensure!(store.file_resume,"unnegotiated CANCEL");
                                store.signal_cancel(&hello.device_id,&cancel.transfer_id);
                                if !enqueue(&file_tx,ApplicationMessage {message:Message::Cancel(cancel),refusal:None,resume_cancel:None},&mut close,None).await?{break;}
                            },
                            m @ (Message::FileBegin(_)|Message::FileChunk(_)|Message::FileEnd(_))=>{
                                let resume_cancel=match &m {Message::FileBegin(b) if b.resume==Some(true)=>Some(store.register_resume_begin(&hello.device_id,&b.transfer_id)),_=>None};
                                if !enqueue(&file_tx,ApplicationMessage { message: m, refusal, resume_cancel },&mut close,pending.as_ref().and_then(|p|p.deadline)).await? {break;}
                            },
                            m @ Message::Clipboard(_)=>{if !enqueue(&text_tx,m,&mut close,pending.as_ref().and_then(|p|p.deadline)).await? {break;}},
                            _=>bail!("unexpected READY frame"),
                        }
                    }
                }
            }Ok(())
        }.await;
        self.end_phone_control(&hello.device_id, store.connection_id.expect("connection id"));
        if let Err(error) = &result
            && close.borrow().is_none()
        {
            let _ = close_tx.send(Some(CloseReason::Error(processing_error_code(error))));
        }
        phone_notices.stop().await;
        workers.reader.abort();
        workers.heartbeat.abort();
        let _=(&mut workers.heartbeat).await;
        drop(file_tx);
        let file_finished=tokio::time::timeout(Duration::from_secs(2),&mut workers.file).await.is_ok();
        if !file_finished {workers.file.abort();}
        workers.text.abort();
        let _ = (&mut workers.reader).await;
        if !file_finished {let _ = (&mut workers.file).await;}
        let _ = (&mut workers.text).await;
        drop(text_tx);
        drop(write_tx);
        drop(automatic_tx);
        drop(automatic_text_tx);
        drop(outbox_tx);
        if !writer_finished
            && tokio::time::timeout(Duration::from_secs(2), &mut workers.writer)
                .await
                .is_err()
        {
            workers.writer.abort();
            let _ = (&mut workers.writer).await;
        }
        // The writer may have been cancelled mid-frame. Its UI offset is
        // useful on disconnect, but never authorizes a future resume offset.
        if store.file_resume && let Some(delivery)=file_pending.as_ref() {
            self.optional(self.outbox.checkpoint_progress(delivery.clone()).await,"outbox progress checkpoint");
        }
        file_preparations.abort_all();
        while file_preparations.join_next().await.is_some() {}
        drop(recovery);
        if let Err(error)=self.outbox.retry_recovery().await {self.error(Some(hello.device_id.clone()),format!("outbox recovery deferred: {error}"));}
        result

        }).await
    }
    async fn validate_clipboard_message(
        &self,
        store: &Store,
        device: &Id,
        mut message: Message,
    ) -> Result<Message> {
        let fields = match &mut message {
            Message::Clipboard(c) => Some((&mut c.hlc, &mut c.source_device, true)),
            Message::FileBegin(f) => Some((
                &mut f.hlc,
                &mut f.source_device,
                f.kind == FileKind::ClipboardImage,
            )),
            _ => None,
        };
        if let Some((hlc, source, required)) = fields {
            if store.mesh {
                ensure!(
                    !required || hlc.is_some(),
                    "unstamped mesh clipboard operation"
                );
                if let Some(hlc) = *hlc {
                    ensure!(
                        source.as_ref() == Some(device),
                        "clipboard source differs from authenticated peer"
                    );
                    crate::store::validate_stamp(hlc, crate::now_ms())?;
                }
            } else {
                // Negotiation selects legacy semantics even if optional fields
                // happen to be supplied by an older or misconfigured endpoint.
                *hlc = None;
                *source = None;
            }
        }
        Ok(message)
    }
    async fn file_loop(
        self: Arc<Self>,
        store: Store,
        action_scope: Option<ActionScope>,
        device: Id,
        mut messages: mpsc::Receiver<impl Into<ApplicationMessage> + Send>,
        writer: mpsc::Sender<Message>,
        close: watch::Sender<Option<CloseReason>>,
    ) -> Result<()> {
        Box::pin(async move {
            let mut incoming = None;
            let mut progress = Instant::now();
            let result: Result<()> = async {
                loop {
                    let deadline = progress + Duration::from_secs(if incoming.as_ref().is_some_and(|s:&Incoming|s.first_chunk&&s.begin.resume==Some(true)){120}else{60});
                    let message = if incoming.is_some() {
                        tokio::time::timeout_at(deadline, messages.recv()).await?
                    } else {
                        messages.recv().await
                    };
                    let Some(message) = message else {
                        break;
                    };
                    let message: ApplicationMessage = message.into();
                    let deadline = if matches!(message.message, Message::FileBegin(ref b) if b.resume==Some(true)) {
                        Instant::now()+Duration::from_secs(90)
                    } else if matches!(message.message, Message::FileEnd(_)) {
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
                    if state.begin.resume == Some(true)
                        && !state.begin.sensitive
                        && state.save_error.is_none()
                        && state.receipt.is_none()
                    {
                        self.optional(
                            store
                                .checkpoint(
                                    device.clone(),
                                    state.begin,
                                    stage,
                                    state.received,
                                    state.hash,
                                    true,
                                )
                                .await,
                            "resume checkpoint",
                        );
                    } else {
                        self.optional(
                            store
                                .purge_partial(device.clone(), state.begin.transfer_id.clone())
                                .await,
                            "resume cleanup",
                        );
                        self.optional(store.discard_stage(stage).await, "staging cleanup");
                    }
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
        message: impl Into<ApplicationMessage> + Send,
        incoming: &mut Option<Incoming>,
        tx: &mpsc::Sender<Message>,
    ) -> Result<()> {
        let ApplicationMessage {
            message,
            refusal,
            resume_cancel,
        } = message.into();
        let resume_cancel = resume_cancel.or_else(|| match &message {
            Message::FileBegin(b) if b.resume == Some(true) => {
                Some(store.register_resume_begin(device, &b.transfer_id))
            }
            _ => None,
        });
        let mut cancelled = resume_cancel.as_ref().map(|c| c.subscribe());
        let receive = Box::pin(async move {
            match message {
                Message::FileBegin(begin) => {
                    ensure!(
                        (begin.resume != Some(true) || (store.file_resume && !begin.sensitive))
                            && (begin.folder.is_none() || store.file_resume),
                        "unnegotiated file extension"
                    );
                    ensure!(incoming.is_none(), "second FILE_BEGIN");
                    let config = self.config();
                    let mut state = Incoming {
                        begin: begin.clone(),
                        received: 0,
                        first_chunk: true,
                        hash: Sha256::new(),
                        file: None,
                        stage: None,
                        destination: None,
                        receipt: None,
                        save_error: None,
                        deduplicated: false,
                        admission: FileAdmission::New,
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
                        Ok(Some((meta, complete))) => {
                            if matches!(&meta, Metadata::File(retained) if retained.content_eq(&begin))
                                && complete
                            {
                                state.admission = FileAdmission::RetainedReplay;
                                state.deduplicated = true;
                            } else {
                                state.admission = FileAdmission::RetainedConflict;
                                state.receipt = Some(NackCode::IdConflict);
                            }
                        }
                        Err(_) => {
                            self.image_refused(
                                device,
                                &begin,
                                "receipt_lookup",
                                NackCode::StorageFailed,
                            );
                            state.receipt = Some(NackCode::StorageFailed);
                        }
                        Ok(None) => {
                            if let Some(code) = refusal {
                                state.receipt = Some(code);
                            } else if store.mesh
                                && !store
                                    .pair_settings
                                    .lock()
                                    .expect("pair settings")
                                    .contains_key(device)
                            {
                                self.image_refused(
                                    device,
                                    &begin,
                                    "policy_cache_missing",
                                    NackCode::StorageFailed,
                                );
                                state.receipt = Some(NackCode::StorageFailed);
                            } else if !self.incoming_kind_allowed(store, device, begin.kind) {
                                state.receipt = Some(NackCode::UnsupportedKind);
                            } else if begin.size <= state.config.max_file_bytes {
                                if begin.resume == Some(true) && !begin.sensitive {
                                    match store
                                        .resume_stage_cancellable(
                                            device.clone(),
                                            begin.clone(),
                                            resume_cancel
                                                .as_ref()
                                                .expect("resume cancellation")
                                                .clone(),
                                        )
                                        .await
                                    {
                                        Ok(Some((stage, offset, hash))) => {
                                            let root = stage.0.folder.path()?;
                                            if root != self.paths.cache {
                                                if crate::config::save_folder_overlaps_cache(
                                                    &root,
                                                    &self.paths.cache,
                                                ) {
                                                    store
                                                        .purge_partial(
                                                            device.clone(),
                                                            begin.transfer_id.clone(),
                                                        )
                                                        .await?;
                                                    self.prepare_incoming(
                                                        store, device, &mut state,
                                                    )
                                                    .await;
                                                } else {
                                                    state.config.save_to_folder = true;
                                                    state.config.save_path = root;
                                                    state.storage_kind =
                                                        crate::ui::ReceiveDestination::SaveFolder;
                                                    state.file = Some(tokio::fs::File::from_std(
                                                        stage.0.file.try_clone()?,
                                                    ));
                                                    state.destination =
                                                        Some(stage.destination_path()?);
                                                    state.stage = Some(stage);
                                                    state.received = offset;
                                                    state.hash = hash;
                                                }
                                            } else {
                                                state.config.save_to_folder = false;
                                                state.storage_kind =
                                                    crate::ui::ReceiveDestination::Cache;
                                                state.file = Some(tokio::fs::File::from_std(
                                                    stage.0.file.try_clone()?,
                                                ));
                                                state.destination = Some(stage.destination_path()?);
                                                state.stage = Some(stage);
                                                state.received = offset;
                                                state.hash = hash;
                                            }
                                        }
                                        Ok(None) => {
                                            self.prepare_incoming(store, device, &mut state).await
                                        }
                                        Err(error) => {
                                            state.receipt = Some(
                                                if error
                                                    .downcast_ref::<crate::store::ReceiptConflict>()
                                                    .is_some()
                                                {
                                                    NackCode::IdConflict
                                                } else {
                                                    crate::store::save_failure_code(&error)
                                                },
                                            );
                                            self.image_refused(
                                                device,
                                                &begin,
                                                "resume_stage",
                                                state.receipt.unwrap(),
                                            );
                                        }
                                    }
                                } else {
                                    match store
                                        .discard_legacy_partial(device.clone(), begin.clone())
                                        .await
                                    {
                                        Ok(()) => {
                                            self.prepare_incoming(store, device, &mut state).await
                                        }
                                        Err(error) => {
                                            state.receipt = Some(
                                                if error
                                                    .downcast_ref::<crate::store::ReceiptConflict>()
                                                    .is_some()
                                                {
                                                    NackCode::IdConflict
                                                } else {
                                                    crate::store::save_failure_code(&error)
                                                },
                                            );
                                            self.image_refused(
                                                device,
                                                &begin,
                                                "discard_partial",
                                                state.receipt.unwrap(),
                                            );
                                        }
                                    }
                                }
                            } else {
                                if begin.kind == FileKind::ClipboardImage {
                                    self.error(Some(device.clone()), format!(
                                        "clipboard receive refused: transfer={} kind=image phase=size_limit code=StorageFailed bytes={} limit={}",
                                        begin.transfer_id, begin.size, state.config.max_file_bytes,
                                    ));
                                }
                                state.receipt = Some(NackCode::StorageFailed);
                            }
                        }
                    }
                    if begin.resume == Some(true) {
                        // A failed requested save must never become a successful
                        // cache-only completion when this BEGIN is retried.
                        if let Some(code) = state.save_error {
                            state.file.take();
                            if let Some(stage) = state.stage.take() {
                                store.discard_stage(stage).await?;
                            }
                            state.receipt = Some(code);
                        }
                        if state.receipt.is_none()
                            && !state.deduplicated
                            && !begin.sensitive
                            && let Some(stage) = &state.stage
                            && let Err(error) = store
                                .checkpoint(
                                    device.clone(),
                                    begin.clone(),
                                    stage.clone(),
                                    state.received,
                                    state.hash.clone(),
                                    true,
                                )
                                .await
                        {
                            state.receipt = Some(crate::store::save_failure_code(&error));
                            self.image_refused(
                                device,
                                &begin,
                                "initial_checkpoint",
                                state.receipt.unwrap(),
                            );
                        }
                        if let Some(code) = state.receipt {
                            tx.send(nack(begin.transfer_id.clone(), code)).await?;
                            return Ok(());
                        }
                        if state.deduplicated {
                            state.received = begin.size;
                        }
                        tx.send(Message::FileReady(pneu_proto::FileReady {
                            transfer_id: begin.transfer_id.clone(),
                            offset: state.received,
                        }))
                        .await?;
                    }
                    self.connection_event(
                        store,
                        device,
                        Event::TransferStarted {
                            device: device.clone(),
                            metadata: begin,
                        },
                    );
                    if state.begin.resume == Some(true) {
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
                    *incoming = Some(state);
                }
                Message::FileChunk(chunk) => {
                    let state = incoming.as_mut().context("chunk without BEGIN")?;
                    state.first_chunk = false;
                    ensure!(
                        chunk.transfer_id == state.begin.transfer_id
                            && chunk.offset == state.received
                            && state.received + chunk.data.len() as u64 <= state.begin.size,
                        "noncontiguous chunk or size overflow"
                    );
                    if state.receipt.is_none() && !state.deduplicated {
                        #[cfg(test)]
                        {
                            let delay = *self.file_write_delay.lock().expect("file write delay");
                            if !delay.is_zero() {
                                tokio::time::sleep(delay).await;
                            }
                        }
                        let written: Result<()> = async {
                            if state.begin.folder.is_some() {
                                // Manifest bytes go to the stage file and content
                                // directly into the private tree: no stream spool.
                                let stage = state.stage.clone().context("missing folder stage")?;
                                let begin = state.begin.clone();
                                let from = state.received;
                                let data = chunk.data.clone();
                                store
                                    .blocking(move |_| stage.folder_write(&begin, from, &data))
                                    .await?;
                            } else if let Some(file) = state.file.as_mut() {
                                file.write_all(&chunk.data).await?;
                                // Tokio File may report a kernel write failure only
                                // on flush. Keep this chunk until that write resolves.
                                file.flush().await?;
                            }
                            Ok(())
                        }
                        .await;
                        if let Err(error) = written {
                            let code = crate::store::save_failure_code(&error);
                            self.image_refused(device, &state.begin, "chunk_write_flush", code);
                            if state.config.save_to_folder
                                && state.begin.folder.is_none()
                                && state.save_error.is_none()
                                && state.begin.kind != FileKind::ClipboardImage
                                && matches!(code, NackCode::NotWritable | NackCode::DiskFull)
                                && self
                                    .fallback_cache(store, device, state, code, state.received)
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
                        if state.receipt.is_none() {
                            state.hash.update(&chunk.data);
                        }
                    }
                    state.received += chunk.data.len() as u64;
                    if state.begin.resume == Some(true)
                        && state.save_error.is_none()
                        && state.receipt.is_none()
                        && !state.begin.sensitive
                        && let Some(stage) = &state.stage
                        && let Err(error) = store
                            .checkpoint(
                                device.clone(),
                                state.begin.clone(),
                                stage.clone(),
                                state.received,
                                state.hash.clone(),
                                false,
                            )
                            .await
                    {
                        state.receipt = Some(crate::store::save_failure_code(&error));
                        self.image_refused(
                            device,
                            &state.begin,
                            "chunk_checkpoint",
                            state.receipt.unwrap(),
                        );
                    }
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
                Message::Cancel(cancel) => {
                    ensure!(store.file_resume, "unnegotiated CANCEL");
                    if incoming
                        .as_ref()
                        .is_some_and(|s| s.begin.transfer_id == cancel.transfer_id)
                    {
                        let mut state = incoming.take().expect("matching transfer");
                        state.file.take();
                        if let Some(stage) = state.stage.take() {
                            store.discard_stage(stage).await?;
                        }
                        self.finish_transfer_activity(store, device, &cancel.transfer_id);
                    }
                    store
                        .purge_partial(device.clone(), cancel.transfer_id)
                        .await?;
                }
                Message::FileEnd(end) => {
                    ensure!(
                        incoming
                            .as_ref()
                            .is_some_and(|s| s.begin.transfer_id == end.transfer_id),
                        "END without matching BEGIN"
                    );
                    let state = incoming.as_mut().expect("checked incoming");
                    if state.admission == FileAdmission::New
                        && !self.incoming_kind_allowed(store, device, state.begin.kind)
                    {
                        state.receipt = Some(NackCode::UnsupportedKind);
                    }
                    if state.admission == FileAdmission::New && state.received != state.begin.size {
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
                            received_file = Some(ReceivedFile::from_transfer(
                                path.clone(),
                                stage.published_file()?,
                                stage.folder_details(),
                                state.begin.size,
                            )?);
                            if let Some(details) = stage.folder_details()
                                && let Some(row) =
                                    self.ui.lock().expect("UI state").incoming.iter_mut().find(
                                        |r| {
                                            r.device == *device
                                                && r.transfer_id == state.begin.transfer_id
                                        },
                                    )
                            {
                                row.folder = Some(details);
                            }
                            state.destination = Some(path);
                        }
                        if stored.is_ok() {
                            // Placement and receipt are already durable. Residue
                            // cleanup cannot suppress their ACK; the guard tracks it.
                            state.stage.take();
                        }
                        if let Err(error) = stored {
                            let code = crate::store::save_failure_code(&error);
                            self.image_refused(device, &state.begin, "commit_file", code);
                            if state.config.save_to_folder
                                && state.begin.folder.is_none()
                                && state.save_error.is_none()
                                && state.begin.kind != FileKind::ClipboardImage
                                && matches!(code, NackCode::NotWritable | NackCode::DiskFull)
                                && self
                                    .fallback_cache(store, device, state, code, state.received)
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
                    if state.begin.resume == Some(true) {
                        if state.receipt.is_some() || state.save_error.is_some() {
                            store
                                .purge_partial(device.clone(), state.begin.transfer_id.clone())
                                .await?;
                        } else {
                            store
                                .forget_partial(device.clone(), state.begin.transfer_id.clone())
                                .await?;
                        }
                    }
                    if let Some(stage) = state.stage.take() {
                        self.optional(store.discard_stage(stage).await, "staging cleanup");
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
                                        metadata: Box::new(state.begin.clone()),
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
                    if self.store.replacement_pending(device) {
                        tx.send(nack(text.transfer_id, NackCode::UnsupportedKind))
                            .await?;
                        return Ok(());
                    }
                    let receipt = if store.mesh {
                        match store
                            .receipt(device.clone(), text.transfer_id.clone())
                            .await
                        {
                            Ok(receipt) => receipt,
                            Err(_) => {
                                self.clipboard_refused(
                                    device,
                                    &text.transfer_id,
                                    "text",
                                    "receipt_lookup",
                                    NackCode::StorageFailed,
                                );
                                tx.send(nack(text.transfer_id, NackCode::StorageFailed))
                                    .await?;
                                return Ok(());
                            }
                        }
                    } else {
                        None
                    };
                    if let Some((metadata, complete)) = receipt {
                        tx.send(
                            if metadata != crate::store::Metadata::Text(store.text_receipt(&text)) {
                                nack(text.transfer_id, NackCode::IdConflict)
                            } else if complete {
                                Message::Ack(Ack {
                                    transfer_id: text.transfer_id,
                                    deduplicated: true,
                                })
                            } else {
                                self.clipboard_refused(
                                    device,
                                    &text.transfer_id,
                                    "text",
                                    "incomplete_reservation",
                                    NackCode::StorageFailed,
                                );
                                nack(text.transfer_id, NackCode::StorageFailed)
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                    if store.mesh
                        && !store
                            .pair_settings
                            .lock()
                            .expect("pair settings")
                            .contains_key(device)
                    {
                        self.clipboard_refused(
                            device,
                            &text.transfer_id,
                            "text",
                            "policy_cache_missing",
                            NackCode::StorageFailed,
                        );
                        tx.send(nack(text.transfer_id, NackCode::StorageFailed))
                            .await?;
                        return Ok(());
                    }
                    if store.mesh
                        && !self.pair_allows(device, device, &self.identity.device_id, "clipboard")
                    {
                        tx.send(nack(text.transfer_id, NackCode::UnsupportedKind))
                            .await?;
                        return Ok(());
                    }
                    let Some(scope) = action_scope else {
                        return Ok(());
                    };
                    let origin = ActionOrigin::captured(store, device);
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
                        Ok(Some(claim)) => claim,
                        Err(error) => {
                            self.clipboard_refused(
                                device,
                                &text.transfer_id,
                                "text",
                                "admission",
                                crate::store::save_failure_code(&error),
                            );
                            tx.send(nack(
                                text.transfer_id,
                                crate::store::save_failure_code(&error),
                            ))
                            .await?;
                            return Ok(());
                        }
                    };
                    let token = self.clipboard.token();
                    let content = crate::ui::clipboard::Content::Text(text.text.clone());
                    let operation = ClipboardOperation {
                        sensitive: text.sensitive,
                        history: self.store.history_capture(),
                        history_id: None,
                        token,
                        id: text.transfer_id.clone(),
                        source: device.clone(),
                        key: text.hlc.map(|hlc| clipboard_actor::Key {
                            hlc,
                            source: device.clone(),
                        }),
                        created_at: text.created_at,
                        payload: None,
                        hashes: clipboard_actor::hashes(&content),
                        content: Some(content),
                        offer: None,
                        received: true,
                        legacy: text.hlc.is_none(),
                        restore: false,
                        repair: false,
                        observation: None,
                    };
                    let (reply, _) = tokio::sync::oneshot::channel();
                    let outcome = self
                        .clipboard
                        .submit(
                            operation,
                            clipboard_driver::Request {
                                input: crate::actions::ClipboardInput::Text(text.text.clone()),
                                claim: Some(claim),
                                store: store.clone(),
                                authority: Some((scope.clone(), origin.clone())),
                                kind: None,
                                policy: self.clipboard_receive_policy(device, None),
                                reply,
                            },
                        )
                        .await;
                    if !scope.allows(&origin) {
                        return Ok(());
                    }
                    if store.mesh
                        && !self.pair_allows(device, device, &self.identity.device_id, "clipboard")
                    {
                        tx.send(nack(text.transfer_id, NackCode::UnsupportedKind))
                            .await?;
                        return Ok(());
                    }
                    let outcome = match outcome {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            let storage = error.is::<crate::store::ClipboardStorageFailure>();
                            if storage {
                                self.clipboard_refused(
                                    device,
                                    &text.transfer_id,
                                    "text",
                                    "actor_submit",
                                    NackCode::StorageFailed,
                                );
                            }
                            if !storage {
                                self.action_queue
                                    .receipt(scope, origin, ReceiptNotice::TextFailed);
                            }
                            tx.send(nack(
                                text.transfer_id,
                                if storage {
                                    NackCode::StorageFailed
                                } else {
                                    NackCode::ClipboardUnavailable
                                },
                            ))
                            .await?;
                            return Ok(());
                        }
                    };
                    if outcome.applied
                        && self.config().notification_preview
                        && !self.peer_is_computer(device)
                    {
                        self.action_queue.receipt(
                            scope,
                            origin,
                            ReceiptNotice::Text(text.text.clone()),
                        );
                    }
                    tx.send(Message::Ack(Ack {
                        transfer_id: text.transfer_id.clone(),
                        deduplicated: outcome.deduplicated,
                    }))
                    .await?;
                    if outcome.applied {
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
                }
                _ => bail!("unexpected application message"),
            }
            Ok(())
        });
        tokio::select! {
            biased;
            _=async {if let Some(cancel)=&mut cancelled {let _=cancel.wait_for(|b|*b).await;}else{std::future::pending::<()>().await;} }=>Ok(()),
            result=receive=>result,
        }
    }
    /// Recover the already received prefix through its retained descriptor,
    /// then keep streaming in cache. Never resolve the selected folder again.
    async fn fallback_cache(
        &self,
        store: &Store,
        device: &Id,
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
                crate::platform::fs::copy_prefix(&input.0.file, &destination.0.file, prefix)
            })
            .await?;
        state.destination = Some(stage.destination_path()?);
        state.file = Some(tokio::fs::File::from_std(stage.0.file.try_clone()?));
        state.stage = Some(stage);
        if state.begin.resume == Some(true) {
            store
                .purge_partial(device.clone(), state.begin.transfer_id.clone())
                .await?;
        }
        let _ = store.discard_stage(source).await;
        Ok(())
    }
    async fn prepare_incoming(&self, store: &Store, device: &Id, state: &mut Incoming) {
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
                } else if state.begin.folder.is_some() {
                    crate::folders::root_name(&state.begin.file_name)
                } else {
                    local_name(&state.begin.file_name, 255)
                };
            // Folders write content straight into their private tree, so the
            // stream size is also the temporary space a folder needs.
            let size = state.begin.size;
            let stage = match async {
                if state.config.save_to_folder && state.begin.kind != FileKind::ClipboardImage {
                    ensure!(
                        !crate::config::save_folder_overlaps_cache(&folder, &self.paths.cache),
                        crate::platform::fs::permission_denied()
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
            Err(error) => {
                let code = crate::store::save_failure_code(&error);
                self.image_refused(device, &state.begin, "prepare_stage", code);
                state.receipt = Some(code);
            }
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
        let file = file.with_source(self.arrival_source(&origin.device));
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
                .notification(&job.origin, || {
                    self.actions
                        .begin_notify_image_failure(&job.file, &job.metadata, job.saved())
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
                .notification(&job.origin, || {
                    self.actions.begin_notify_file_at(
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
                .notification(&job.origin, || {
                    self.actions
                        .begin_notify_image_failure(&job.file, &job.metadata, job.saved())
                })
                .await;
            return;
        }
        let copied = if image && (explicit || job.config.copy_image_to_clipboard) {
            if !job.scope.allows(&job.origin) || (job.origin.mesh && job.metadata.hlc.is_none()) {
                return;
            }
            let mut store = self.store.with_current_pairing();
            store.mesh = job.origin.mesh;
            let token = self.clipboard.token();
            let operation = ClipboardOperation {
                sensitive: job.metadata.kind == FileKind::ClipboardImage && job.metadata.sensitive,
                history: self.store.history_capture(),
                history_id: None,
                token,
                id: job.metadata.transfer_id.clone(),
                source: job.origin.device.clone(),
                key: job.metadata.hlc.map(|hlc| clipboard_actor::Key {
                    hlc,
                    source: job.origin.device.clone(),
                }),
                created_at: job.metadata.created_at,
                payload: None,
                hashes: vec![
                    hex::decode(job.metadata.sha256.as_str())
                        .expect("SHA-256")
                        .try_into()
                        .expect("SHA-256 length"),
                ],
                content: None,
                offer: None,
                received: true,
                legacy: !job.origin.mesh,
                restore: false,
                repair: false,
                observation: None,
            };
            let (reply, _) = tokio::sync::oneshot::channel();
            match self
                .clipboard
                .submit(
                    operation,
                    clipboard_driver::Request {
                        input: crate::actions::ClipboardInput::Image(
                            job.file.clone(),
                            job.metadata.mime.clone(),
                        ),
                        claim: None,
                        store,
                        authority: Some((job.scope.clone(), job.origin.clone())),
                        kind: Some(job.metadata.kind),
                        policy: self
                            .clipboard_receive_policy(&job.origin.device, Some(job.metadata.kind)),
                        reply,
                    },
                )
                .await
            {
                Ok(outcome) => {
                    if !outcome.applied {
                        return;
                    }
                    self.actions.record_clipboard_copy(&job.file).await;
                    true
                }
                Err(error) => {
                    self.error(
                        Some(job.origin.device.clone()),
                        format!("image clipboard unavailable: {error}"),
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
                .notification(&job.origin, || {
                    self.actions
                        .begin_notify_image_failure(&job.file, &job.metadata, job.saved())
                })
                .await;
        }
        if explicit
            && copied
            && job.config.notification_preview
            && let Some(Err(e)) = job
                .scope
                .notification(&job.origin, || {
                    self.actions
                        .begin_notify_copied_received(&job.file, &job.metadata.mime)
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
    let source = core.arrival_source(&job.origin.device);
    let effect = job.scope.notification(&job.origin, || async {
        match &job.notice {
            ReceiptNotice::Text(text) => core.actions.begin_notify_text_from(text, &source).await,
            ReceiptNotice::TextFailed => core.actions.begin_notify_text_failure_from(&source).await,
            ReceiptNotice::SaveFailed {
                metadata,
                code,
                destination,
                folder,
            } => {
                core.actions
                    .begin_notify_save_failure_from(metadata, *code, *destination, folder, &source)
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
                NotificationJob::Delivery(transfer, name) => {
                    if core.config().notification_preview {
                        tokio::select! { biased;
                            _ = stop.changed() => {},
                            result = tokio::time::timeout(Duration::from_secs(5), core.actions.notify_delivery(&transfer, &name)) => {
                                if !matches!(result, Ok(Ok(()))) { core.error(Some(transfer.device), "send notification unavailable"); }
                            }
                        }
                    }
                },
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
    heartbeat: tokio::task::JoinHandle<()>,
    reader: tokio::task::JoinHandle<Result<()>>,
    writer: tokio::task::JoinHandle<Result<()>>,
    file: tokio::task::JoinHandle<Result<()>>,
    text: tokio::task::JoinHandle<Result<()>>,
}
impl Drop for ConnectionTasks {
    fn drop(&mut self) {
        self.heartbeat.abort();
        self.reader.abort();
        self.writer.abort();
        self.file.abort();
        self.text.abort();
    }
}
// Transmission is governed by frame write/flush deadlines. Receipt time starts
// only after the last frame is flushed.
struct PendingOutgoing {
    id: Id,
    deadline: Option<Instant>,
}
impl PendingOutgoing {
    fn queued(id: Id) -> Self {
        Self { id, deadline: None }
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
    CancelSent(Id),
    Sent { id: Id, at: Instant },
}
fn frame_receipt_id(message: &Message) -> Option<Id> {
    match message {
        Message::Clipboard(text) => Some(text.transfer_id.clone()),
        Message::PeerSettings(snapshot) => Some(snapshot.transfer_id.clone()),
        Message::Members(snapshot) => Some(snapshot.transfer_id.clone()),
        Message::FileEnd(end) => Some(end.transfer_id.clone()),
        _ => None,
    }
}
async fn report_frame(reports: &mpsc::Sender<WriteReport>, message: &Message) {
    if let Message::Cancel(cancel) = message {
        let _ = reports
            .send(WriteReport::CancelSent(cancel.transfer_id.clone()))
            .await;
    }
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
    normal: phone_controls::FrameQueue,
    priority: mpsc::Receiver<Message>,
    automatic: mpsc::Receiver<AutomaticWrite>,
    text: mpsc::Receiver<AutomaticWrite>,
    files: mpsc::Receiver<FileWrite>,
    reports: mpsc::Sender<WriteReport>,
    close: watch::Receiver<Option<CloseReason>>,
    // Run after the close arm polls Pending, before a queue can return None.
    #[cfg(test)]
    before_queue_poll: Option<Box<dyn FnOnce() + Send>>,
}
async fn writer_loop<W: tokio::io::AsyncWrite + Unpin>(
    core: Arc<Core>,
    writer: &mut W,
    queues: WriterQueues,
) -> Result<()> {
    Box::pin(async move {
    let WriterQueues { normal: mut write_rx, priority: mut priority_rx, automatic: mut automatic_rx, text: mut text_rx, files: mut files_rx, reports: reports_tx, close: mut writer_close,
        #[cfg(test)]
        mut before_queue_poll,
    } = queues;
            loop {
                let reason = *writer_close.borrow();
                if let Some(reason) = reason {
                    return close_transport(writer, reason).await;
                }
                let work = tokio::select! {biased;
                    _=writer_close.changed()=>{let reason=writer_close.borrow().unwrap_or(CloseReason::Error(ErrorCode::Closing));return close_transport(writer,reason).await;},
                    message=async {
                        #[cfg(test)]
                        if let Some(hook) = before_queue_poll.take() { hook(); }
                        priority_rx.recv().await
                    }=>message.map(WriterWork::Frame),
                    message=write_rx.recv()=>message.map(WriterWork::Frame),
                    text=text_rx.recv()=>text.map(WriterWork::Automatic),
                    file=files_rx.recv()=>file.map(WriterWork::File),
                    automatic=automatic_rx.recv()=>automatic.map(WriterWork::Automatic),
                };
                let Some(work) = work else {
                    // The close arm may have polled Pending before teardown
                    // published its reason and dropped the queue senders.
                    let reason = *writer_close.borrow();
                    if let Some(reason) = reason {
                        return close_transport(writer, reason).await;
                    }
                    break;
                };
                let message = match work {
                    WriterWork::Frame(message) => message,
                    WriterWork::File(work)=>{
                        let id=work.delivery.begin.transfer_id.clone();
                        let result=tokio::select! {biased;_=writer_close.changed()=>None,result=transmit_file(&core,writer,work,&mut priority_rx,&mut write_rx,&mut text_rx,&reports_tx)=>Some(result)};
                        match result {
                            Some(Ok(true))=>{let _=reports_tx.send(WriteReport::Sent{id,at:Instant::now()}).await;},
                            Some(Ok(false))=>{let _=reports_tx.send(WriteReport::Canceled(id)).await;},
                            Some(Err(error))=>{core.error(None,format!("file send interrupted: {error}"));return crate::transport::close(writer,&[]).await;},
                            None=>return crate::transport::close(writer,&[]).await,
                        }
                        continue;
                    },
                    WriterWork::Automatic(work) => {
                        let id = work.selection.id().clone();
                        let sent = tokio::select! { biased;
                            _ = writer_close.changed() => None,
                            sent = transmit_automatic(&core, writer, work, &mut priority_rx, &mut write_rx, Some(&mut text_rx), Some(&reports_tx)) => Some(sent),
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
                    sent = write_rx.send(&core, writer, &message) => Some(sent),
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
            let reason = *writer_close.borrow();
            if let Some(reason) = reason {
                return close_transport(writer, reason).await;
            }
            crate::transport::close(writer, &[]).await
    }).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileAdmission {
    New,
    RetainedReplay,
    RetainedConflict,
}
struct Incoming {
    begin: FileBegin,
    received: u64,
    first_chunk: bool,
    hash: Sha256,
    file: Option<tokio::fs::File>,
    stage: Option<Stage>,
    destination: Option<PathBuf>,
    receipt: Option<NackCode>,
    save_error: Option<NackCode>,
    storage_kind: crate::ui::ReceiveDestination,
    deduplicated: bool,
    admission: FileAdmission,
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
        CloseReason::Unpair | CloseReason::MembershipAck(_) => {
            vec![reason.message(), error_message(ErrorCode::Closing)]
        }
        _ => vec![reason.message()],
    };
    crate::transport::close(writer, &messages).await
}

/// Only the acceptor has a receive-idle deadline. An actively transmitting
/// dialer can receive nothing until END; its PING/PONG and receipt timers own
/// liveness independently of this cancellation-safe, retained frame read.
async fn read_ready_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    outbound: bool,
) -> std::result::Result<pneu_proto::Result<Option<Message>>, tokio::time::error::Elapsed> {
    if outbound {
        Ok(read_frame(reader).await)
    } else {
        tokio::time::timeout(Duration::from_secs(180), read_frame(reader)).await
    }
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
async fn enqueue<T>(
    tx: &mpsc::Sender<T>,
    message: T,
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
                sent.map_err(|_| anyhow::anyhow!("connection queue closed"))?;
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
        Self::register(
            &format!("Pneu-{}", &core.identity.device_id.as_str()[..8]),
            core.identity.fingerprint.as_str(),
            core.bound_port(),
            addresses,
        )
    }
    fn register(
        instance: &str,
        fingerprint: &str,
        port: u16,
        addresses: Vec<crate::discovery::InterfaceAddress>,
    ) -> Result<Self> {
        ensure!(!addresses.is_empty(), "no reachable interface for mDNS");
        let ips: Vec<_> = addresses.iter().map(|i| i.ip).collect();
        let mut info = mdns_sd::ServiceInfo::new(
            "_pneu._tcp.local.",
            instance,
            &format!("{instance}.local."),
            ips.as_slice(),
            port,
            [("v", "1"), ("fp", fingerprint)].as_slice(),
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

#[cfg(all(test, unix))]
mod tests {
    #[derive(Default)]
    struct BoundaryActions {
        clipboard: Mutex<String>,
        notices: Mutex<Vec<String>>,
        noticed: Notify,
        prepared: Notify,
        release: Notify,
        delay: AtomicBool,
    }
    #[async_trait::async_trait]
    impl PcActions for BoundaryActions {
        async fn notify_text(&self, text: &str) -> Result<()> {
            self.notices.lock().unwrap().push(text.into());
            self.noticed.notify_one();
            Ok(())
        }
        async fn clipboard_text(&self, text: &str) -> Result<()> {
            *self.clipboard.lock().unwrap() = text.into();
            Ok(())
        }
        async fn prepare_clipboard(
            &self,
            input: crate::actions::ClipboardInput,
            _ticket: u128,
        ) -> Result<crate::actions::PreparedClipboard> {
            if self.delay.load(Ordering::SeqCst) {
                self.prepared.notify_one();
                self.release.notified().await;
            }
            crate::actions::PreparedClipboard::immediate(input)
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
    async fn explicit_clipboard_share_rejects_an_unapplied_actor_outcome() -> Result<()> {
        let (_dir, core, peer) = fixture(Arc::new(NoActions)).await?;
        let _session = admit_ui_test_phone(&core, &peer, 1)?;
        core.shutdown_actions().await?;
        let mut core = Arc::try_unwrap(core)
            .ok()
            .expect("fixture holds no other strong Core refs");
        let (handle, mut inbox) = clipboard_driver::Handle::channel();
        core.clipboard = handle;
        // An actor may finish a superseded share successfully for deduplication,
        // while reporting that it never applied that operation.
        let actor = tokio::spawn(async move {
            let Some(clipboard_driver::Message::Submit(_, request, _permit)) = inbox.recv().await
            else {
                panic!("expected clipboard submission");
            };
            let _ = request.reply.send(Ok(clipboard_driver::Outcome {
                applied: false,
                deduplicated: true,
            }));
        });
        let result = core.send_clipboard(peer, "superseded share".into()).await;
        actor.await?;
        assert_eq!(
            result.unwrap_err().to_string(),
            "clipboard share was superseded"
        );
        Ok(())
    }
    #[tokio::test]
    async fn unapplied_stale_text_and_replays_never_report_a_copy_success() -> Result<()> {
        for mesh in [false, true] {
            let actions = Arc::new(BoundaryActions::default());
            let (_dir, core, peer) = fixture(actions.clone()).await?;
            core.store
                .settings_fields(core.identity.device_id.clone(), peer.clone(), true)
                .await?;
            let mut store = core.store.with_current_pairing();
            store.mesh = mesh;
            store.local_device = Some(core.identity.device_id.clone());
            let mut events = core.subscribe();
            let (tx, mut rx) = mpsc::channel(8);
            let winner = Clipboard {
                sensitive: false,
                transfer_id: Id::from_bytes([10; 16]),
                text: "winner".into(),
                created_at: 200,
                origin: ClipboardOrigin::Clipboard,
                hlc: mesh.then_some(200),
                source_device: mesh.then_some(peer.clone()),
            };
            let stale = Clipboard {
                transfer_id: Id::from_bytes([20; 16]),
                text: "rejected stale".into(),
                created_at: 100,
                hlc: mesh.then_some(100),
                ..winner.clone()
            };
            for (index, text) in [winner.clone(), stale, winner.clone()]
                .into_iter()
                .enumerate()
            {
                core.receive_test(&store, &peer, Message::Clipboard(text), &mut None, &tx)
                    .await?;
                let response = rx.recv().await;
                assert!(
                    matches!(response, Some(Message::Ack(Ack { deduplicated, .. })) if deduplicated == (index != 0)),
                    "mesh={mesh} index={index}: {response:?}"
                );
                if index == 0 {
                    tokio::time::timeout(Duration::from_secs(3), actions.noticed.notified())
                        .await?;
                }
            }
            core.shutdown_actions().await?;
            assert_eq!(&*actions.clipboard.lock().unwrap(), "winner");
            assert_eq!(&*actions.notices.lock().unwrap(), &["winner"]);
            let received = std::iter::from_fn(|| events.try_recv().ok())
                .filter_map(|event| match event {
                    Event::ClipboardReceived { transfer_id, .. } => Some(transfer_id),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(received, vec![winner.transfer_id]);
            let (register, legacy, _) = core.store.clipboard_snapshot().await?;
            if mesh {
                assert_eq!(register.unwrap().hlc, 200);
            } else {
                assert_eq!(legacy.get(&peer), Some(&200));
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn retained_file_replays_and_conflicts_precede_disabled_direction_at_begin_and_end()
    -> Result<()> {
        for kind in [FileKind::ClipboardImage, FileKind::Screenshot] {
            let (_dir, core, peer) = fixture(Arc::new(NoActions)).await?;
            let local = core.identity.device_id.clone();
            core.store
                .settings_fields(local.clone(), peer.clone(), true)
                .await?;
            let mut store = core.store.with_current_pairing();
            store.mesh = true;
            store.local_device = Some(local.clone());
            let bytes = b"\x89PNG\r\n\x1a\nretained";
            let mut begin = test_begin(bytes);
            begin.kind = kind;
            begin.hlc = Some(crate::now_ms());
            begin.source_device = Some(peer.clone());
            let (tx, mut rx) = mpsc::channel(8);
            let frames = |begin: FileBegin| {
                vec![
                    Message::FileBegin(begin.clone()),
                    Message::FileChunk(FileChunk {
                        transfer_id: begin.transfer_id.clone(),
                        offset: 0,
                        data: bytes.to_vec(),
                    }),
                    Message::FileEnd(FileEnd {
                        transfer_id: begin.transfer_id,
                    }),
                ]
            };
            let mut incoming = None;
            for frame in frames(begin.clone()) {
                core.receive_test(&store, &peer, frame, &mut incoming, &tx)
                    .await?;
            }
            assert!(matches!(
                rx.recv().await,
                Some(Message::Ack(Ack {
                    deduplicated: false,
                    ..
                }))
            ));
            let feature = if kind == FileKind::ClipboardImage {
                "clipboard"
            } else {
                "automatic_media"
            };
            core.store
                .settings_edit(
                    local.clone(),
                    peer.clone(),
                    true,
                    format!("{peer}/{local}/{feature}"),
                    false,
                )
                .await?;
            for conflict in [false, true] {
                let mut replay = begin.clone();
                replay.resume = Some(false); // Transport-only field must not conflict with retained metadata.
                if conflict {
                    replay.file_name = "conflict.png".into();
                }
                // END deliberately has no chunks: it cannot replace a retained
                // replay/conflict result with direction or size policy.
                core.receive_test(
                    &store,
                    &peer,
                    Message::FileBegin(replay.clone()),
                    &mut incoming,
                    &tx,
                )
                .await?;
                core.receive_test(
                    &store,
                    &peer,
                    Message::FileEnd(FileEnd {
                        transfer_id: replay.transfer_id,
                    }),
                    &mut incoming,
                    &tx,
                )
                .await?;
                let reply = rx.recv().await.unwrap();
                if conflict {
                    assert!(matches!(
                        reply,
                        Message::Nack(Nack {
                            code: NackCode::IdConflict,
                            ..
                        })
                    ));
                } else {
                    assert!(matches!(
                        reply,
                        Message::Ack(Ack {
                            deduplicated: true,
                            ..
                        })
                    ));
                }
            }
            let mut new = begin;
            new.transfer_id = Id::from_bytes(rand::random());
            for frame in frames(new) {
                core.receive_test(&store, &peer, frame, &mut incoming, &tx)
                    .await?;
            }
            assert!(matches!(
                rx.recv().await,
                Some(Message::Nack(Nack {
                    code: NackCode::UnsupportedKind,
                    ..
                }))
            ));
            core.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn watcher_observation_cancels_prepared_remote_write_and_later_winner_converges()
    -> Result<()> {
        let actions = Arc::new(BoundaryActions::default());
        actions.delay.store(true, Ordering::SeqCst);
        let (_dir, core, device) = fixture(actions.clone()).await?;
        let text = Clipboard {
            sensitive: false,
            transfer_id: Id::from_bytes(rand::random()),
            text: "older remote".into(),
            hlc: Some(crate::now_ms()),
            source_device: Some(device.clone()),
            created_at: 1,
            origin: ClipboardOrigin::Clipboard,
        };
        let (tx, mut rx) = mpsc::channel(4);
        let c = core.clone();
        let d = device.clone();
        let t = tx.clone();
        let task = tokio::spawn(async move {
            c.receive_test(&c.store, &d, Message::Clipboard(text), &mut None, &t)
                .await
        });
        actions.prepared.notified().await;
        // The physical local selection and watcher event happen while the remote
        // effect still holds the global clipboard mutex and prepares its helper.
        *actions.clipboard.lock().unwrap() = "new local".into();
        core.observe_local_clipboard_offer().await?;
        actions.release.notify_one();
        task.await??;
        assert_eq!(&*actions.clipboard.lock().unwrap(), "new local");
        assert!(matches!(
            rx.recv().await,
            Some(Message::Ack(Ack {
                deduplicated: true,
                ..
            }))
        ));
        let seen: String = core
            .store
            .call(|c| Ok(c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?))
            .await?;
        actions.delay.store(false, Ordering::SeqCst);
        core.receive_test(
            &core.store,
            &device,
            Message::Clipboard(Clipboard {
                sensitive: false,
                transfer_id: Id::from_bytes(rand::random()),
                text: "next winner".into(),
                hlc: Some(seen.parse::<u64>()? + 1),
                source_device: Some(device.clone()),
                created_at: 2,
                origin: ClipboardOrigin::Clipboard,
            }),
            &mut None,
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
        assert_eq!(&*actions.clipboard.lock().unwrap(), "next winner");
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn legacy_effect_stamp_is_allocated_after_helper_prep_and_already_observed_mesh_clock()
    -> Result<()> {
        let actions = Arc::new(BoundaryActions::default());
        actions.delay.store(true, Ordering::SeqCst);
        let (_dir, core, device) = fixture(actions.clone()).await?;
        let (tx, mut rx) = mpsc::channel(4);
        let c = core.clone();
        let d = device.clone();
        let task = tokio::spawn(async move {
            c.receive_test(
                &c.store,
                &d,
                Message::Clipboard(Clipboard {
                    sensitive: false,
                    transfer_id: Id::from_bytes(rand::random()),
                    text: "legacy now".into(),
                    hlc: None,
                    source_device: None,
                    created_at: 1,
                    origin: ClipboardOrigin::Clipboard,
                }),
                &mut None,
                &tx,
            )
            .await
        });
        actions.prepared.notified().await;
        let observed = crate::now_ms() + 500;
        core.store.observe_hlc(device.clone(), observed).await?;
        actions.release.notify_one();
        task.await??;
        assert!(matches!(
            rx.recv().await,
            Some(Message::Ack(Ack {
                deduplicated: false,
                ..
            }))
        ));
        assert!(
            !core.store.stamp_eligible(observed, device).await?,
            "an already observed mesh operation can overwrite the later legacy effect"
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn persisted_disable_cannot_resurrect_unsent_clipboard_after_reenable() -> Result<()> {
        let (_dir, core, peer) = fixture(Arc::new(NoActions)).await?;
        let _link = admit_ui_test_phone(&core, &peer, 1)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&peer)
            .unwrap()
            .mesh = true;
        let local = core.identity.device_id.clone();
        core.store
            .settings_fields(local.clone(), peer.clone(), true)
            .await?;
        let offer = core.observe_clipboard_offer().unwrap();
        core.send_observed_clipboard(offer.clone(), "must be dropped".into())
            .await?;
        let key = format!("{local}/{peer}/clipboard");
        // Store continuations stand in for an incoming merge or an IPC caller
        // canceled after admission: neither can rely on Core's post-await cleanup.
        core.store
            .settings_edit(local.clone(), peer.clone(), true, key.clone(), false)
            .await?;
        core.store
            .settings_edit(local, peer.clone(), true, key, true)
            .await?;
        assert!(!core.automatic_offer_current(&offer));
        assert!(
            core.take_automatic(&peer, 1).is_none(),
            "disabled unsent text was paused and later replayed"
        );
        core.shutdown_actions().await?;
        Ok(())
    }

    #[test]
    fn receipt_notices_share_bounded_queue_and_revocation_rules() {
        let queue = ActionQueue::default();
        let scope = ActionScope::new();
        let origin = ActionOrigin {
            mesh: false,
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
            mesh: false,
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
            let scope = self.action_scope.lock().unwrap().get(device).cloned();
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
            let scope = self.action_scope.lock().unwrap().get(&device).cloned();
            self.file_loop(store, scope, device, messages, writer, close)
                .await
        }
    }
    #[tokio::test]
    async fn settings_readiness_is_scoped_to_current_connection_and_peer() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let other = Id::from_bytes([99; 16]);
        let _other_link = admit_ui_test_phone(&core, &other, 7)?;
        let _old_link = admit_ui_test_phone(&core, &device, 1)?;
        let mut old = core.store.with_current_pairing();
        old.mesh = true;
        old.connection_id = Some(1);
        core.publish_settings_ready(&old, &device, false);
        assert!(!core.content_ready(&old, &device));
        let _new_link = admit_ui_test_phone(&core, &device, 2)?;
        let mut current = old.clone();
        current.connection_id = Some(2);
        core.publish_settings_ready(&current, &device, true);
        for ready in [true, false] {
            core.publish_settings_ready(&old, &device, ready);
            assert!(core.content_ready(&current, &device));
            assert!(!core.content_ready(&old, &device));
            assert_eq!(core.settings_ready.lock().unwrap().get(&other), Some(&7));
        }
        core.publish_settings_ready(&current, &device, false);
        assert!(!core.content_ready(&current, &device));
        current.connection_id = None;
        assert!(!core.content_ready(&current, &device));
        core.shutdown_actions().await
    }
    fn admit_ui_test_phone(
        core: &Core,
        device: &Id,
        id: u64,
    ) -> Result<watch::Receiver<Option<CloseReason>>> {
        let (close, receiver) = watch::channel(None);
        core.settings_ready
            .lock()
            .unwrap()
            .insert(device.clone(), id);
        core.admission
            .lock()
            .unwrap()
            .peers
            .retain(|_, id| id != device);
        core.admission
            .lock()
            .unwrap()
            .peers
            .insert(Hash::new(crate::random_hex(32))?, device.clone());
        core.action_scope
            .lock()
            .unwrap()
            .entry(device.clone())
            .or_insert_with(ActionScope::new)
            .connection_generation
            .store(id, Ordering::SeqCst);
        let mut state = core.sessions.lock().unwrap();
        state.sessions.insert(
            device.clone(),
            Session {
                outbound: false,
                mesh: false,
                role: "phone".into(),
                device: device.clone(),
                id,
                close,
                wake: Arc::new(Notify::new()),
                paused: false,
                capabilities: vec!["clipboard_text".into(), "clipboard_image".into()],
                phone: crate::ui::Phone::default(),
            },
        );
        core.publish_phone(
            &state,
            id,
            &Hello {
                setup: None,
                role: None,
                addresses: None,
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
    #[tokio::test]
    async fn notification_ack_releases_the_authority_gate_and_revocation_cancels_wait() -> Result<()>
    {
        let scope = ActionScope::new();
        let origin = ActionOrigin {
            mesh: false,
            device: Id::new("11".repeat(16))?,
            connection_id: Some(0),
        };
        let issued = Arc::new(tokio::sync::Notify::new());
        let alive = Arc::new(AtomicU64::new(1));
        let task = {
            let scope = scope.clone();
            let origin = origin.clone();
            let issued = issued.clone();
            let alive = alive.clone();
            tokio::spawn(async move {
                scope
                    .notification(&origin, || async move {
                        let lifetime = ViewerLifetime(alive);
                        issued.notify_one();
                        Ok(Box::pin(async move {
                            let _lifetime = lifetime;
                            std::future::pending().await
                        }) as crate::ui::portal::Interaction)
                    })
                    .await
            })
        };
        issued.notified().await;
        tokio::time::timeout(
            Duration::from_millis(100),
            scope.effect(&origin, || async { 42 }),
        )
        .await?
        .unwrap();
        assert_eq!(alive.load(Ordering::SeqCst), 1);
        scope.cancelled.send_replace(true);
        assert!(task.await?.is_none());
        assert_eq!(alive.load(Ordering::SeqCst), 0);
        Ok(())
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
            .get(&device)
            .unwrap()
            .clone();
        let job = ActionJob {
            file: ReceivedFile::from_file(dir.path().join("file"), tempfile::tempfile()?)?,
            scope: scope.clone(),
            origin: ActionOrigin {
                mesh: false,
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
                mesh: false,
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
                mesh: false,
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
            .get(&device)
            .unwrap()
            .clone();
        let path = dir.path().join("image.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n")?;
        let job = ActionJob {
            file: ReceivedFile::open(&path)?,
            scope,
            origin: ActionOrigin {
                mesh: false,
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
    #[tokio::test]
    async fn writer_closed_queue_preserves_concurrent_close_reason() -> Result<()> {
        for reason in [
            None,
            Some(CloseReason::Error(ErrorCode::Unauthorized)),
            Some(CloseReason::Unpair),
        ] {
            let (_dir, core, _) = fixture(Arc::new(NoActions)).await?;
            let (normal_tx, normal) = mpsc::channel(1);
            let (priority_tx, priority) = mpsc::channel(1);
            let (automatic_tx, automatic) = mpsc::channel(1);
            let (text_tx, text) = mpsc::channel(1);
            let (files_tx, files) = mpsc::channel(1);
            let (reports, _reported) = mpsc::channel(1);
            let (close_tx, close) = watch::channel(None);
            let polled = Arc::new(AtomicBool::new(false));
            let observed = polled.clone();
            let mut bytes = Vec::new();
            writer_loop(
                core.clone(),
                &mut bytes,
                WriterQueues {
                    normal: normal.into(),
                    priority,
                    automatic,
                    text,
                    files,
                    reports,
                    close,
                    before_queue_poll: Some(Box::new(move || {
                        // The biased select has already polled changed() as
                        // Pending. Publish the reason before dropping every
                        // sender, as READY teardown does, in this same poll.
                        if let Some(reason) = reason {
                            close_tx.send_replace(Some(reason));
                        }
                        drop((normal_tx, priority_tx, automatic_tx, text_tx, files_tx));
                        observed.store(true, Ordering::SeqCst);
                    })),
                },
            )
            .await?;
            core.shutdown_actions().await?;
            assert!(polled.load(Ordering::SeqCst), "queue arm was not polled");
            let mut wire = std::io::Cursor::new(bytes);
            if let Some(reason) = reason {
                assert_eq!(read_frame(&mut wire).await?, Some(reason.message()));
                if matches!(reason, CloseReason::Unpair) {
                    assert_eq!(
                        read_frame(&mut wire).await?,
                        Some(error_message(ErrorCode::Closing))
                    );
                }
            }
            assert_eq!(read_frame(&mut wire).await?, None);
        }
        Ok(())
    }

    #[tokio::test]
    async fn writer_canceled_partial_frame_does_not_append_close_reason() -> Result<()> {
        use std::future::Future;
        use std::task::Poll;
        use tokio::io::AsyncReadExt;

        for reason in [
            CloseReason::Error(ErrorCode::Unauthorized),
            CloseReason::Unpair,
        ] {
            let (_dir, core, _) = fixture(Arc::new(NoActions)).await?;
            let (normal_tx, normal) = mpsc::channel(1);
            let (_priority_tx, priority) = mpsc::channel(1);
            let (_automatic_tx, automatic) = mpsc::channel(1);
            let (_text_tx, text) = mpsc::channel(1);
            let (_files_tx, files) = mpsc::channel(1);
            let (reports, _reported) = mpsc::channel(1);
            let (close_tx, close) = watch::channel(None);
            normal_tx
                .send(Message::Ping(Ping {
                    nonce: Nonce::new("0123456789abcdef")?,
                }))
                .await?;
            let (mut stream, mut wire) = tokio::io::duplex(1);
            let writer = writer_loop(
                core.clone(),
                &mut stream,
                WriterQueues {
                    normal: normal.into(),
                    priority,
                    automatic,
                    text,
                    files,
                    reports,
                    close,
                    before_queue_poll: None,
                },
            );
            tokio::pin!(writer);
            // One byte fits, so the first frame must suspend partway through.
            std::future::poll_fn(|cx| {
                assert!(writer.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            close_tx.send_replace(Some(reason));
            writer.await?;
            let mut bytes = Vec::new();
            wire.read_to_end(&mut bytes).await?;
            core.shutdown_actions().await?;
            assert_eq!(bytes.len(), 1, "close reason appended to a partial frame");
        }
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
        for image in [false, true] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let close = admit_ui_test_phone(&core, &device, 1)?;
            core.set_setting("clipboard_include_images".into(), true.into())
                .await?;
            let work = test_automatic_work(&core, &device, image).await?;
            let id = work.selection.id().clone();
            let mut pending = PendingOutgoing::queued(id.clone());
            let (_normal_tx, normal) = mpsc::channel(4);
            let (_priority_tx, priority) = mpsc::channel(4);
            let (automatic_tx, automatic) = mpsc::channel(1);
            let (_text_tx, text) = mpsc::channel(1);
            let (_files_tx, files) = mpsc::channel(1);
            let (reports, mut rx) = mpsc::channel(4);
            automatic_tx
                .send(work)
                .await
                .map_err(|_| anyhow::anyhow!("writer queue"))?;
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
                        normal: normal.into(),
                        priority,
                        automatic,
                        text,
                        files,
                        reports,
                        close,
                        before_queue_poll: None,
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
                core.set_setting("clipboard_include_images".into(), true.into())
                    .await?;
                let work = test_automatic_work(&core, &device, image).await?;
                let stale = work.selection.id().clone();
                let (normal_tx, normal) = mpsc::channel(4);
                let (_priority_tx, priority) = mpsc::channel(4);
                let (automatic_tx, automatic) = mpsc::channel(1);
                let (_text_tx, text) = mpsc::channel(1);
                let (_files_tx, files) = mpsc::channel(1);
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
                            normal: normal.into(),
                            priority,
                            automatic,
                            text,
                            files,
                            reports: canceled,
                            close,
                            before_queue_poll: None,
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
            core.set_setting("clipboard_include_images".into(), true.into())
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
            let (_normal_tx, normal) = mpsc::channel(1);
            let mut normal = phone_controls::FrameQueue::from(normal);
            let task = tokio::spawn(Box::pin(async move {
                transmit_automatic(
                    &writer_core,
                    &mut writer,
                    work,
                    &mut priority,
                    &mut normal,
                    None,
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
        assert!(core.ui.lock().unwrap().receiving_until.is_empty());
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
            core.unpair_device(old.device.clone().unwrap()).await?;
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
    async fn cached_phone_metadata_is_bound_to_identity_and_pairing_after_restart() -> Result<()> {
        for filename in ["phone.json", "phones.json"] {
            for cache in ["matching", "different", "legacy", "unversioned", "revoked"] {
                let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
                let _connection = admit_ui_test_phone(&core, &device, 1)?;
                core.persist_phone(1).await?;
                let paths = core.paths.clone();
                let config = core.config();
                core.shutdown_actions().await?;
                drop(core);
                let other = if filename == "phone.json" {
                    "phones.json"
                } else {
                    "phone.json"
                };
                std::fs::remove_file(paths.data.join(other))?;
                let file = paths.data.join(filename);
                let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&file)?)?;
                let phone = if filename == "phones.json" {
                    &mut value[device.as_str()]
                } else {
                    &mut value
                };
                if cache == "legacy" {
                    phone.as_object_mut().unwrap().remove("device");
                }
                if cache == "different" {
                    phone["device"] = serde_json::to_value(Id::from_bytes([9; 16]))?;
                }
                if cache == "unversioned" {
                    phone.as_object_mut().unwrap().remove("pairing_generation");
                }
                if cache == "revoked" {
                    phone["pairing_generation"] = serde_json::json!(0);
                }
                crate::config::atomic_private(&file, &serde_json::to_vec(&value)?)?;
                let restarted = Core::new(paths, config, Arc::new(NoActions)).await?;
                let snapshot = restarted.ui_snapshot();
                restarted.shutdown_actions().await?;
                assert_eq!(snapshot.device, Some(device));
                assert_eq!(snapshot.phone.device, snapshot.device);
                assert_eq!(
                    snapshot.phone.name,
                    if cache == "matching" {
                        "Phone 1"
                    } else {
                        "Phone"
                    },
                    "{filename}: {cache}"
                );
                assert_eq!(snapshot.phone.last_seen != 0, cache == "matching");
                assert_eq!(snapshot.phone.route.is_empty(), cache != "matching");
                assert!(snapshot.generation.is_none());
                assert!(snapshot.paired && !snapshot.connected && !snapshot.paused);
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn current_phone_cache_survives_a_revoked_entry_in_the_other_file() -> Result<()> {
        for stale_file in ["phone.json", "phones.json"] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let _old = admit_ui_test_phone(&core, &device, 1)?;
            core.persist_phone(1).await?;
            let path = core.paths.data.join(stale_file);
            let stale = std::fs::read(&path)?;
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
            let _current = admit_ui_test_phone(&core, &device, 2)?;
            core.persist_phone(2).await?;
            let paths = core.paths.clone();
            let config = core.config();
            core.store.drain().await;
            core.shutdown_actions().await?;
            drop(core);
            crate::config::atomic_private(&path, &stale)?;
            let restarted = Core::new(paths, config, Arc::new(NoActions)).await?;
            let snapshot = restarted.ui_snapshot();
            restarted.shutdown_actions().await?;
            assert_eq!(snapshot.phone.name, "Phone 2", "stale {stale_file}");
            assert!(snapshot.phone.last_seen > 0);
            assert!(!snapshot.phone.route.is_empty());
        }
        Ok(())
    }
    #[tokio::test]
    async fn repaired_pairing_does_not_restore_revoked_metadata_after_restart() -> Result<()> {
        revoked_metadata_restart(false, true).await
    }
    #[tokio::test]
    async fn unpaired_device_does_not_restore_metadata_after_restart() -> Result<()> {
        revoked_metadata_restart(true, false).await
    }
    #[tokio::test]
    async fn unpair_then_pair_does_not_restore_revoked_metadata_after_restart() -> Result<()> {
        revoked_metadata_restart(true, true).await
    }
    async fn revoked_metadata_restart(unpair: bool, repair: bool) -> Result<()> {
        for cache in ["phone.json", "phones.json", "both"] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let connection = admit_ui_test_phone(&core, &device, 1)?;
            core.persist_phone(1).await?;
            assert!(core.finish_connection(&device, 1));
            core.persist_phone(1).await?;
            let paths = core.paths.clone();
            let config = core.config();
            // Keep the old bytes, modelling a crash after the trust transaction
            // commits but before any cache invalidation/write can finish.
            let old_phone = std::fs::read(paths.data.join("phone.json"))?;
            let old_phones = std::fs::read(paths.data.join("phones.json"))?;
            if unpair {
                core.unpair_device(device.clone()).await?;
            }
            if repair {
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
                assert_eq!(core.ui_snapshot().phone.name, "Phone");
            }
            core.store.drain().await;
            core.shutdown_actions().await?;
            drop(connection);
            drop(core);
            for (file, bytes) in [("phone.json", old_phone), ("phones.json", old_phones)] {
                if cache == "both" || cache == file {
                    crate::config::atomic_private(&paths.data.join(file), &bytes)?;
                } else {
                    std::fs::remove_file(paths.data.join(file))?;
                }
            }
            let restarted = Core::new(paths, config, Arc::new(NoActions)).await?;
            let snapshot = restarted.ui_snapshot();
            let caches_empty = restarted.ui.lock().unwrap().phones.is_empty();
            restarted.shutdown_actions().await?;
            assert!(
                caches_empty,
                "revoked entry loaded from {cache}, unpair={unpair}"
            );
            assert_eq!(
                snapshot.phone.name,
                if repair { "Phone" } else { "" },
                "cache={cache}"
            );
            assert_eq!(snapshot.phone.last_seen, 0);
            assert!(snapshot.phone.route.is_empty());
            assert_eq!(snapshot.device, repair.then_some(device));
            assert!(!snapshot.connected);
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
    #[cfg(target_os = "linux")]
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
    async fn sweep_activity_is_per_device_and_disconnect_reconnect_scoped() -> Result<()> {
        let (_dir, core, a) = fixture(Arc::new(NoActions)).await?;
        let b = Id::from_bytes([2; 16]);
        let _a = admit_ui_test_phone(&core, &a, 1)?;
        let _b = admit_ui_test_phone(&core, &b, 2)?;
        let transfer = test_begin(b"same ID, different senders");
        core.event(Event::TransferStarted {
            device: b.clone(),
            metadata: transfer.clone(),
        });
        assert!(
            !core.ui_snapshot().receiving,
            "B must not show Receiving from A"
        );
        let before_start = std::time::Instant::now();
        core.event(Event::TransferStarted {
            device: a.clone(),
            metadata: transfer.clone(),
        });
        let after_start = std::time::Instant::now();
        let until =
            core.ui.lock().unwrap().receiving_until[&(a.clone(), transfer.transfer_id.clone())];
        let hold = Duration::from_millis(1500);
        assert!(
            (before_start + hold..=after_start + hold).contains(&until),
            "receiving presentation must retain its 1500 ms hold"
        );
        assert_eq!(core.ui.lock().unwrap().active_transfers.len(), 2);
        core.event(Event::TransferFinished {
            device: a.clone(),
            transfer_id: transfer.transfer_id.clone(),
            path: None,

            deduplicated: false,
        });
        // Expire A's presentation hold explicitly; this test checks per-device
        // activity ownership, not how quickly the host schedules a sleep.
        *core
            .ui
            .lock()
            .unwrap()
            .receiving_until
            .get_mut(&(a.clone(), transfer.transfer_id.clone()))
            .unwrap() = std::time::Instant::now() - Duration::from_secs(1);
        assert!(
            !core.ui_snapshot().receiving,
            "B's equal ID must remain independent"
        );
        let _replacement = admit_ui_test_phone(&core, &a, 3)?;
        assert_eq!(
            core.ui.lock().unwrap().active_transfers.len(),
            1,
            "A reconnect must preserve B activity"
        );
        assert!(core.finish_connection(&b, 2));
        assert!(
            core.ui.lock().unwrap().active_transfers.is_empty(),
            "secondary disconnect must remove its own unfinished receive"
        );
        assert!(!core.ui_snapshot().receiving);
        let _b_reconnected = admit_ui_test_phone(&core, &b, 4)?;
        core.event(Event::TransferStarted {
            device: b.clone(),
            metadata: transfer,
        });
        assert!(core.finish_connection(&a, 3));
        let snapshot = core.ui_snapshot();
        assert_eq!(snapshot.device, Some(b.clone()));
        assert_eq!(snapshot.phone.name, "Phone 4");
        assert!(snapshot.receiving);
        assert_eq!(snapshot.status(), "Receiving from Phone 4…");
        assert!(core.finish_connection(&b, 4));
        assert!(core.ui.lock().unwrap().active_transfers.is_empty());
        assert!(!core.ui_snapshot().receiving);
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn sweep_display_prefers_lowest_connected_peer_and_keeps_it() -> Result<()> {
        let (_dir, core, a) = fixture(Arc::new(NoActions)).await?;
        let b = Id::from_bytes([2; 16]);
        let c = Id::from_bytes([3; 16]);
        let _a = admit_ui_test_phone(&core, &a, 1)?;
        let _c = admit_ui_test_phone(&core, &c, 2)?;
        let _b = admit_ui_test_phone(&core, &b, 3)?;
        assert_eq!(core.ui_snapshot().device, Some(a.clone()));
        assert!(core.finish_connection(&a, 1));
        assert_eq!(core.ui_snapshot().device, Some(b.clone()));
        let _a_again = admit_ui_test_phone(&core, &a, 4)?;
        assert_eq!(
            core.ui_snapshot().device,
            Some(b.clone()),
            "a live displayed peer stays selected"
        );
        core.set_phone_paused(&b, 3, true);
        let snapshot = core.ui_snapshot();
        assert_eq!(snapshot.phone.name, "Phone 3");
        assert_eq!(snapshot.generation, Some(3));
        assert!(snapshot.connected && snapshot.paused);
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn sweep_display_switch_persists_ended_peer_metadata_across_restart() -> Result<()> {
        for shutdown in [false, true] {
            let (_dir, core, a) = fixture(Arc::new(NoActions)).await?;
            let b = Id::from_bytes([2; 16]);
            core.store
                .pair(b.clone(), Hash::new(crate::random_hex(32))?)
                .await?;
            let _a = admit_ui_test_phone(&core, &a, 1)?;
            let _b = admit_ui_test_phone(&core, &b, 2)?;
            core.ui.lock().unwrap().snapshot.phone.last_seen = 1;
            core.persist_phone(1).await?;
            assert_eq!(core.ui_snapshot().device, Some(a.clone()));
            if shutdown {
                // Use the production serve shutdown path with both sessions live.
                core.request_shutdown();
                core.clone()
                    .serve(TcpListener::bind("127.0.0.1:0").await?, false)
                    .await?;
            } else {
                assert!(core.finish_connection(&a, 1));
                core.persist_phone(1).await?;
                let snapshot = core.ui_snapshot();
                assert_eq!(snapshot.device, Some(b.clone()));
                assert_eq!(snapshot.phone.device, snapshot.device);
                assert_eq!(snapshot.phone.name, "Phone 2");
                assert!(snapshot.connected);
                let disk: crate::ui::Phone =
                    serde_json::from_slice(&std::fs::read(core.paths.data.join("phone.json"))?)?;
                assert_eq!(disk.device, snapshot.device);
                assert_eq!(disk.name, snapshot.phone.name);
                assert!(core.finish_connection(&b, 2));
                core.persist_phone(2).await?;
            }
            let paths = core.paths.clone();
            let config = core.config();
            core.store.drain().await;
            core.shutdown_actions().await?;
            drop(core);
            let core = Core::new(paths, config, Arc::new(NoActions)).await?;
            core.unpair_device(b).await?;
            let snapshot = core.ui_snapshot();
            assert_eq!(snapshot.device, Some(a));
            assert_eq!(snapshot.phone.name, "Phone 1");
            assert!(
                snapshot.phone.last_seen > 1,
                "A final last seen must survive B staying connected and restart"
            );
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
        let unpaired = std::fs::read(&path)?;
        core.persist_phone(2).await?;
        assert_eq!(
            std::fs::read(&path)?,
            unpaired,
            "revoked metadata cannot overwrite the unpaired snapshot"
        );
        assert!(
            serde_json::from_slice::<crate::ui::Phone>(&unpaired)?
                .device
                .is_none()
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn obsolete_admission_cannot_publish_or_persist_phone_metadata() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        core.admission.lock().unwrap().peers = core.store.peers().await?;
        let (close, _rx) = watch::channel(None);
        core.sessions.lock().unwrap().sessions.insert(
            device.clone(),
            Session {
                outbound: false,
                mesh: false,
                role: "phone".into(),
                device: device.clone(),
                id: 2,
                close,
                wake: Arc::new(Notify::new()),
                paused: false,
                capabilities: vec!["clipboard_text".into(), "clipboard_image".into()],
                phone: crate::ui::Phone::default(),
            },
        );
        let current = Hello {
            setup: None,
            role: None,
            addresses: None,
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
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        assert!(
            !core
                .automatic_target()
                .unwrap()
                .payload_plans(false, true)
                .is_empty()
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    async fn automatic_wire(core: &Core, work: AutomaticWrite) -> Result<Vec<Message>> {
        let (_priority_tx, mut priority) = mpsc::channel(1);
        let (_normal_tx, normal) = mpsc::channel(1);
        let mut normal = phone_controls::FrameQueue::from(normal);
        let mut bytes = Vec::new();
        assert!(
            transmit_automatic(
                core,
                &mut bytes,
                work,
                &mut priority,
                &mut normal,
                None,
                None
            )
            .await?
        );
        let mut wire = bytes.as_slice();
        let mut messages = Vec::new();
        while let Some(message) = read_frame(&mut wire).await? {
            messages.push(message);
        }
        Ok(messages)
    }
    #[tokio::test]
    async fn encoded_clipboard_rejected_before_automatic_publication() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _link = admit_ui_test_phone(&core, &device, 1)?;
        for mesh in [false, true] {
            core.sessions
                .lock()
                .unwrap()
                .sessions
                .get_mut(&device)
                .unwrap()
                .mesh = mesh;
            let result = core
                .send_observed_clipboard(
                    core.observe_clipboard_offer().unwrap(),
                    "\u{1}".repeat(11_000),
                )
                .await;
            assert!(
                result.is_err(),
                "oversized JSON was accepted for mesh={mesh}"
            );
            assert!(core.automatic.lock().unwrap().selections.is_empty());
            assert_eq!(core.connected_devices(), vec![device.clone()]);
        }
        core.shutdown_actions().await
    }
    #[tokio::test]
    async fn encoded_clipboard_failure_is_per_recipient_and_keeps_valid_image() -> Result<()> {
        let (_dir, core, text_peer) = fixture(Arc::new(NoActions)).await?;
        let image_peer = Id::from_bytes([2; 16]);
        let _text = admit_ui_test_phone(&core, &text_peer, 1)?;
        let _image = admit_ui_test_phone(&core, &image_peer, 2)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&text_peer)
            .unwrap()
            .capabilities = vec!["clipboard_text".into()];
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        core.send_observed_formats(
            core.observe_clipboard_offer().unwrap(),
            Some("\u{1}".repeat(11_000)),
            Some(b"\x89PNG\r\n\x1a\nimage".to_vec()),
        )
        .await?;
        assert!(
            core.take_automatic(&text_peer, 1).is_none(),
            "invalid text was published"
        );
        let wire = automatic_wire(&core, core.take_automatic(&image_peer, 2).unwrap()).await?;
        assert!(matches!(&wire[0], Message::FileBegin(_)));
        assert_eq!(core.connected_devices().len(), 2);
        core.shutdown_actions().await
    }
    #[tokio::test]
    async fn encoded_mesh_header_limit_rejects_only_mesh_recipient() -> Result<()> {
        let (_dir, core, mesh_peer) = fixture(Arc::new(NoActions)).await?;
        let legacy_peer = Id::from_bytes([2; 16]);
        let _mesh = admit_ui_test_phone(&core, &mesh_peer, 1)?;
        let _legacy = admit_ui_test_phone(&core, &legacy_peer, 2)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&mesh_peer)
            .unwrap()
            .mesh = true;
        let mut wire = Clipboard {
            sensitive: false,
            transfer_id: Id::from_bytes([0; 16]),
            text: "\u{1}".repeat(10_800),
            created_at: crate::now_ms(),
            origin: ClipboardOrigin::Clipboard,
            hlc: None,
            source_device: None,
        };
        let remaining = MAX_CONTROL_PAYLOAD - serde_json::to_vec(&wire)?.len();
        wire.text.push_str(&"x".repeat(remaining));
        encode_frame(&Message::Clipboard(wire.clone()))?;
        let offer = core.observe_clipboard_offer().unwrap();
        core.send_observed_clipboard(offer, wire.text.clone())
            .await?;
        assert!(core.take_automatic(&mesh_peer, 1).is_none());
        let messages = automatic_wire(&core, core.take_automatic(&legacy_peer, 2).unwrap()).await?;
        assert!(
            matches!(&messages[0],Message::Clipboard(text) if text.text==wire.text && text.hlc.is_none())
        );
        assert_eq!(core.connected_devices().len(), 2);
        core.shutdown_actions().await
    }
    #[tokio::test]
    async fn only_implemented_protocol_extensions_are_advertised() -> Result<()> {
        let (_dir, core, peer) = fixture(Arc::new(NoActions)).await?;
        let hello = core.computer_hello();
        assert_eq!(
            hello.capabilities,
            [
                "files",
                "file_resume_v1",
                "folder_v1",
                "clipboard_text",
                "clipboard_image",
                "mesh",
                "setup",
                "battery_v1",
                "ring_v1",
                "notifications_v1"
            ]
        );
        let mut incoming = None;
        let (tx, mut rx) = mpsc::channel(8);
        {
            let mut begin = test_begin(b"abc");
            begin.resume = Some(true);
            let error = core
                .receive_test(
                    &core.store,
                    &peer,
                    Message::FileBegin(begin),
                    &mut incoming,
                    &tx,
                )
                .await
                .unwrap_err();
            assert_eq!(processing_error_code(&error), ErrorCode::UnexpectedMessage);
            assert!(incoming.is_none());
            assert!(rx.try_recv().is_err());
        }
        core.shutdown_actions().await
    }
    #[tokio::test]
    async fn hello_and_status_bounds_are_validated_before_publication() -> Result<()> {
        let (_dir, core, _peer) = fixture(Arc::new(NoActions)).await?;
        let mut config = core.config();
        config.name = "\u{1}".repeat(128);
        core.update_config(config.clone()).await?;
        let mut hello = core.computer_hello();
        hello.addresses = Some(
            ['a', 'b', 'c']
                .map(|last| {
                    format!(
                        "{}.{}.{}.{}:65535",
                        "a".repeat(63),
                        "b".repeat(63),
                        "c".repeat(63),
                        last.to_string().repeat(61)
                    )
                })
                .to_vec(),
        );
        let message = Message::Hello(hello.clone());
        let encoded = encode_frame(&message)?;
        assert_eq!(read_frame(&mut encoded.as_slice()).await?, Some(message));
        config.name.push('x');
        assert!(core.update_config(config).await.is_err());
        assert_eq!(core.config().name, hello.name);
        hello.addresses.as_mut().unwrap()[0] = format!("{}:65535", "x".repeat(254));
        assert!(encode_frame(&Message::Hello(hello)).is_err());
        for paused in [false, true] {
            let message = Message::Status(Status { paused });
            let encoded = encode_frame(&message)?;
            assert_eq!(read_frame(&mut encoded.as_slice()).await?, Some(message));
        }
        core.shutdown_actions().await
    }
    #[tokio::test]
    async fn encoded_image_begin_rejected_before_automatic_publication() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _link = admit_ui_test_phone(&core, &device, 1)?;
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        let offer = core.observe_clipboard_offer().unwrap();
        core.store
            .call(|c| {
                c.execute("UPDATE mesh_clock SET last_seen=?", [u64::MAX.to_string()])?;
                Ok(())
            })
            .await?;
        assert!(
            core.send_observed_image(offer, b"\x89PNG\r\n\x1a\nimage".to_vec())
                .await
                .is_err()
        );
        assert!(
            core.automatic.lock().unwrap().selections.is_empty(),
            "invalid BEGIN was published"
        );
        core.shutdown_actions().await
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn automatic_formats_ignore_paused_image_peer() -> Result<()> {
        let (_dir, core, text_peer) = fixture(Arc::new(NoActions)).await?;
        let image_peer = Id::from_bytes([2; 16]);
        let _text = admit_ui_test_phone(&core, &text_peer, 1)?;
        let _image = admit_ui_test_phone(&core, &image_peer, 2)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&text_peer)
            .unwrap()
            .capabilities = vec!["clipboard_text".into()];
        core.set_phone_paused(&image_peer, 2, true);
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        crate::ui::clipboard::tests::deliver_offer(
            &core,
            &[
                ("image/png", b"\x89PNG\r\n\x1a\nimage"),
                ("text/plain", b"fallback"),
            ],
        )
        .await?;
        let work = core
            .take_automatic(&text_peer, 1)
            .expect("text peer must receive fallback");
        assert!(
            matches!(automatic_wire(&core, work).await?.as_slice(), [Message::Clipboard(c)] if c.text == "fallback")
        );
        assert!(core.take_automatic(&image_peer, 2).is_none());
        core.shutdown_actions().await?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn automatic_formats_deliver_best_format_to_each_peer() -> Result<()> {
        let (_dir, core, image_peer) = fixture(Arc::new(NoActions)).await?;
        let text_peer = Id::from_bytes([2; 16]);
        let _image = admit_ui_test_phone(&core, &image_peer, 1)?;
        let _text = admit_ui_test_phone(&core, &text_peer, 2)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&text_peer)
            .unwrap()
            .capabilities = vec!["clipboard_text".into()];
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        let requested = crate::ui::clipboard::tests::deliver_offer(
            &core,
            &[
                ("image/png", b"\x89PNG\r\n\x1a\nimage"),
                ("text/plain", b"fallback"),
            ],
        )
        .await?;
        let image = core
            .take_automatic(&image_peer, 1)
            .expect("image recipient");
        let image_wire = automatic_wire(&core, image).await?;
        assert!(
            matches!(image_wire.as_slice(), [Message::FileBegin(begin), Message::FileChunk(chunk), Message::FileEnd(end)]
            if begin.mime == "image/png" && begin.kind == FileKind::ClipboardImage
                && begin.transfer_id == end.transfer_id && chunk.data == b"\x89PNG\r\n\x1a\nimage")
        );
        let text = core.take_automatic(&text_peer, 2).expect("text recipient");
        assert!(
            matches!(automatic_wire(&core, text).await?.as_slice(), [Message::Clipboard(c)] if c.text == "fallback")
        );
        assert!(requested.iter().any(|m| m == "image/png"));
        assert!(requested.iter().any(|m| m == "text/plain"));
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn mesh_fanout_formats_share_one_stamp_and_legacy_wire_uses_hlc() -> Result<()> {
        let (_dir, core, image_peer) = fixture(Arc::new(NoActions)).await?;
        let text_peer = Id::from_bytes([2; 16]);
        let legacy_peer = Id::from_bytes([3; 16]);
        let _a = admit_ui_test_phone(&core, &image_peer, 1)?;
        let _b = admit_ui_test_phone(&core, &text_peer, 2)?;
        let _c = admit_ui_test_phone(&core, &legacy_peer, 3)?;
        {
            let mut sessions = core.sessions.lock().unwrap();
            sessions.sessions.get_mut(&image_peer).unwrap().mesh = true;
            let text = sessions.sessions.get_mut(&text_peer).unwrap();
            text.mesh = true;
            text.capabilities = vec!["clipboard_text".into()];
            sessions
                .sessions
                .get_mut(&legacy_peer)
                .unwrap()
                .capabilities = vec!["clipboard_text".into()];
        }
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        let offer = core.observe_clipboard_offer().unwrap();
        core.send_observed_formats(
            offer,
            Some("fallback".into()),
            Some(b"\x89PNG\r\n\x1a\nimage".to_vec()),
        )
        .await?;
        let image = automatic_wire(&core, core.take_automatic(&image_peer, 1).unwrap()).await?;
        let text = automatic_wire(&core, core.take_automatic(&text_peer, 2).unwrap()).await?;
        let legacy = automatic_wire(&core, core.take_automatic(&legacy_peer, 3).unwrap()).await?;
        let Message::FileBegin(image) = &image[0] else {
            panic!("image")
        };
        let Message::Clipboard(text) = &text[0] else {
            panic!("text")
        };
        let Message::Clipboard(legacy) = &legacy[0] else {
            panic!("legacy")
        };
        assert_eq!(image.hlc, text.hlc);
        assert!(text.hlc.is_some());
        assert_eq!(image.source_device, text.source_device);
        assert_eq!(text.source_device.as_ref(), Some(&core.identity.device_id));
        assert_eq!(image.transfer_id, text.transfer_id);
        assert_eq!(text.transfer_id, legacy.transfer_id);
        assert_eq!(image.created_at, text.created_at);
        assert_eq!(text.created_at, legacy.created_at);
        assert!(legacy.hlc.is_none() && legacy.source_device.is_none());
        assert_eq!(legacy.created_at, text.hlc.unwrap());
        let hlc: String = core
            .store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT last_seen FROM mesh_clock WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await?;
        assert_eq!(hlc, text.hlc.unwrap().to_string());
        core.shutdown_actions().await
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn automatic_formats_validate_independently_before_recipient_preference() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _connection = admit_ui_test_phone(&core, &device, 1)?;
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        let png = b"\x89PNG\r\n\x1a\nvalid image".to_vec();
        for bytes in [
            b"invalid PNG".to_vec(),
            vec![1; crate::ui::clipboard::MAX_IMAGE + 1],
        ] {
            core.send_observed_formats(
                core.observe_clipboard_offer().unwrap(),
                Some("fallback".into()),
                Some(bytes),
            )
            .await?;
            let work = core.take_automatic(&device, 1).expect("text fallback");
            assert!(
                matches!(automatic_wire(&core, work).await?.as_slice(), [Message::Clipboard(c)] if c.text == "fallback")
            );
        }
        // An unusable text representation must not block the image for one
        // peer merely because a different frozen recipient needs text.
        let text_peer = Id::from_bytes([2; 16]);
        let _text = admit_ui_test_phone(&core, &text_peer, 2)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&text_peer)
            .unwrap()
            .capabilities = vec!["clipboard_text".into()];
        for text in ["bad\0text".to_owned(), "t".repeat(MAX_CLIPBOARD_BYTES + 1)] {
            core.send_observed_formats(
                core.observe_clipboard_offer().unwrap(),
                Some(text),
                Some(png.clone()),
            )
            .await?;
            let work = core
                .take_automatic(&device, 1)
                .expect("valid image survives invalid text");
            assert!(matches!(
                automatic_wire(&core, work).await?.as_slice(),
                [
                    Message::FileBegin(_),
                    Message::FileChunk(_),
                    Message::FileEnd(_)
                ]
            ));
            assert!(core.take_automatic(&text_peer, 2).is_none());
        }
        core.set_phone_paused(&text_peer, 2, true);
        let requested = crate::ui::clipboard::tests::deliver_offer(
            &core,
            &[("text/plain", b"fallback"), ("image/png", b"invalid PNG")],
        )
        .await?;
        assert_eq!(requested, ["text/plain", "image/png"]);
        let work = core
            .take_automatic(&device, 1)
            .expect("image-capable recipient falls back");
        assert!(
            matches!(automatic_wire(&core, work).await?.as_slice(), [Message::Clipboard(c)] if c.text == "fallback")
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn failed_local_clock_commit_keeps_remote_effects_fenced_until_recovery() -> Result<()> {
        let (_dir, core, _device) = fixture(Arc::new(NoActions)).await?;
        core.store.call(|c| {
            c.execute_batch("CREATE TRIGGER reject_clock BEFORE UPDATE ON mesh_clock BEGIN SELECT RAISE(ABORT,'injected clock failure'); END;")?;
            Ok(())
        }).await?;
        let mut events = core.subscribe();
        let delivery = tokio::spawn({
            let core = core.clone();
            async move {
                crate::ui::clipboard::tests::deliver_offer(&core, &[("text/plain", b"local")]).await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Event::Error { message, .. } = events.recv().await?
                    && message.starts_with("clipboard clock unavailable:")
                {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await??;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), core.wait_clipboard_observation())
                .await
                .is_err()
        );
        core.store
            .call(|c| {
                c.execute_batch("DROP TRIGGER reject_clock;")?;
                Ok(())
            })
            .await?;
        // The original watcher operation must recover without a second copy.
        tokio::time::timeout(Duration::from_secs(2), delivery).await???;
        tokio::time::timeout(Duration::from_secs(1), core.wait_clipboard_observation()).await?;
        core.shutdown_actions().await
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn watcher_classification_is_independent_of_paused_disabled_and_offline_policy()
    -> Result<()> {
        for policy in [
            "connected",
            "disabled",
            "paused",
            "pair_send_off",
            "offline",
        ] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let _connection = admit_ui_test_phone(&core, &device, 1)?;
            match policy {
                "disabled" => {
                    core.toggle_setting(crate::ui::Toggle::Clipboard).await?;
                }
                "paused" => core.set_phone_paused(&device, 1, true),
                "offline" => {
                    core.sessions.lock().unwrap().sessions.clear();
                }
                "pair_send_off" => {
                    core.sessions
                        .lock()
                        .unwrap()
                        .sessions
                        .get_mut(&device)
                        .unwrap()
                        .mesh = true;
                    core.store
                        .settings_fields(core.identity.device_id.clone(), device.clone(), true)
                        .await?;
                    core.store
                        .settings_edit(
                            core.identity.device_id.clone(),
                            device.clone(),
                            true,
                            format!("{}/{device}/clipboard", core.identity.device_id),
                            false,
                        )
                        .await?;
                }
                _ => {}
            }
            assert_eq!(
                core.automatic_target().is_some(),
                policy == "connected",
                "{policy}"
            );
            let echo = Arc::new(crate::ui::clipboard::EchoGuard::default());
            echo.remember(b"received text");
            let before: String = core
                .store
                .call(|c| Ok(c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?))
                .await?;
            crate::ui::clipboard::tests::deliver_fixture_offer(
                &core,
                &[("text/plain", b"received text")],
                echo,
                None,
                || {},
            )
            .await?;
            let after: String = core
                .store
                .call(|c| Ok(c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?))
                .await?;
            assert_eq!(before, after, "echo stamped with {policy}");
            assert!(core.automatic.lock().unwrap().selections.is_empty());
            // The image completed before this filtered local selection, but its
            // deferred effect may capture the revision only after reading ends.
            let older_image = crate::now_ms();
            core.store.observe_hlc(device.clone(), older_image).await?;
            crate::ui::clipboard::tests::deliver_offer(
                &core,
                &[
                    ("text/plain", b"secret"),
                    ("x-kde-passwordManagerHint", b"secret"),
                ],
            )
            .await?;
            assert!(
                !core
                    .store
                    .stamp_eligible(older_image, device.clone())
                    .await?,
                "deferred image overwrites filtered local content with {policy}"
            );
            assert!(core.automatic.lock().unwrap().selections.is_empty());
            core.shutdown_actions().await?;
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn automatic_formats_image_only_text_recipients_are_silent() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _connection = admit_ui_test_phone(&core, &device, 1)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&device)
            .unwrap()
            .capabilities = vec!["clipboard_text".into()];
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        let mut events = core.subscribe();
        let requested = crate::ui::clipboard::tests::deliver_offer(
            &core,
            &[("image/png", b"\x89PNG\r\n\x1a\nimage")],
        )
        .await?;
        assert_eq!(
            requested,
            ["image/png"],
            "read for echo classification even without image recipients"
        );
        assert!(core.take_automatic(&device, 1).is_none());
        assert!(
            core.send_observed_image(
                core.observe_clipboard_offer().unwrap(),
                b"\x89PNG\r\n\x1a\nimage".to_vec()
            )
            .await
            .is_err()
        );
        assert!(core.take_automatic(&device, 1).is_none());
        assert!(core.automatic.lock().unwrap().selections.is_empty());
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, Event::ClipboardSent { .. }),
                "unsupported offer reported as sent"
            );
        }
        core.shutdown_actions().await?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn automatic_formats_sensitive_policy_gates_every_representation() -> Result<()> {
        let (_dir, core, image_peer) = fixture(Arc::new(NoActions)).await?;
        let text_peer = Id::from_bytes([2; 16]);
        let _image = admit_ui_test_phone(&core, &image_peer, 1)?;
        let _text = admit_ui_test_phone(&core, &text_peer, 2)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&text_peer)
            .unwrap()
            .capabilities = vec!["clipboard_text".into()];
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        for marker in [
            "application/x-keepassxc-secret",
            crate::ui::clipboard::ORIGIN_MIME,
            "x-kde-passwordManagerHint",
        ] {
            let requested = crate::ui::clipboard::tests::deliver_offer(
                &core,
                &[
                    ("image/png", b"\x89PNG\r\n\x1a\nimage"),
                    ("text/plain", b"secret text"),
                    (marker, b" secret\0 "),
                ],
            )
            .await?;
            if marker == "x-kde-passwordManagerHint" {
                assert_eq!(
                    requested.len(),
                    3,
                    "marker and both formats should be requested"
                );
            } else {
                assert!(requested.is_empty(), "sensitive MIME gates every read");
            }
            assert!(core.take_automatic(&image_peer, 1).is_none());
            assert!(core.take_automatic(&text_peer, 2).is_none());
        }
        core.shutdown_actions().await?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn automatic_formats_image_policy_and_text_aliases_follow_frozen_offer() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _connection = admit_ui_test_phone(&core, &device, 1)?;
        // Explicitly disabling images still lets a dual-capability recipient get text.
        core.set_setting("clipboard_include_images".into(), false.into())
            .await?;
        for mime in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"] {
            let requested = crate::ui::clipboard::tests::deliver_offer(
                &core,
                &[("image/png", b"\x89PNG\r\n\x1a\nimage"), (mime, b"text")],
            )
            .await?;
            assert_eq!(requested, vec![mime, "image/png"]);
            let work = core
                .take_automatic(&device, 1)
                .expect("text with images disabled");
            assert!(matches!(work.selection, OutgoingSelection::Text(c) if c.text == "text"));
        }
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&device)
            .unwrap()
            .capabilities = vec!["files".into(), "future_feature".into()];
        assert!(
            core.observe_clipboard_offer().is_none(),
            "general file support cannot receive clipboard payloads"
        );
        assert_eq!(
            crate::ui::clipboard::tests::deliver_offer(&core, &[("text/plain", b"text")]).await?,
            ["text/plain"]
        );
        core.shutdown_actions().await?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn automatic_formats_resuming_or_connecting_cannot_expand_frozen_recipients() -> Result<()>
    {
        let (_dir, core, text_peer) = fixture(Arc::new(NoActions)).await?;
        let image_peer = Id::from_bytes([2; 16]);
        let late_peer = Id::from_bytes([3; 16]);
        let _text = admit_ui_test_phone(&core, &text_peer, 1)?;
        let _image = admit_ui_test_phone(&core, &image_peer, 2)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&text_peer)
            .unwrap()
            .capabilities = vec!["clipboard_text".into()];
        core.set_phone_paused(&image_peer, 2, true);
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        let mut late_connection = None;
        let requested = crate::ui::clipboard::tests::deliver_offer_after_observation(
            &core,
            &[
                ("image/png", b"\x89PNG\r\n\x1a\nimage"),
                ("text/plain", b"frozen text"),
            ],
            || {
                core.set_phone_paused(&image_peer, 2, false);
                late_connection = Some(admit_ui_test_phone(&core, &late_peer, 3).unwrap());
                // Even a capability gained on the original session cannot expand its plan.
                core.sessions
                    .lock()
                    .unwrap()
                    .sessions
                    .get_mut(&text_peer)
                    .unwrap()
                    .capabilities
                    .push("clipboard_image".into());
            },
        )
        .await?;
        assert_eq!(requested, vec!["text/plain", "image/png"]);
        let work = core
            .take_automatic(&text_peer, 1)
            .expect("original text recipient");
        assert!(matches!(work.selection, OutgoingSelection::Text(c) if c.text == "frozen text"));
        assert!(core.take_automatic(&image_peer, 2).is_none());
        assert!(core.take_automatic(&late_peer, 3).is_none());
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn automatic_formats_send_rechecks_only_remove_frozen_recipients() -> Result<()> {
        let (_dir, core, image_peer) = fixture(Arc::new(NoActions)).await?;
        let text_peer = Id::from_bytes([2; 16]);
        let _image = admit_ui_test_phone(&core, &image_peer, 1)?;
        let _text = admit_ui_test_phone(&core, &text_peer, 2)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&text_peer)
            .unwrap()
            .capabilities = vec!["clipboard_text".into()];
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        let offer = core.observe_clipboard_offer().unwrap();
        core.set_phone_paused(&image_peer, 1, true);
        core.send_observed_formats(
            offer,
            Some("remaining text".into()),
            Some(b"\x89PNG\r\n\x1a\nimage".to_vec()),
        )
        .await?;
        assert!(core.take_automatic(&image_peer, 1).is_none());
        let work = core
            .take_automatic(&text_peer, 2)
            .expect("still eligible text recipient");
        assert!(
            matches!(automatic_wire(&core, work).await?.as_slice(), [Message::Clipboard(c)] if c.text == "remaining text")
        );
        // Re-enabling a policy cannot authorize an offer observed before it changed.
        let stale = core.observe_clipboard_offer().unwrap();
        core.toggle_setting(crate::ui::Toggle::IncludeImages)
            .await?;
        core.toggle_setting(crate::ui::Toggle::IncludeImages)
            .await?;
        assert!(
            core.send_observed_formats(
                stale,
                Some("stale text".into()),
                Some(b"\x89PNG\r\n\x1a\nimage".to_vec())
            )
            .await
            .is_err()
        );
        assert!(core.take_automatic(&text_peer, 2).is_none());
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn explicit_clipboard_sends_supersede_older_reads_only_for_their_peer() -> Result<()> {
        for mesh in [false, true] {
            for image in [false, true] {
                let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
                core.set_setting("clipboard_include_images".into(), true.into())
                    .await?;
                let _link = admit_ui_test_phone(&core, &device, 1)?;
                core.sessions
                    .lock()
                    .unwrap()
                    .sessions
                    .get_mut(&device)
                    .unwrap()
                    .mesh = mesh;
                let other = Id::from_bytes([7; 16]);
                let _other_link = admit_ui_test_phone(&core, &other, 2)?;
                let old_read = core.observe_clipboard_offer().unwrap();
                let explicit = async {
                    if image {
                        core.send_clipboard_image(
                            device.clone(),
                            b"\x89PNG\r\n\x1a\nexplicit".to_vec(),
                        )
                        .await
                    } else {
                        core.send_clipboard(device.clone(), "explicit".into()).await
                    }
                };
                tokio::pin!(explicit);
                assert!(futures_util::poll!(&mut explicit).is_pending());
                // Simulate completion of a watcher read/debounce captured before
                // the manual send. It may still publish to an unaffected peer.
                core.send_observed_formats(
                    old_read,
                    Some("older watcher".into()),
                    Some(b"\x89PNG\r\n\x1a\nold".to_vec()),
                )
                .await?;
                let explicit = explicit.await?;
                let other_work = core
                    .take_automatic(&other, 2)
                    .expect("other peer retained offer");
                assert_ne!(other_work.selection.id(), &explicit);
                core.settings_ready.lock().unwrap().remove(&device);
                if mesh {
                    assert!(core.take_automatic(&device, 1).is_none());
                }
                core.settings_ready
                    .lock()
                    .unwrap()
                    .insert(device.clone(), 1);
                assert_eq!(
                    core.take_automatic(&device, 1).unwrap().selection.id(),
                    &explicit
                );
                // A newer watcher offer supersedes queued manual work for every peer.
                let fresh = core.observe_clipboard_offer().unwrap();
                let id = core
                    .send_observed_clipboard(fresh, "new watcher".into())
                    .await?;
                assert_eq!(core.take_automatic(&device, 1).unwrap().selection.id(), &id);
                assert!(core.store.first_outgoing(device.clone()).await?.is_none());
                core.shutdown_actions().await?;
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn explicit_clipboard_sends_cancel_queued_manual_and_automatic_writes() -> Result<()> {
        for mesh in [false, true] {
            for old_kind in 0..3 {
                for new_image in [false, true] {
                    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
                    core.set_setting("clipboard_include_images".into(), true.into())
                        .await?;
                    let _link = admit_ui_test_phone(&core, &device, 1)?;
                    core.sessions
                        .lock()
                        .unwrap()
                        .sessions
                        .get_mut(&device)
                        .unwrap()
                        .mesh = mesh;
                    let old = match old_kind {
                        0 => test_automatic_work(&core, &device, false).await?,
                        1 => {
                            core.send_clipboard_image(
                                device.clone(),
                                b"\x89PNG\r\n\x1a\nold manual".to_vec(),
                            )
                            .await?;
                            core.take_automatic(&device, 1).unwrap()
                        }
                        _ => {
                            core.send_clipboard(device.clone(), "old manual".into())
                                .await?;
                            core.take_automatic(&device, 1).unwrap()
                        }
                    };
                    let newest = if new_image {
                        core.send_clipboard_image(
                            device.clone(),
                            b"\x89PNG\r\n\x1a\nnew manual".to_vec(),
                        )
                        .await?
                    } else {
                        core.send_clipboard(device.clone(), "new manual".into())
                            .await?
                    };
                    let (_priority, mut priority_rx) = mpsc::channel(1);
                    let (_normal, normal_rx) = mpsc::channel(1);
                    let mut normal_rx = phone_controls::FrameQueue::from(normal_rx);
                    let mut bytes = Vec::new();
                    assert!(
                        !transmit_automatic(
                            &core,
                            &mut bytes,
                            old,
                            &mut priority_rx,
                            &mut normal_rx,
                            None,
                            None
                        )
                        .await?
                    );
                    assert!(
                        bytes.is_empty(),
                        "superseded work crossed first-byte boundary"
                    );
                    assert_eq!(
                        core.take_automatic(&device, 1).unwrap().selection.id(),
                        &newest
                    );
                    core.shutdown_actions().await?;
                }
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn legacy_pairing_hints_are_normalized_before_initial_persistence() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let invitation = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        let hello = Hello {
            setup: None,
            version: 1,
            device_id: device.clone(),
            name: "Legacy phone".into(),
            role: Some(DeviceRole::Computer),
            addresses: Some(vec!["192.0.2.99:47821".into()]),
            capabilities: vec!["clipboard_text".into()],
            paused: None,
        };
        core.finish_pairing_hello(
            &device,
            &Hash::new(crate::random_hex(32))?,
            &invitation.token,
            Some(hello.clone()),
        )
        .await
        .map_err(|e| anyhow::anyhow!("pairing rejected: {e:?}"))?;
        // No register/PAIR_RESULT is needed to establish the safe durable state.
        let peer = core
            .store
            .peer_info()
            .await?
            .into_iter()
            .find(|p| p.id == device)
            .unwrap();
        assert_eq!(peer.role, "phone");
        assert!(peer.addresses.is_empty());
        // Simulate historical poisoned metadata; a normal legacy HELLO update
        // clears it, and never installs the newly supplied hints.
        let id = device.clone();
        core.store
            .call(move |c| {
                c.execute(
                    "UPDATE peers SET role='computer',addresses='[\"192.0.2.1:47821\"]' WHERE id=?",
                    [id.as_str()],
                )?;
                Ok(())
            })
            .await?;
        core.store
            .remember_peer_hello(hello.clone(), Arc::new(AtomicU64::new(1)), 1, false)
            .await?;
        let peer = core
            .store
            .peer_info()
            .await?
            .into_iter()
            .find(|p| p.id == device)
            .unwrap();
        assert_eq!(peer.role, "phone");
        assert!(peer.addresses.is_empty());
        // The outbound role comes from the chosen route, never from HELLO.
        let mut outbound = hello;
        outbound.role = Some(DeviceRole::Phone);
        normalize_legacy_hello(&mut outbound, true);
        assert_eq!(outbound.role, Some(DeviceRole::Computer));
        assert!(outbound.addresses.is_none());
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn production_ui_policy_cleanup_discards_pending_clipboard() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _link = admit_ui_test_phone(&core, &device, 7)?;
        core.send_clipboard(device.clone(), "explicit".into())
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
            assert!(core.take_automatic(&device, 7).is_none());
            assert!(core.store.first_outgoing(device.clone()).await?.is_none());
        }
        core.shutdown_actions().await?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn clipboard_observation_ignores_paused_offers_during_slow_reads() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let (close, _receiver) = watch::channel(None);
        core.sessions.lock().unwrap().sessions.insert(
            device.clone(),
            Session {
                outbound: false,
                mesh: false,
                role: "phone".into(),
                device: device.clone(),
                id: 7,
                close,
                wake: Arc::new(Notify::new()),
                paused: false,
                capabilities: vec!["clipboard_text".into(), "clipboard_image".into()],
                phone: crate::ui::Phone::default(),
            },
        );
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
    // Drop releases every submitted closure, including on an assertion failure.
    async fn occupy_store(store: &Store) -> Vec<std::sync::mpsc::Sender<()>> {
        let mut releases = Vec::new();
        for _ in 0..crate::work::BLOCKING_LIMIT {
            let (release, blocked) = std::sync::mpsc::channel();
            let mut job = Box::pin(store.blocking(move |_| {
                let _ = blocked.recv();
                Ok(())
            }));
            assert!(futures_util::poll!(job.as_mut()).is_pending());
            drop(job);
            releases.push(release);
        }
        releases
    }
    #[tokio::test]
    async fn canceled_metadata_wait_after_connection_change_preserves_latest_snapshot() -> Result<()>
    {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _first = admit_ui_test_phone(&core, &device, 1)?;
        core.persist_phone(1).await?;
        let releases = occupy_store(&core.store).await;
        let _replacement = admit_ui_test_phone(&core, &device, 2)?;
        let mut persistence = Box::pin(core.persist_phone(2));
        let _ = futures_util::poll!(persistence.as_mut());
        drop(persistence);
        drop(releases);
        core.store.drain().await;
        let saved: crate::ui::Phone =
            serde_json::from_slice(&std::fs::read(core.paths.data.join("phone.json"))?)?;
        core.shutdown_actions().await?;
        assert_eq!(
            saved.name, "Phone 2",
            "connection mutation lost its metadata follow-up"
        );
        Ok(())
    }
    #[tokio::test]
    async fn canceled_unpair_waiting_for_capacity_keeps_pairing_usable() -> Result<()> {
        for remote in [false, true] {
            let (dir, core, _) = fixture(Arc::new(NoActions)).await?;
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?.to_string();
            let server = tokio::spawn(core.clone().serve(listener, false));
            let phone = crate::client::Phone::new(dir.path().join("phone"))?;
            let invitation = core.start_pairing_with_addresses(vec![address]).await?;
            let mut original = phone.pair(&invitation).await?;
            let ping = Message::Ping(Ping {
                nonce: Nonce::new("f".repeat(16))?,
            });
            original.write(&ping).await?;
            original.read(Duration::from_secs(2)).await?;
            settle_legacy_ready(&core, &mut original).await?;
            let device = phone.identity.device_id.clone();
            let scope = core.action_scope.lock().unwrap()[&device].clone();
            let connection = core.sessions.lock().unwrap().sessions[&device].id;
            // Hold every admission permit explicitly. Polling and dropping
            // blocking jobs can leave a free slot when unrelated setup work
            // already holds capacity; a pending future may then be post-admission.
            let mut releases = Vec::new();
            for _ in 0..crate::work::BLOCKING_LIMIT {
                releases.push(core.store.reserve().await);
            }
            assert_eq!(core.store.admitted_jobs(), crate::work::BLOCKING_LIMIT);
            let mut unpair = Box::pin(core.forget_pairing(
                device.clone(),
                CloseReason::Unpair,
                remote.then_some(connection),
                remote.then(|| "Phone".into()),
            ));
            assert!(futures_util::poll!(unpair.as_mut()).is_pending());
            drop(unpair);
            drop(releases);
            core.store.drain().await;
            let consistent = !core.sessions.lock().unwrap().unpairing.contains(&device)
                && !*scope.cancelled.borrow();
            let binding = core
                .store
                .peers()
                .await?
                .get(&phone.identity.fingerprint)
                .cloned();
            let reconnect = if consistent {
                Some(phone.reconnect().await)
            } else {
                None
            };
            core.request_shutdown();
            tokio::time::timeout(Duration::from_secs(3), server).await???;
            assert!(consistent, "cancelled admission stranded lifecycle state");
            assert_eq!(binding, Some(device));
            reconnect.context("reconnect skipped")??;
        }
        Ok(())
    }
    struct SaturatingText {
        store: Mutex<Option<Store>>,
        releases: Mutex<Vec<std::sync::mpsc::Sender<()>>>,
        value: Mutex<String>,
        writes: AtomicU64,
    }
    #[async_trait::async_trait]
    impl PcActions for SaturatingText {
        async fn clipboard_text(&self, text: &str) -> Result<()> {
            *self.value.lock().unwrap() = text.into();
            self.writes.fetch_add(1, Ordering::SeqCst);
            let store = self.store.lock().unwrap().take();
            if let Some(store) = store {
                *self.releases.lock().unwrap() = occupy_store(&store).await;
            }
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
    async fn disconnect_after_text_apply_with_saturated_capacity_records_and_deduplicates()
    -> Result<()> {
        let actions = Arc::new(SaturatingText {
            store: Mutex::new(None),
            releases: Mutex::new(Vec::new()),
            value: Mutex::new(String::new()),
            writes: AtomicU64::new(0),
        });
        let (dir, core, _) = fixture(actions.clone()).await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let server = tokio::spawn(core.clone().serve(listener, false));
        let phone = crate::client::Phone::new(dir.path().join("phone"))?;
        let invitation = core.start_pairing_with_addresses(vec![address]).await?;
        let mut original = phone.pair(&invitation).await?;
        original
            .write(&Message::Ping(Ping {
                nonce: Nonce::new("f".repeat(16))?,
            }))
            .await?;
        original.read(Duration::from_secs(2)).await?;
        let device = phone.identity.device_id.clone();
        let scope = core.action_scope.lock().unwrap()[&device].clone();
        let connection = core.sessions.lock().unwrap().sessions[&device].id;
        let mut store = core.store.with_current_pairing();
        store.connection_id = Some(connection);
        let text = phone.text("already applied".into(), ClipboardOrigin::Clipboard)?;
        *actions.store.lock().unwrap() = Some(core.store.clone());
        let (tx, _rx) = mpsc::channel(4);
        let mut incoming = None;
        let mut receive = Box::pin(core.receive(
            &store,
            Some(&scope),
            &device,
            Message::Clipboard(text.clone()),
            &mut incoming,
            &tx,
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let _ = futures_util::poll!(receive.as_mut());
                if actions.writes.load(Ordering::SeqCst) != 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        // Model the READY worker abort immediately after the successful OS write.
        drop(receive);
        core.finish_connection(&device, connection);
        actions.releases.lock().unwrap().clear();
        core.store.drain().await;
        let recorded = core.store.receipt(device, text.transfer_id.clone()).await?;
        *actions.value.lock().unwrap() = "newer local clipboard".into();
        let mut replacement = phone.reconnect().await?;
        let replay = replacement.send_text(text).await?;
        core.request_shutdown();
        tokio::time::timeout(Duration::from_secs(3), server).await???;
        assert!(
            recorded.is_some_and(|(_, complete)| complete),
            "applied text lost its durable history"
        );
        assert!(replay.deduplicated);
        assert_eq!(actions.writes.load(Ordering::SeqCst), 1);
        assert_eq!(*actions.value.lock().unwrap(), "newer local clipboard");
        Ok(())
    }
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
            let dir = crate::test_support::scratch()?;
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
        // Plant crash orphans while the stage is live: empty staging roots are retired.
        std::fs::write(history.join(".pneu-staging/orphan"), b"crashed")?;
        drop(stage);
        core.store.drain().await;
        let healthy = core.config().save_path;
        let stage = core
            .store
            .create_stage(healthy.clone(), "temporary".into(), 1)
            .await?;
        std::fs::write(healthy.join(".pneu-staging/orphan"), b"crashed")?;
        drop(stage);
        core.store.drain().await;
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
            first_chunk: true,
            hash: Sha256::new(),
            file: None,
            stage: None,
            destination: None,
            receipt: None,
            save_error: None,
            storage_kind: crate::ui::ReceiveDestination::Cache,
            deduplicated: false,
            admission: FileAdmission::New,
            config: core.config(),
        };
        core.prepare_incoming(&core.store, &core.identity.device_id, &mut state)
            .await;
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
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn disk_full_on_flush_nacks_and_retains_cache_without_receipt() -> Result<()> {
        let dir = crate::test_support::scratch()?;
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
            resume: None,
            folder: None,
            sensitive: false,
            hlc: None,
            source_device: None,
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
        // Resolve while linked: cleanup also retires the empty staging root.
        let stage_path = state.stage.as_ref().unwrap().path()?;
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
        assert!(!stage_path.exists());
        assert!(
            !core
                .config()
                .save_path
                .join(crate::storage::STAGING_DIR)
                .exists()
        );
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
    include!("phone_control_tests.rs");
    #[tokio::test]
    async fn clipboard_nack_diagnostic_identifies_delivery_without_content() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let mut peer = core.computer_hello();
        peer.device_id = device.clone();
        peer.name = "Fixture phone".into();
        let clip_text = "PRIVATE clipboard fixture must never be logged";
        let content = crate::ui::clipboard::Content::Text(clip_text.into());
        let id = Id::from_bytes([42; 16]);
        let Message::Nack(nack) = nack(id.clone(), NackCode::StorageFailed) else {
            unreachable!();
        };
        let mut events = core.subscribe();
        for kind in ["text", "image"] {
            core.clipboard_rejected(&peer, &id, kind, &nack);
            let Event::Error {
                device: logged_peer,
                message,
            } = events.try_recv()?
            else {
                panic!("expected diagnostic event");
            };
            assert_eq!(logged_peer.as_ref(), Some(&device));
            let line = crate::diagnostics::error_line(logged_peer.as_ref(), &message);
            for field in [
                format!("peer={device}"),
                format!("transfer={id}"),
                format!("kind={kind}"),
                "peer_name=\"Fixture phone\"".into(),
                "phase=receipt".into(),
                "code=StorageFailed".into(),
                "retryable=true".into(),
                "action=dropped".into(),
            ] {
                assert!(line.contains(&field), "missing {field}: {line}");
            }
            let crate::ui::clipboard::Content::Text(private) = &content else {
                unreachable!()
            };
            assert!(!line.contains(private));
            #[cfg(target_os = "macos")]
            assert!(
                chrono::DateTime::parse_from_rfc3339(line.split_whitespace().nth(1).unwrap())
                    .is_ok()
            );
            #[cfg(target_os = "linux")]
            assert!(line.starts_with("pneud: peer="));
        }
        core.shutdown_actions().await?;
        Ok(())
    }
    async fn fixture(actions: Arc<dyn PcActions>) -> Result<(tempfile::TempDir, Arc<Core>, Id)> {
        let dir = crate::test_support::scratch()?;
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
        core.action_scope
            .lock()
            .unwrap()
            .insert(device.clone(), ActionScope::new());
        Ok((dir, core, device))
    }
    // PONG is handled by the reader before READY initialization finishes.
    // STATUS reaches the control loop; draining afterwards includes its evidence
    // write rather than racing that job's admission after ConnectionChanged.
    async fn settle_legacy_ready(
        core: &Core,
        connection: &mut crate::client::PhoneConnection,
    ) -> Result<()> {
        let mut events = core.subscribe();
        for paused in [true, false] {
            connection
                .write(&Message::Status(Status { paused }))
                .await?;
            tokio::time::timeout(Duration::from_secs(2), async {
                while !matches!(events.recv().await?, Event::PhonePaused { paused: value, .. } if value == paused) {}
                Ok::<_, anyhow::Error>(())
            }).await??;
        }
        core.store.drain().await;
        Ok(())
    }
    #[tokio::test]
    async fn blocked_notice_uses_canonical_peer_addresses() -> Result<()> {
        let (dir, core, _) = fixture(Arc::new(NoActions)).await?;
        let phone = crate::client::Phone::new(dir.path().join("unpaired-phone"))?;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(crate::tls::client_config(
            &phone.identity,
            core.identity.fingerprint.clone(),
        )?));
        let mut events = core.subscribe();
        for (bind, host) in [
            ("::", "127.0.0.1"),
            ("127.0.0.1", "127.0.0.1"),
            ("::1", "::1"),
        ] {
            let listener = if bind == "::" {
                crate::net::listen(0).await?
            } else {
                TcpListener::bind((bind, 0)).await?
            };
            let port = listener.local_addr()?.port();
            let (client, accepted) =
                tokio::try_join!(TcpStream::connect((host, port)), listener.accept())?;
            let permit = core.ingress.clone().acquire_owned().await?;
            let server = core.clone().connection(accepted.0, permit);
            let client = connector.connect(
                rustls::pki_types::ServerName::try_from("pneu.local")?,
                client,
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                let (server, client) = tokio::join!(server, async {
                    let mut tls: TlsStream<TcpStream> = client.await?.into();
                    assert!(matches!(
                        read_handshake(&mut tls).await?,
                        Message::Error(ErrorMessage {
                            code: ErrorCode::Unauthorized,
                            ..
                        })
                    ));
                    Ok::<_, anyhow::Error>(())
                });
                server?;
                client?;
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            let Event::Blocked { address, .. } =
                tokio::time::timeout(Duration::from_secs(2), events.recv()).await??
            else {
                panic!("expected blocked event");
            };
            assert_eq!(address, host);
            assert_eq!(
                crate::ui::notifications::Notice::blocked(&address).body,
                format!("{host} tried to connect")
            );
        }
        core.shutdown_actions().await?;
        Ok(())
    }

    fn test_begin(bytes: &[u8]) -> FileBegin {
        FileBegin {
            resume: None,
            folder: None,
            sensitive: false,
            hlc: None,
            source_device: None,
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
    #[cfg(target_os = "linux")]
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
                        sensitive: false,
                        hlc: None,
                        source_device: None,
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
        assert!(
            !text_still_blocked,
            "actor must time out the blocked helper independently"
        );
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
        let begin = test_begin(b"file");
        let text = Clipboard {
            sensitive: false,
            hlc: None,
            source_device: None,
            transfer_id: begin.transfer_id.clone(),
            text: "must not apply".into(),
            created_at: 1,
            origin: ClipboardOrigin::Clipboard,
        };
        let held = core.clone();
        let peer = device.clone();
        let (tx, mut rx) = mpsc::channel(8);
        let text_tx = tx.clone();
        let waiting = async move {
            held.receive_test(
                &held.store,
                &peer,
                Message::Clipboard(text),
                &mut None,
                &text_tx,
            )
            .await
        };
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
        waiting.await?;
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
            .get(&device)
            .unwrap()
            .clone();
        scope.connection_generation.store(1, Ordering::SeqCst);
        let (close, _closed) = watch::channel(None);
        core.sessions.lock().unwrap().sessions.insert(
            device.clone(),
            Session {
                outbound: false,
                mesh: false,
                role: "phone".into(),
                device: device.clone(),
                id: 1,
                close,
                wake: Arc::new(Notify::new()),
                paused: false,
                capabilities: vec!["clipboard_text".into(), "clipboard_image".into()],
                phone: crate::ui::Phone::default(),
            },
        );
        let mut store = core.store.with_current_pairing();
        store.connection_id = Some(1);
        let text = Clipboard {
            sensitive: false,
            hlc: None,
            source_device: None,
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
            while core.store.admitted_jobs() < 2 {
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
        settle_legacy_ready(&core, &mut original).await?;
        let device = phone.identity.device_id.clone();
        let scope = core
            .action_scope
            .lock()
            .unwrap()
            .get(&device)
            .unwrap()
            .clone();
        let mut old_store = core.store.with_current_pairing();
        old_store.connection_id = Some(
            core.sessions
                .lock()
                .unwrap()
                .sessions
                .get(&device)
                .unwrap()
                .id,
        );
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
            while core.store.admitted_jobs() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let mut replacement = phone.reconnect().await?;
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
        .await
        .context("replacement registration blocked by READY evidence persistence")??;
        replacement
            .write(&Message::Status(Status { paused: true }))
            .await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !matches!(
                events.recv().await?,
                Event::PhonePaused { paused: true, .. }
            ) {}
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("replacement STATUS blocked by READY evidence persistence")??;
        let gate = tokio::time::timeout(Duration::from_secs(2), core.pairing_gate.lock())
            .await
            .context("READY evidence persistence held the pairing gate")?;
        drop(gate);
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
            .get(&device)
            .unwrap()
            .clone();
        let (close, _closed) = watch::channel(None);
        core.sessions.lock().unwrap().sessions.insert(
            device.clone(),
            Session {
                outbound: false,
                mesh: false,
                role: "phone".into(),
                device: device.clone(),
                id: 1,
                close,
                wake: Arc::new(Notify::new()),
                paused: false,
                capabilities: vec!["clipboard_text".into(), "clipboard_image".into()],
                phone: crate::ui::Phone::default(),
            },
        );
        scope.connection_generation.store(1, Ordering::SeqCst);
        let job = ActionJob {
            file: ReceivedFile::from_file(dir.path().join("image.png"), tempfile::tempfile()?)?,
            scope: scope.clone(),
            origin: ActionOrigin {
                mesh: false,
                device: device.clone(),
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
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&device)
            .unwrap()
            .id = 2;
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
                mesh: false,
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
            .get(&device)
            .unwrap()
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
                        sensitive: false,
                        hlc: None,
                        source_device: None,
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
        // The window captured displayed before it was unpaired elsewhere.
        core.unpair_device(displayed.clone()).await?;
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
            .get(&replacement)
            .unwrap()
            .clone();
        let mut events = core.subscribe();
        core.unpair_device(displayed).await?;
        assert_eq!(core.paired_devices(), vec![replacement.clone()]);
        assert_eq!(
            core.store.peers().await?,
            core.admission.lock().unwrap().peers
        );
        assert!(
            !core
                .sessions
                .lock()
                .unwrap()
                .unpairing
                .contains(&replacement)
        );
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
        core.action_scope
            .lock()
            .unwrap()
            .insert(device.clone(), admitted.clone());
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
            .get(&device)
            .unwrap()
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
    async fn concurrent_clipboard_ipc_reports_supersession_and_keeps_commit_order() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _link = admit_ui_test_phone(&core, &device, 1)?;
        let mut requests = JoinSet::new();
        for n in 0..32 {
            let core = core.clone();
            let device = device.clone();
            requests.spawn(async move { core.send_clipboard(device, format!("text {n}")).await });
        }
        let mut ids = Vec::new();
        let mut cancelled = 0;
        while let Some(result) = requests.join_next().await {
            match result? {
                Ok(id) => ids.push(id),
                Err(error) => {
                    assert_eq!(error.to_string(), "clipboard operation cancelled");
                    cancelled += 1;
                }
            }
        }
        assert!(!ids.is_empty(), "at least the final request must publish");
        assert_eq!(ids.len() + cancelled, 32);
        assert_eq!(
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            ids.len()
        );
        let wire = automatic_wire(&core, core.take_automatic(&device, 1).unwrap()).await?;
        let Message::Clipboard(latest) = &wire[0] else {
            panic!("expected clipboard");
        };
        let (register, _, _) = core.store.clipboard_snapshot().await?;
        assert_eq!(latest.created_at, register.unwrap().hlc);
        assert!(latest.hlc.is_none());
        assert!(ids.contains(&latest.transfer_id));
        assert!(core.store.first_outgoing(device).await?.is_none());
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
            settle_legacy_ready(&core, &mut original).await?;
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
                .get(&device)
                .unwrap()
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
            while !core.sessions.lock().unwrap().unpairing.contains(&device)
                || core.store.active_jobs() < 2
            {
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
            assert!(core.ui.lock().unwrap().receiving_until.is_empty());
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
            assert!(!core.sessions.lock().unwrap().unpairing.contains(&device));
            assert!(core.sessions.lock().unwrap().sessions.is_empty());
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
                .get(&device)
                .unwrap()
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
            core.action_scope
                .lock()
                .unwrap()
                .insert(device.clone(), scope.clone());
            let (close, closed) = watch::channel(None);
            core.sessions.lock().unwrap().sessions.insert(
                device.clone(),
                Session {
                    outbound: false,
                    mesh: false,
                    role: "phone".into(),
                    device: device.clone(),
                    id: 1,
                    close,
                    wake: Arc::new(Notify::new()),
                    paused: false,
                    capabilities: vec!["clipboard_text".into(), "clipboard_image".into()],
                    phone: crate::ui::Phone::default(),
                },
            );
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
            assert!(!core.sessions.lock().unwrap().unpairing.contains(&device));
            assert_eq!(
                core.sessions
                    .lock()
                    .unwrap()
                    .sessions
                    .get(&device)
                    .unwrap()
                    .id,
                1
            );
            assert!(closed.borrow().is_none());
            assert!(!*scope.cancelled.borrow());
            assert!(Arc::ptr_eq(
                &scope.active,
                &core
                    .action_scope
                    .lock()
                    .unwrap()
                    .get(&device)
                    .unwrap()
                    .active
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
    async fn inviter_restart_after_binding_commit_invalidates_unconsumed_memory_token() -> Result<()>
    {
        let (_dir, core, _device) = fixture(Arc::new(NoActions)).await?;
        let invitation = core
            .start_pairing_with_addresses(vec!["127.0.0.1:47821".into()])
            .await?;
        let device = Id::new(crate::random_hex(16))?;
        let fp = Hash::new(crate::random_hex(32))?;
        // Model process death after SQLite commit, before Core's infallible
        // callback consumes the memory-only invitation and publishes admission.
        core.store
            .pair_then(device.clone(), fp.clone(), || {})
            .await?;
        assert!(core.admission.lock().unwrap().invitation.is_some());
        assert!(!core.paired_devices().contains(&device));
        let paths = core.paths.clone();
        let config = core.config();
        core.shutdown_actions().await?;
        core.store.drain().await;
        drop(core);
        let core = Core::new(paths, config, Arc::new(NoActions)).await?;
        assert_eq!(core.store.peers().await?.get(&fp), Some(&device));
        assert!(core.paired_devices().contains(&device));
        assert!(core.admission.lock().unwrap().invitation.is_none());
        let other = Id::new(crate::random_hex(16))?;
        let other_fp = Hash::new(crate::random_hex(32))?;
        assert!(matches!(
            core.finish_pairing(&other, &other_fp, &invitation.token)
                .await,
            Err(PairCode::PairingClosed)
        ));
        assert_eq!(core.paired_devices().len(), 2); // fixture binding plus the committed peer
        core.shutdown_actions().await?;
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
        let dir = crate::test_support::scratch()?;
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
    async fn final_shutdown_waits_for_cancelled_callers_store_save() -> Result<()> {
        let (_dir, core, _) = fixture(Arc::new(NoActions)).await?;
        // Remove unrelated action waits so Pending below can only be Store work.
        core.shutdown_actions().await?;
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocking) = std::sync::mpsc::channel();
        let path = core.paths.config.clone();
        let mut config = core.config();
        config.name = "Saved before shutdown".into();
        let store = core.store.clone();
        let caller = tokio::spawn(async move {
            store
                .blocking(move |_| {
                    let _ = entered.send(());
                    blocking.recv()?;
                    config.save(&path)
                })
                .await
        });
        wait.await?;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        core.request_shutdown();
        let mut shutdown = Box::pin(core.finish_shutdown());
        assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
        release.send(())?;
        shutdown.await?;
        assert_eq!(core.store.active_jobs(), 0);
        assert_eq!(
            Config::load(&core.paths.config)?.name,
            "Saved before shutdown"
        );
        Ok(())
    }
    #[test]
    fn canceled_phone_cache_writes_keep_their_blocking_capacity() -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let (entered, wait) = tokio::sync::oneshot::channel();
            let (release, blocking) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = entered.send(());
                let _ = blocking.recv();
            });
            wait.await?;
            let mut maximum = 0;
            for id in 1..=200 {
                let _connection = admit_ui_test_phone(&core, &device, id)?;
                // Poll once, then cancel exactly as a replaced connection does.
                let mut write = Box::pin(core.persist_phone(id));
                assert!(futures_util::poll!(write.as_mut()).is_pending());
                drop(write);
                maximum = maximum.max(core.store.active_jobs());
            }
            release.send(())?;
            blocker.await?;
            core.persist_phone(200).await?;
            core.store.drain().await;
            let phones: HashMap<Id, crate::ui::Phone> =
                serde_json::from_slice(&std::fs::read(core.paths.data.join("phones.json"))?)?;
            core.shutdown_actions().await?;
            assert!(maximum <= crate::work::BLOCKING_LIMIT);
            assert_eq!(phones[&device].name, "Phone 200");
            Ok(())
        })
    }
    #[test]
    fn reconnect_storm_bounds_store_jobs_and_preserves_other_device_progress() -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let (dir, core, other) = fixture(Arc::new(NoActions)).await?;
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let server = tokio::spawn(core.clone().serve(listener, false));
            let phone = crate::client::Phone::new(dir.path().join("phone"))?;
            let invitation = core.start_pairing_with_addresses(vec![address.to_string()]).await?;
            let mut initial = phone.pair(&invitation).await?;
            let ping = Ping { nonce: Nonce::new("d".repeat(16))? };
            initial.write(&Message::Ping(ping.clone())).await?;
            assert_eq!(initial.read(Duration::from_secs(2)).await?, Message::Pong(ping));
            settle_legacy_ready(&core, &mut initial).await?;
            let connector = tokio_rustls::TlsConnector::from(Arc::new(crate::tls::client_config(
                &phone.identity, core.identity.fingerprint.clone(),
            )?));
            let (entered, wait) = tokio::sync::oneshot::channel();
            let (release, blocking) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = entered.send(());
                let _ = blocking.recv();
            });
            wait.await?;
            let mut release = Some(release);
            let mut operation = None;
            let mut progress = None;
            let mut latest = None;
            let mut maximum = 0;
            let mut maximum_admitted = 0;
            let outcome: Result<()> = async {
                for index in 0..240 {
                    if index == 100 {
                        let store = core.store.clone();
                        let device = other.clone();
                        operation = Some(tokio::spawn(async move {
                            store.enqueue_clipboard(device, "another device".into()).await
                        }));
                    }
                    if index == 200 {
                        // Keep the producer running after release while an
                        // independent timer checks the other device's progress.
                        release.take().unwrap().send(())?;
                        let mut operation = operation.take().unwrap();
                        progress = Some(tokio::spawn(async move {
                            let result = tokio::time::timeout(Duration::from_secs(2), &mut operation).await;
                            if result.is_err() {
                                operation.abort();
                                let _ = operation.await;
                            }
                            result
                        }));
                    }
                    let mut events = core.subscribe();
                    let stream = TcpStream::connect(address).await?;
                    let mut tls = connector.connect(
                        rustls::pki_types::ServerName::try_from("pneu.local")?, stream,
                    ).await?;
                    write_frame(&mut tls, &Message::Hello(Hello {
                        setup: None,
                        role: Some(DeviceRole::Computer),
                        addresses: Some(vec![format!("127.0.0.1:{}", 10000 + index)]),
                        version: 1,
                        device_id: phone.identity.device_id.clone(),
                        name: format!("Reconnect {index}"),
                        capabilities: vec!["clipboard_text".into()],
                        paused: Some(false),
                    })).await?;
                    ensure!(matches!(read_frame(&mut tls).await?, Some(Message::Hello(_))), "HELLO");
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while !matches!(events.recv().await?, Event::ConnectionChanged { device, connected: true } if device == phone.identity.device_id) {}
                        Ok::<_, anyhow::Error>(())
                    }).await??;
                    // Let the connection submit its metadata before sampling.
                    tokio::task::yield_now().await;
                    if index < 200 {
                        maximum = maximum.max(core.store.active_jobs());
                        maximum_admitted = maximum_admitted.max(core.store.admitted_jobs());
                        assert!(core.phone_writes.lock().unwrap().pending.len() <= 1,
                            "reconnect metadata did not coalesce per peer");
                    }
                    latest = Some(tls);
                }
                Ok(())
            }.await;
            if let Some(release) = release {
                let _ = release.send(());
            }
            blocker.await?;
            if let Some(operation) = operation {
                operation.await??;
            }
            let progress = match progress {
                Some(progress) => Some(progress.await?),
                None => None,
            };
            let ready = tokio::time::timeout(Duration::from_secs(2), async {
                let latest = latest.as_mut().context("latest connection")?;
                let ping = Ping { nonce: Nonce::new("e".repeat(16))? };
                write_frame(latest, &Message::Ping(ping.clone())).await?;
                ensure!(read_frame(latest).await? == Some(Message::Pong(ping)), "latest READY");
                Ok(())
            }).await;
            core.store.drain().await;
            let saved: Result<(String, String)> = core.store.call(move |c| {
                Ok(c.query_row("SELECT name,addresses FROM peers WHERE id=?", [phone.identity.device_id.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))?)
            }).await;
            let cache: Result<crate::ui::Phone> = (|| {
                Ok(serde_json::from_slice(&std::fs::read(core.paths.data.join("phone.json"))?)?)
            })();
            core.request_shutdown();
            let stopped = tokio::time::timeout(Duration::from_secs(5), server).await;
            drop((initial, latest));
            outcome?;
            let progress = progress.context("other device progress")?;
            let operation = progress.context("other device stalled")?;
            operation??;
            ready??;
            stopped???;
            assert!(maximum_admitted <= crate::work::BLOCKING_LIMIT,
                "reconnects retained {maximum_admitted} normal admissions");
            // One coalesced, uncancelable metadata continuation can coexist
            // with the 16 normal admissions while the blocking pool is stalled.
            assert!(maximum <= crate::work::BLOCKING_LIMIT + 1,
                "reconnects queued {maximum} blocking jobs");
            // This fixture does not negotiate mesh: its optional routing/role
            // fields must not become dial authority or learned hints.
            assert_eq!(saved?, ("Reconnect 239".into(), "[]".into()));
            assert_eq!(cache?.name, "Reconnect 239");
            let final_cache: crate::ui::Phone = serde_json::from_slice(&std::fs::read(core.paths.data.join("phone.json"))?)?;
            assert_eq!(final_cache.last_seen, core.ui_snapshot().phone.last_seen);
            Ok(())
        })
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
        // ConnectionChanged precedes READY initialization and its legacy
        // setup evidence write. Synchronize with the actual control loop before
        // blocking the store; drain alone can run before that job is admitted.
        connection
            .write(&Message::Status(Status { paused: true }))
            .await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !matches!(
                events.recv().await?,
                Event::PhonePaused { paused: true, .. }
            ) {}
            Ok::<_, anyhow::Error>(())
        })
        .await??;
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
        settle_legacy_ready(&core, &mut original).await?;
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
        let unpair_device = device.clone();
        let unpair = tokio::spawn(async move { unpair_core.unpair_device(unpair_device).await });
        while !core.sessions.lock().unwrap().unpairing.contains(&device) {
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
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&device)
            .unwrap()
            .id = 2;
        drop(guard);
        assert!(!task.await??);
        assert!(events.try_recv().is_err());
        assert_eq!(core.phone_paused(&device), Some(false));
        assert!(!core.store.peers().await?.is_empty());
        assert!(!core.sessions.lock().unwrap().unpairing.contains(&device));
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
            while !core.sessions.lock().unwrap().unpairing.contains(&device)
                || core.store.active_jobs() < 2
            {
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
            .retain(|_, id| *id != device);
        core.admission
            .lock()
            .unwrap()
            .peers
            .insert(Hash::new(crate::random_hex(32))?, device.clone());
        core.sessions.lock().unwrap().sessions.insert(
            device.clone(),
            Session {
                outbound: false,
                mesh: false,
                role: "phone".into(),
                device: device.clone(),
                id: 2,
                close,
                wake: Arc::new(Notify::new()),
                paused: false,
                capabilities: vec!["clipboard_text".into(), "clipboard_image".into()],
                phone: crate::ui::Phone::default(),
            },
        );
        let mut events = core.subscribe();
        core.unpair_device(Id::new(crate::random_hex(16))?).await?;
        assert!(!core.sessions.lock().unwrap().unpairing.contains(&device));
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
        core.action_scope
            .lock()
            .unwrap()
            .insert(device.clone(), scope.clone());
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
    #[tokio::test]
    async fn legacy_incoming_clock_and_ids_are_independent_between_peers() -> Result<()> {
        let actions = Arc::new(CountingActions(AtomicU64::new(0)));
        let (_dir, core, a) = fixture(actions.clone()).await?;
        let b = Id::from_bytes([2; 16]);
        core.store
            .pair(b.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        core.action_scope
            .lock()
            .unwrap()
            .insert(b.clone(), ActionScope::new());
        let shared = Id::from_bytes([3; 16]);
        let (tx, mut rx) = mpsc::channel(8);
        for (device, timestamp, value) in [(&a, 100, "from A"), (&b, 1, "from B")] {
            core.receive_test(
                &core.store,
                device,
                Message::Clipboard(Clipboard {
                    sensitive: false,
                    hlc: None,
                    source_device: None,
                    transfer_id: shared.clone(),
                    text: value.into(),
                    created_at: timestamp,
                    origin: ClipboardOrigin::Clipboard,
                }),
                &mut None,
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
        }
        assert_eq!(actions.0.load(Ordering::SeqCst), 2);
        core.receive_test(
            &core.store,
            &a,
            Message::Clipboard(Clipboard {
                sensitive: false,
                hlc: None,
                source_device: None,
                transfer_id: Id::from_bytes([4; 16]),
                text: "stale A".into(),
                created_at: 99,
                origin: ClipboardOrigin::Clipboard,
            }),
            &mut None,
            &tx,
        )
        .await?;
        assert!(matches!(
            rx.recv().await,
            Some(Message::Ack(Ack {
                deduplicated: true,
                ..
            }))
        ));
        assert_eq!(actions.0.load(Ordering::SeqCst), 2);
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn ready_receive_idle_deadline_belongs_only_to_acceptor() -> Result<()> {
        let (mut dialer_writer, mut dialer_reader) = tokio::io::duplex(1024);
        let (_acceptor_writer, mut acceptor_reader) = tokio::io::duplex(1024);
        let dialer = tokio::spawn(async move { read_ready_frame(&mut dialer_reader, true).await });
        let acceptor =
            tokio::spawn(async move { read_ready_frame(&mut acceptor_reader, false).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(181)).await;
        assert!(acceptor.await?.is_err());
        assert!(
            !dialer.is_finished(),
            "transmitting dialer relies on PONG/receipt deadlines"
        );
        let ping = Message::Ping(Ping {
            nonce: Nonce::new(crate::random_hex(8))?,
        });
        write_frame(&mut dialer_writer, &ping).await?;
        assert_eq!(dialer.await???, Some(ping));
        Ok(())
    }
    async fn file_test_peer(core: &Core, device: &Id) -> Result<crate::outbox::Authority> {
        let peer = core
            .store
            .peer_info()
            .await?
            .into_iter()
            .find(|p| &p.id == device)
            .unwrap();
        Ok(crate::outbox::Authority {
            device: device.clone(),
            generation: peer.generation,
            fingerprint: peer.fingerprint,
        })
    }

    #[tokio::test]
    async fn replacement_retry_image_sends_and_applies_after_newer_clipboard_activity() -> Result<()>
    {
        let (dir, sender, _) = fixture(Arc::new(NoActions)).await?;
        let actions = Arc::new(CountingActions(AtomicU64::new(0)));
        let (_received_dir, receiver, _) = fixture(actions.clone()).await?;
        let old = Id::from_bytes([81; 16]);
        let pin = Hash::new("81".repeat(32))?;
        let pair = |device: Id, fingerprint: Hash, offline| {
            sender.store.pair_then_hello_replacement(
                device.clone(),
                fingerprint,
                Some(Hello {
                    version: 1,
                    device_id: device,
                    name: "Replacement PC".into(),
                    capabilities: vec!["mesh".into()],
                    role: Some(DeviceRole::Computer),
                    addresses: None,
                    setup: None,
                    paused: None,
                }),
                None,
                SetupScope::Disabled,
                false,
                offline,
                || {},
            )
        };
        pair(old.clone(), pin.clone(), vec![]).await?;
        let operation = Id::from_bytes([82; 16]);
        let source = dir.path().join("retained.png");
        let immutable = b"\x89PNG\r\n\x1a\noriginal image";
        std::fs::write(&source, immutable)?;
        let (artifact, _) = sender
            .outbox
            .admit_selected(
                operation.clone(),
                source.clone(),
                vec![file_test_peer(&sender, &old).await?],
                crate::outbox::PEER_BYTES,
                sender.identity.device_id.clone(),
            )
            .await?;
        pair(
            receiver.identity.device_id.clone(),
            receiver.identity.fingerprint.clone(),
            vec![(old.clone(), 42)],
        )
        .await?;
        let incoming = test_begin(b"previous receive");
        sender.event(Event::TransferStarted {
            device: old.clone(),
            metadata: incoming.clone(),
        });
        sender.event(Event::TransferFinished {
            device: old.clone(),
            transfer_id: incoming.transfer_id.clone(),
            path: Some(dir.path().join("received.jpg")),
            deduplicated: false,
        });
        let offer = sender.store.replacements.lock().unwrap()[0].clone();
        sender
            .store
            .replacement_decide(sender.identity.device_id.clone(), offer, true, |_| {})
            .await?;
        assert!(
            sender
                .ui_snapshot()
                .transfers
                .iter()
                .any(|r| r.direction == "incoming"
                    && r.device == receiver.identity.device_id
                    && r.transfer_id == incoming.transfer_id
                    && r.state == "sent")
        );
        sender.outbox.reconcile().await?;
        // The mutable source is no longer the content being sent.
        std::fs::write(&source, b"changed original source")?;
        let peer = sender.identity.device_id.clone();
        receiver
            .store
            .pair(peer.clone(), sender.identity.fingerprint.clone())
            .await?;
        receiver
            .action_scope
            .lock()
            .unwrap()
            .insert(peer.clone(), ActionScope::new());
        let mut config = receiver.config();
        config.notification_preview = false;
        config.open_in_viewer = false;
        config.copy_image_to_clipboard = true;
        receiver.update_config(config).await?;
        let (created_at, hlc) = sender.store.local_clipboard(peer.clone()).await?;
        let (tx, mut rx) = mpsc::channel(2);
        receiver
            .receive_test(
                &receiver.store,
                &peer,
                Message::Clipboard(Clipboard {
                    sensitive: false,
                    hlc: Some(hlc),
                    source_device: Some(peer.clone()),
                    transfer_id: Id::from_bytes([83; 16]),
                    text: "newer clipboard".into(),
                    created_at,
                    origin: ClipboardOrigin::Clipboard,
                }),
                &mut None,
                &tx,
            )
            .await?;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), rx.recv()).await?,
            Some(Message::Ack(_))
        ));
        assert_eq!(actions.0.load(Ordering::SeqCst), 1);
        let target = file_test_peer(&sender, &receiver.identity.device_id).await?;
        sender
            .outbox
            .retry_replaced(
                operation.clone(),
                Some(artifact.clone()),
                old,
                vec![target.clone()],
                peer.clone(),
            )
            .await?;
        let delivery = sender
            .outbox
            .claim(target.device, target.generation, 1)
            .await?
            .context("retry send")?;
        assert_ne!(delivery.begin.transfer_id, artifact);
        assert!(delivery.begin.hlc.context("retry HLC")? > hlc);
        assert!(
            sender
                .outbox
                .snapshot(&[])
                .iter()
                .any(|r| r.transfer_id == delivery.begin.transfer_id && r.operation != operation)
        );
        let mut file = sender.outbox.reader(delivery.clone()).await?;
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut file, &mut bytes).await?;
        assert_eq!(bytes, immutable);
        let mut incoming = None;
        for message in [
            Message::FileBegin(delivery.begin.clone()),
            Message::FileChunk(FileChunk {
                transfer_id: delivery.begin.transfer_id.clone(),
                offset: 0,
                data: bytes,
            }),
            Message::FileEnd(FileEnd {
                transfer_id: delivery.begin.transfer_id.clone(),
            }),
        ] {
            receiver
                .receive_test(&receiver.store, &peer, message, &mut incoming, &tx)
                .await?;
        }
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), rx.recv()).await?,
            Some(Message::Ack(Ack {
                deduplicated: false,
                ..
            }))
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (register, _, _) = receiver.store.clipboard_snapshot().await?;
                if actions.0.load(Ordering::SeqCst) == 2
                    && register.is_some_and(|key| Some(key.hlc) == delivery.begin.hlc)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("retried image must apply after the newer clipboard")??;
        sender.outbox.receipt(delivery, None).await?;
        sender.shutdown_actions().await?;
        receiver.shutdown_actions().await?;
        Ok(())
    }

    async fn file_recipient_fixture() -> Result<(
        tempfile::TempDir,
        Arc<Core>,
        Id,
        Id,
        watch::Receiver<Option<CloseReason>>,
    )> {
        let (dir, core, connected) = fixture(Arc::new(NoActions)).await?;
        let close = admit_ui_test_phone(&core, &connected, 1)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&connected)
            .unwrap()
            .capabilities
            .push("files".into());
        let offline = Id::from_bytes([89; 16]);
        core.store
            .pair(offline.clone(), Hash::new("ab".repeat(32))?)
            .await?;
        core.outbox.note(
            offline.clone(),
            core.store.pairing_generation(&offline).unwrap(),
            true,
        );
        core.admission.lock().unwrap().peers = core.store.peers().await?;
        Ok((dir, core, connected, offline, close))
    }

    #[tokio::test]
    async fn desktop_handoff_rejections_notify_even_when_the_cli_cannot_show_errors() -> Result<()>
    {
        #[derive(Default)]
        struct Notices(Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl PcActions for Notices {
            async fn clipboard_text(&self, _: &str) -> Result<()> {
                panic!("no real clipboard");
            }
            async fn clipboard_image(&self, _: &std::path::Path, _: &str) -> Result<()> {
                panic!("no real clipboard");
            }
            async fn notify(&self, _: &std::path::Path, _: &str) -> Result<()> {
                panic!("no received files");
            }
            async fn open(&self, _: &std::path::Path) -> Result<()> {
                panic!("no viewer");
            }
            async fn notify_send_admission_failure(&self, reason: &str) -> Result<()> {
                self.0.lock().unwrap().push(reason.into());
                Ok(())
            }
        }
        let notices = Arc::new(Notices::default());
        let (_dir, core, _) = fixture(notices.clone()).await?;
        for n in 0..16 {
            core.present_send_error(format!("pending {n}"))?;
        }
        #[cfg(target_os = "macos")]
        core.ui_ready
            .lock()
            .unwrap()
            .store(true, std::sync::atomic::Ordering::Release);
        let (listener, _owner) = crate::ipc::bind(&core.paths.socket)?;
        let (stop, stopped) = watch::channel(false);
        let ipc = tokio::spawn(crate::ipc::serve(core.clone(), listener, stopped));
        let command = crate::ipc::desktop_send_command(&[core
            .paths
            .data
            .join("missing.txt")
            .to_string_lossy()
            .into_owned()]);
        let error = crate::ipc::request(&core.paths.socket, command)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("send window is busy"));
        assert_eq!(*notices.0.lock().unwrap(), vec!["send window is busy"]);
        assert_eq!(
            core.pending_sends.lock().unwrap().len(),
            16,
            "never discard earlier selections"
        );
        for command in [
            crate::ipc::Command::SendFiles {
                paths: Vec::new(),
                targets: crate::outbox::Targets::All("all".into()),
                request_id: None,
            },
            crate::ipc::Command::SendClipboard {
                notify_too_large: false,
                targets: crate::outbox::Targets::All("all".into()),
            },
        ] {
            assert!(
                crate::ipc::request(&core.paths.socket, command)
                    .await
                    .is_err()
            );
        }
        let failures = notices.0.lock().unwrap().clone();
        assert_eq!(failures.len(), 3);
        assert!(failures[1].contains("choose 1 to 128 files"));
        assert!(failures[2].contains("no connected clipboard recipients"));
        stop.send(true)?;
        ipc.await??;
        core.shutdown_actions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn desktop_explicit_targets_preserve_rejections_and_admit_eligible_siblings() -> Result<()>
    {
        use crate::outbox::Targets;
        for case in ["unknown", "unpaired", "unsupported", "offline"] {
            let (dir, core, connected, offline, _close) = file_recipient_fixture().await?;
            let other = if case == "unknown" {
                Id::from_bytes([90; 16])
            } else {
                offline.clone()
            };
            if case == "unpaired" {
                core.store.unpair_then(other.clone(), |_| {}).await?;
                core.admission.lock().unwrap().peers = core.store.peers().await?;
            } else if case == "unsupported" {
                core.outbox.note(
                    other.clone(),
                    core.store.pairing_generation(&other).unwrap(),
                    false,
                );
            }
            core.set_setting("choose_devices_each_time".into(), true.into())
                .await?;
            let source = dir.path().join("explicit.txt");
            std::fs::write(&source, b"selected bytes")?;
            let (listener, _owner) = crate::ipc::bind(&core.paths.socket)?;
            let (stop, stopped) = watch::channel(false);
            let ipc = tokio::spawn(crate::ipc::serve(core.clone(), listener, stopped));
            for queue in [false, true] {
                core.set_setting("queue_offline".into(), queue.into())
                    .await?;
                let args = vec![
                    "--request-id".into(),
                    format!("{case}-{queue}"),
                    "--to".into(),
                    connected.as_str().into(),
                    "--to".into(),
                    other.as_str().into(),
                    source.to_string_lossy().into_owned(),
                ];
                assert!(matches!(
                    crate::ipc::desktop_send_command(&args),
                    crate::ipc::Command::SendFiles {
                        targets: Targets::Devices(_),
                        ..
                    }
                ));
                let reply = crate::ipc::request(
                    &core.paths.socket,
                    crate::ipc::desktop_send_command(&args),
                )
                .await?;
                assert!(
                    core.pending_send().is_none(),
                    "--to must bypass even the one-recipient picker"
                );
                assert!(
                    core.window_request() > 0,
                    "Recent remains visible for feedback"
                );
                let rejected = reply["rejected"].as_array().context("CLI rejected list")?;
                if case == "offline" {
                    assert!(
                        rejected.is_empty(),
                        "explicit offline choices override queue_offline"
                    );
                } else {
                    assert_eq!(rejected.len(), 1, "{case}, queue={queue}");
                    assert_eq!(rejected[0]["device"], serde_json::json!(other));
                    assert_eq!(
                        rejected[0]["reason"],
                        if case == "unsupported" {
                            "Update this device to receive files"
                        } else {
                            "Device is not paired"
                        }
                    );
                }
                core.file_admissions.drain().await;
                let operation: Id = serde_json::from_value(reply["operation"].clone())?;
                let status = core.operation_status(operation.clone()).await?;
                assert_eq!(status["state"], "admitted");
                assert_eq!(status["accepted"].as_array().unwrap().len(), 1);
                assert_eq!(status["rejected"], reply["rejected"]);
                let rows = core.ui_snapshot().transfers;
                assert!(rows.iter().any(|r| r.operation == operation
                    && r.device == connected
                    && r.state == "queued"));
                assert!(rows.iter().any(|r| r.operation == operation
                    && r.device == other
                    && r.state
                        == if case == "offline" {
                            "waiting_for_device"
                        } else {
                            "failed"
                        }));
                let retry = crate::ipc::request(
                    &core.paths.socket,
                    crate::ipc::desktop_send_command(&args),
                )
                .await?;
                assert_eq!(retry["operation"], reply["operation"]);
                assert_eq!(retry["rejected"], reply["rejected"]);
                core.cancel_transfer(operation, None).await?;
            }
            stop.send(true)?;
            ipc.await??;
            core.shutdown_actions().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn desktop_file_handoffs_follow_picker_queue_rules_and_freeze_authority() -> Result<()> {
        use crate::outbox::Targets;
        let (dir, core, connected, _offline, _close) = file_recipient_fixture().await?;
        let source = dir.path().join("report.txt");
        std::fs::write(&source, b"selected")?;
        for (ask, queue, count) in [(false, false, 1), (false, true, 2), (true, false, 2)] {
            core.set_setting("choose_devices_each_time".into(), ask.into())
                .await?;
            core.set_setting("queue_offline".into(), queue.into())
                .await?;
            let reply = core.present_files(
                vec![source.clone()],
                Targets::All("all".into()),
                Some("desktop-test".into()),
            )?;
            assert!(core.window_request() > 0);
            let request = core.pending_send().unwrap();
            assert_eq!(request.id, reply["request"]);
            assert_eq!(request.targets.len(), count);
            assert!(!request.targeted);
            assert_eq!(request.paths, Some(vec![source.clone()]));
            assert!(
                core.ui_snapshot().transfers.is_empty(),
                "handoff never admits before picker"
            );
            core.acknowledge_send("stale-ack");
            assert!(core.pending_send().is_some());
            core.acknowledge_send(&request.id);
            assert!(core.pending_send().is_none());
        }
        core.present_files(vec![source.clone()], Targets::All("all".into()), None)?;
        let request = core.pending_send().unwrap();
        let old = request
            .targets
            .iter()
            .find(|t| t.device == connected)
            .unwrap()
            .clone();
        // No new pairing may be substituted between Dolphin click and Send.
        core.store
            .pair(connected.clone(), Hash::new("cd".repeat(32))?)
            .await?;
        core.admission.lock().unwrap().peers = core.store.peers().await?;
        let reply = core
            .send_files(vec![source], Targets::Captured(vec![old]))
            .await?;
        core.file_admissions.drain().await;
        let operation = serde_json::from_value(reply["operation"].clone())?;
        let result = core.operation_status(operation).await?;
        assert_eq!(result["state"], "failed");
        assert!(result["accepted"].as_array().unwrap().is_empty());
        assert!(
            result["rejected"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["reason"] == "Pairing changed")
        );
        core.shutdown_actions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn desktop_cli_exits_before_admission_but_window_recent_keeps_failures() -> Result<()> {
        use crate::ipc::Command;
        use crate::outbox::Targets;
        let (dir, core, connected, _, _close) = file_recipient_fixture().await?;
        #[cfg(target_os = "macos")]
        core.ui_ready
            .lock()
            .unwrap()
            .store(true, std::sync::atomic::Ordering::Release);
        let (listener, _owner) = crate::ipc::bind(&core.paths.socket)?;
        let (stop, stopped) = watch::channel(false);
        let ipc = tokio::spawn(crate::ipc::serve(core.clone(), listener, stopped));
        let path = dir.path().join("missing.pdf");
        let reply = crate::ipc::request(
            &core.paths.socket,
            crate::ipc::desktop_send_command(&[path.to_string_lossy().into_owned()]),
        )
        .await?;
        let request = core.pending_send().unwrap();
        assert_eq!(reply["request"], request.id);
        assert!(core.ui_snapshot().transfers.is_empty());
        // This is the window's admission, after the CLI has disconnected.
        let reply = crate::ipc::request(
            &core.paths.socket,
            Command::SendFiles {
                paths: request.paths.unwrap(),
                targets: Targets::Captured(request.targets),
                request_id: Some(request.id.clone()),
            },
        )
        .await?;
        crate::ipc::request(
            &core.paths.socket,
            Command::AcknowledgeSend {
                request: request.id,
            },
        )
        .await?;
        core.file_admissions.drain().await;
        let operation: Id = serde_json::from_value(reply["operation"].clone())?;
        let rows = core.ui_snapshot().transfers;
        let failed = rows.iter().find(|t| t.operation == operation).unwrap();
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.file_name, "missing.pdf");
        assert!(failed.error.is_some());
        // A file that disappears after the picker is equally visible.
        let authority = file_test_peer(&core, &connected).await?;
        std::fs::write(&path, b"selected")?;
        core.present_files(vec![path.clone()], Targets::Captured(vec![authority]), None)?;
        let request = core.pending_send().unwrap();
        std::fs::remove_file(path)?;
        let reply = core
            .send_files_request(
                request.paths.unwrap(),
                Targets::Captured(request.targets),
                Some(request.id),
            )
            .await?;
        core.file_admissions.drain().await;
        let operation: Id = serde_json::from_value(reply["operation"].clone())?;
        assert!(
            core.ui_snapshot()
                .transfers
                .iter()
                .any(|t| t.operation == operation && t.state == "failed" && t.error.is_some())
        );
        stop.send(true)?;
        ipc.await??;
        core.shutdown_actions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn captured_file_and_clipboard_commands_reject_replacement_pairings() -> Result<()> {
        use crate::outbox::Targets;
        let (dir, core, connected, offline, _close) = file_recipient_fixture().await?;
        let old = file_test_peer(&core, &connected).await?;
        let sibling = file_test_peer(&core, &offline).await?;
        core.store
            .pair(connected.clone(), Hash::new("cd".repeat(32))?)
            .await?;
        core.admission.lock().unwrap().peers = core.store.peers().await?;
        let current = file_test_peer(&core, &connected).await?;
        assert_ne!(old.generation, current.generation);
        let source = dir.path().join("selected.txt");
        std::fs::write(&source, b"old menu selection")?;
        for mut stale in [old.clone(), current.clone()] {
            if stale.generation == current.generation {
                stale.fingerprint = old.fingerprint.clone();
            }
            assert!(
                core.share_clipboard(Targets::Captured(vec![stale.clone()]))
                    .await
                    .is_err()
            );
            let reply = core
                .send_files(
                    vec![source.clone()],
                    Targets::Captured(vec![stale, sibling.clone()]),
                )
                .await?;
            core.file_admissions.drain().await;
            let operation: Id = serde_json::from_value(reply["operation"].clone())?;
            let status = core.operation_status(operation.clone()).await?;
            assert_eq!(status["accepted"].as_array().unwrap().len(), 1);
            assert_eq!(status["rejected"][0]["device"], connected.as_str());
            let rows = core.ui_snapshot().transfers;
            assert!(rows.iter().any(|t| t.operation == operation
                && t.device == offline
                && t.state == "waiting_for_device"));
            assert!(
                !rows.iter().any(|t| t.operation == operation
                    && t.device == connected
                    && t.state != "failed")
            );
            assert!(
                core.retry_transfer(operation.clone(), connected.clone())
                    .await
                    .is_err()
            );
            core.cancel_transfer(operation, Some(offline.clone()))
                .await?;
        }
        core.shutdown_actions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn ipc_admits_128_long_paths_from_cli_and_picker() -> Result<()> {
        use crate::ipc::Command;
        use crate::outbox::Targets;
        let (dir, core, connected, _, _close) = file_recipient_fixture().await?;
        let mut parent = dir.path().to_owned();
        for n in 0..6 {
            parent.push(format!("{n}{}", "p".repeat(100)));
        }
        std::fs::create_dir_all(&parent)?;
        let source = parent.join("report.txt");
        std::fs::write(&source, b"long path")?;
        let authority = file_test_peer(&core, &connected).await?;
        let (listener, _owner) = crate::ipc::bind(&core.paths.socket)?;
        let (stop, stopped) = watch::channel(false);
        let ipc = tokio::spawn(crate::ipc::serve(core.clone(), listener, stopped));
        let mut args = vec!["--to".into(), connected.as_str().into()];
        args.extend(vec![source.to_string_lossy().into_owned(); 128]);
        let commands = [
            crate::ipc::send_command(&args)?,
            Command::SendFiles {
                paths: vec![source; 128],
                targets: Targets::Captured(vec![authority]),
                request_id: None,
            },
        ];
        for command in commands {
            assert!(serde_json::to_vec(&command)?.len() > 65_536);
            let result = crate::ipc::request(&core.paths.socket, command).await?;
            let operation: Id = serde_json::from_value(result["operation"].clone())?;
            // Admission stages 128 durable files. Its tracked completion, not
            // disk speed under parallel tests, is the IPC frame-size barrier.
            core.file_admissions.drain().await;
            let result = crate::ipc::request(
                &core.paths.socket,
                Command::OperationStatus {
                    operation: operation.clone(),
                },
            )
            .await?;
            assert_eq!(result["state"], "admitted");
            assert_eq!(result["accepted"].as_array().unwrap().len(), 128);
            assert!(result["rejected"].as_array().unwrap().is_empty());
            core.cancel_transfer(operation, None).await?;
        }
        stop.send(true)?;
        ipc.await??;
        core.shutdown_actions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn named_file_recipient_sets_match_cli_and_ipc_selection_semantics() -> Result<()> {
        use crate::outbox::{Authority, Targets};
        let (dir, core, connected, offline, _close) = file_recipient_fixture().await?;
        let source = dir.path().join("selection.txt");
        std::fs::write(&source, b"recipient selection")?;
        let captured = file_test_peer(&core, &offline).await?;
        for queue in [false, true] {
            core.set_setting("queue_offline".into(), queue.into())
                .await?;
            let variants = vec![
                (
                    crate::ipc::send_command(&[source.to_string_lossy().into_owned()])?,
                    if queue {
                        vec![connected.clone(), offline.clone()]
                    } else {
                        vec![connected.clone()]
                    },
                ),
                (
                    crate::ipc::send_command(&[
                        "--to".into(),
                        offline.as_str().into(),
                        source.to_string_lossy().into_owned(),
                    ])?,
                    vec![offline.clone()],
                ),
                (
                    serde_json::from_value(
                        serde_json::json!({"command":"send_files","paths":[source],"targets":"all"}),
                    )?,
                    if queue {
                        vec![connected.clone(), offline.clone()]
                    } else {
                        vec![connected.clone()]
                    },
                ),
                (
                    crate::ipc::Command::SendFiles {
                        paths: vec![source.clone()],
                        targets: Targets::Devices(vec![offline.clone()]),
                        request_id: None,
                    },
                    vec![offline.clone()],
                ),
                (
                    crate::ipc::Command::SendFiles {
                        paths: vec![source.clone()],
                        targets: Targets::Captured(vec![Authority {
                            device: captured.device.clone(),
                            generation: captured.generation,
                            fingerprint: captured.fingerprint.clone(),
                        }]),
                        request_id: None,
                    },
                    vec![offline.clone()],
                ),
            ];
            for (command, expected) in variants {
                let crate::ipc::Command::SendFiles {
                    paths,
                    targets,
                    request_id,
                } = command
                else {
                    unreachable!()
                };
                let reply = core.send_files_request(paths, targets, request_id).await?;
                core.file_admissions.drain().await;
                let operation: Id = serde_json::from_value(reply["operation"].clone())?;
                let rows: Vec<_> = core
                    .ui_snapshot()
                    .transfers
                    .into_iter()
                    .filter(|t| t.operation == operation)
                    .collect();
                assert_eq!(rows.len(), expected.len());
                for row in rows {
                    assert!(expected.contains(&row.device));
                    assert_eq!(
                        row.state,
                        if row.device == offline {
                            "waiting_for_device"
                        } else {
                            "queued"
                        }
                    );
                }
            }
        }
        core.shutdown_actions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn full_file_recipient_is_rejected_in_status_and_recent_without_blocking_sibling()
    -> Result<()> {
        use crate::outbox::{PEER_DELIVERIES, Targets};
        let (dir, core, connected, offline, _close) = file_recipient_fixture().await?;
        let source = dir.path().join("queued.txt");
        std::fs::write(&source, b"pending")?;
        let peer = file_test_peer(&core, &offline).await?;
        for _ in 0..PEER_DELIVERIES {
            core.outbox
                .admit(
                    Id::from_bytes([90; 16]),
                    source.clone(),
                    vec![peer.clone()],
                    100,
                )
                .await?;
        }
        core.set_setting("queue_offline".into(), true.into())
            .await?;
        let source = dir.path().join("report.pdf");
        std::fs::write(&source, b"new artifact")?;
        for only_full in [false, true] {
            let targets = if only_full {
                Targets::Devices(vec![offline.clone()])
            } else {
                Targets::All("all".into())
            };
            let reply = core.send_files(vec![source.clone()], targets).await?;
            core.file_admissions.drain().await;
            let operation: Id = serde_json::from_value(reply["operation"].clone())?;
            let status = core.operation_status(operation.clone()).await?;
            assert_eq!(
                status["state"],
                if only_full { "failed" } else { "admitted" }
            );
            assert_eq!(
                status["accepted"].as_array().unwrap().len(),
                usize::from(!only_full)
            );
            assert_eq!(
                status["rejected"],
                serde_json::json!([{"path":source,"device":offline,"reason":"device outbox is full"}])
            );
            let rows: Vec<_> = core
                .ui_snapshot()
                .transfers
                .into_iter()
                .filter(|t| t.operation == operation)
                .collect();
            assert_eq!(rows.len(), if only_full { 1 } else { 2 });
            let rejected = rows.iter().find(|t| t.device == offline).unwrap();
            assert_eq!(rejected.state, "failed");
            assert_eq!(rejected.file_name, "report.pdf");
            assert!(rejected.path.is_none());
            assert_eq!(rejected.error.as_deref(), Some("device outbox is full"));
            if !only_full {
                assert_eq!(
                    rows.iter().find(|t| t.device == connected).unwrap().state,
                    "queued"
                );
            }
        }
        core.shutdown_actions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn file_replacement_during_claim_handoff_recovers_after_waiter_is_cancelled() -> Result<()>
    {
        let (dir, core, device) = fixture(Arc::new(NoActions)).await?;
        core.set_setting("queue_offline".into(), true.into())
            .await?;
        let _old = admit_ui_test_phone(&core, &device, 1)?;
        let authority = file_test_peer(&core, &device).await?;
        let source = dir.path().join("source");
        std::fs::write(&source, b"claimed bytes")?;
        let operation = Id::from_bytes([80; 16]);
        let id = core
            .outbox
            .admit(operation, source, vec![authority.clone()], 100)
            .await?;
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        *core.outbox.claim_barrier.lock().unwrap() = Some((entered, blocked));
        let sender = core.clone();
        let peer = device.clone();
        let generation = authority.generation;
        let old = tokio::spawn(async move {
            // Armed before claim, including cancellation after its durable commit.
            let _recovery = sender.arm_file_recovery(peer.clone(), generation, 1);
            sender.outbox.claim(peer, generation, 1).await
        });
        wait.await?;
        let _replacement = admit_ui_test_phone(&core, &device, 2)?;
        old.abort();
        assert!(old.await.unwrap_err().is_cancelled());
        release.send(())?;
        core.store.drain().await;
        let next = core
            .outbox
            .claim(device.clone(), generation, 2)
            .await?
            .unwrap();
        assert_eq!(next.begin.transfer_id, id);
        core.outbox.retry_recovery().await?;
        assert!(core.outbox.current(&next));
        assert_eq!(core.ui_snapshot().transfers[0].state, "sending");
        core.outbox.receipt(next, None).await?;
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn file_staged_hash_verification_does_not_occupy_transport_writer() -> Result<()> {
        let (dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let close = admit_ui_test_phone(&core, &device, 1)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&device)
            .unwrap()
            .capabilities
            .push("files".into());
        let authority = file_test_peer(&core, &device).await?;
        let source = dir.path().join("source.png");
        std::fs::write(&source, b"verified bytes")?;
        core.outbox
            .admit(
                Id::from_bytes([80; 16]),
                source,
                vec![authority.clone()],
                100,
            )
            .await?;
        let delivery = core
            .outbox
            .claim(device, authority.generation, 1)
            .await?
            .unwrap();
        let legacy_stamp = delivery.begin.hlc.expect("origin image stamp");
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        *core.outbox.reader_barrier.lock().unwrap() = Some((entered, blocked));
        let outbox = core.outbox.clone();
        let captured = delivery.clone();
        let preparing = tokio::spawn(async move { outbox.reader(captured).await });
        wait.await?;
        let (_normal, normal) = mpsc::channel(4);
        let (priority_tx, priority) = mpsc::channel(4);
        let (_automatic, automatic) = mpsc::channel(1);
        let (_text, text) = mpsc::channel(1);
        let (files_tx, files) = mpsc::channel(1);
        let (reports, mut received) = mpsc::channel(4);
        let (mut stream, mut wire) = tokio::io::duplex(4096);
        let sender = core.clone();
        let writer = tokio::spawn(async move {
            writer_loop(
                sender,
                &mut stream,
                WriterQueues {
                    normal: normal.into(),
                    priority,
                    automatic,
                    text,
                    files,
                    reports,
                    close,
                    before_queue_poll: None,
                },
            )
            .await
        });
        let pong = Message::Pong(Ping {
            nonce: Nonce::new("0123456789abcdef")?,
        });
        priority_tx.send(pong.clone()).await?;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), read_frame(&mut wire)).await??,
            Some(pong)
        );
        // The verifier is still blocked, yet heartbeat already reached the peer.
        assert!(!preparing.is_finished());
        release.send(())?;
        files_tx
            .send(FileWrite {
                delivery,
                connection: 1,
                mesh: false,
                file: Some(preparing.await??.into()),
                ready: None,
                receipt_ready: Arc::new(AtomicBool::new(false)),
            })
            .await
            .map_err(|_| anyhow::anyhow!("writer stopped"))?;
        assert!(matches!(
            read_frame(&mut wire).await?,
            Some(Message::FileBegin(begin)) if begin.hlc.is_none() && begin.source_device.is_none() && begin.created_at == legacy_stamp
        ));
        assert!(matches!(
            read_frame(&mut wire).await?,
            Some(Message::FileChunk(_))
        ));
        assert!(matches!(
            read_frame(&mut wire).await?,
            Some(Message::FileEnd(_))
        ));
        assert!(matches!(
            received.recv().await,
            Some(WriteReport::Sent { .. })
        ));
        writer.abort();
        let _ = writer.await;
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn file_ipc_returns_operation_before_staging_and_lost_client_retry_is_idempotent()
    -> Result<()> {
        let (dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _close = admit_ui_test_phone(&core, &device, 1)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&device)
            .unwrap()
            .capabilities
            .push("files".into());
        let peers = core.store.peers().await?;
        core.admission.lock().unwrap().peers = peers;
        let path = dir.path().join("slow-source");
        std::fs::write(&path, b"slow disk")?;
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let hook = Arc::new(Mutex::new(Some((entered, blocked))));
        *core.outbox.source_hook.lock().unwrap() = Some(Arc::new(move |_| {
            let (entered, release) = hook.lock().unwrap().take().unwrap();
            let _ = entered.send(());
            let _ = release.recv_timeout(Duration::from_secs(10));
        }));
        let (listener, _socket) = crate::ipc::bind(&core.paths.socket)?;
        let (stop, rx) = watch::channel(false);
        let ipc = tokio::spawn(crate::ipc::serve(core.clone(), listener, rx));
        let command = || crate::ipc::Command::SendFiles {
            paths: vec![path.clone()],
            targets: crate::outbox::Targets::All("all".into()),
            request_id: Some("slow-request".into()),
        };
        let first = tokio::time::timeout(
            Duration::from_secs(1),
            crate::ipc::request(&core.paths.socket, command()),
        )
        .await??;
        assert!(
            core.window_request() > 0,
            "untargeted IPC opens Recent before staging can fail"
        );
        wait.await?;
        let operation: Id = serde_json::from_value(first["operation"].clone())?;
        assert_eq!(
            core.operation_status(operation.clone()).await?["state"],
            "staging"
        );
        assert!(
            core.ui_snapshot()
                .transfers
                .iter()
                .any(|t| t.operation == operation && t.state == "staging")
        );
        let retry = crate::ipc::request(&core.paths.socket, command()).await?;
        assert_eq!(retry, first);
        release.send(())?;
        core.file_admissions.drain().await;
        let status = core.operation_status(operation.clone()).await?;
        assert_eq!(status["state"], "admitted");
        assert_eq!(status["accepted"].as_array().unwrap().len(), 1);
        let retry = crate::ipc::request(&core.paths.socket, command()).await?;
        assert_eq!(retry, first);
        assert_eq!(
            core.ui_snapshot()
                .transfers
                .iter()
                .filter(|t| t.operation == operation)
                .count(),
            1
        );
        stop.send(true)?;
        ipc.await??;
        core.shutdown_actions().await?;
        Ok(())
    }
    fn held_clipboard_publication(core: &Core) -> OutgoingSelection {
        OutgoingSelection::Text(Clipboard {
            sensitive: false,
            transfer_id: Id::from_bytes([70; 16]),
            text: "already computed publication".into(),
            origin: ClipboardOrigin::Clipboard,
            created_at: 10,
            hlc: Some(10),
            source_device: Some(core.identity.device_id.clone()),
        })
    }

    #[tokio::test]
    async fn late_publication_and_first_byte_are_fenced_by_config_changes() -> Result<()> {
        for key in [
            "clipboard_mode",
            "clipboard_sync_to_phone",
            "clipboard_include_images",
        ] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let _link = admit_ui_test_phone(&core, &device, 1)?;
            core.set_setting("clipboard_include_images".into(), true.into())
                .await?;
            let mut held = core.automatic_target().unwrap();
            held.observed = true;
            let selection = held_clipboard_publication(&core);
            // Hold one computed Publish effect, with a sibling already queued.
            core.publish_automatic(held.clone(), Some(selection.clone()), None)?;
            let work = core.take_automatic(&device, 1).unwrap();
            let value = if key == "clipboard_mode" {
                serde_json::json!("manual")
            } else {
                false.into()
            };
            core.set_setting(key.into(), value).await?;
            assert!(
                core.publish_automatic(held.clone(), Some(selection.clone()), None)
                    .is_err(),
                "{key}"
            );
            assert!(
                core.automatic.lock().unwrap().selections.is_empty(),
                "{key}"
            );
            // A first-byte writer already holding work also rejects it.
            let (_priority, mut priority) = mpsc::channel(1);
            let (_normal, normal) = mpsc::channel(1);
            let mut normal = phone_controls::FrameQueue::from(normal);
            let mut bytes = Vec::new();
            assert!(
                !transmit_automatic(
                    &core,
                    &mut bytes,
                    work,
                    &mut priority,
                    &mut normal,
                    None,
                    None
                )
                .await?
            );
            assert!(bytes.is_empty());
            let restored = if key == "clipboard_mode" {
                serde_json::json!("automatic")
            } else {
                true.into()
            };
            core.set_setting(key.into(), restored).await?;
            assert!(
                core.publish_automatic(held, Some(selection.clone()), None)
                    .is_err(),
                "re-enable {key}"
            );
            // Current offers still publish after re-enable.
            core.publish_automatic(core.automatic_target().unwrap(), Some(selection), None)?;
            core.shutdown_actions().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn manual_mode_rejects_observed_publication_but_accepts_explicit_share() -> Result<()> {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _link = admit_ui_test_phone(&core, &device, 1)?;
        core.set_setting("clipboard_mode".into(), serde_json::json!("manual"))
            .await?;
        let mut offer = core.automatic_target().unwrap();
        let selection = held_clipboard_publication(&core);
        offer.observed = true;
        assert!(
            core.publish_automatic(offer.clone(), Some(selection.clone()), None)
                .is_err()
        );
        offer.observed = false;
        core.publish_automatic(offer, Some(selection), None)?;
        assert!(core.take_automatic(&device, 1).is_some());
        core.shutdown_actions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn pair_send_disable_fences_a_precomputed_publication_even_after_reenable() -> Result<()>
    {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _link = admit_ui_test_phone(&core, &device, 1)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&device)
            .unwrap()
            .mesh = true;
        let local = core.identity.device_id.clone();
        core.store
            .settings_fields(local.clone(), device.clone(), true)
            .await?;
        let mut held = core.automatic_target().unwrap();
        held.observed = true;
        let selection = held_clipboard_publication(&core);
        for enabled in [false, true] {
            core.store
                .settings_edit(
                    local.clone(),
                    device.clone(),
                    true,
                    format!("{local}/{device}/clipboard"),
                    enabled,
                )
                .await?;
            assert!(
                core.publish_automatic(held.clone(), Some(selection.clone()), None)
                    .is_err()
            );
            assert!(core.automatic.lock().unwrap().selections.is_empty());
        }
        core.publish_automatic(core.automatic_target().unwrap(), Some(selection), None)?;
        core.shutdown_actions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_staging_stops_remaining_paths_without_failed_recent_rows() -> Result<()> {
        for per_device in [false, true] {
            let (dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let _link = admit_ui_test_phone(&core, &device, 1)?;
            core.sessions
                .lock()
                .unwrap()
                .sessions
                .get_mut(&device)
                .unwrap()
                .capabilities
                .push("files".into());
            let peers = core.store.peers().await?;
            core.admission.lock().unwrap().peers = peers;
            let source = dir.path().join("staging-source");
            std::fs::write(&source, b"cancel during open")?;
            let paths = std::iter::once(source)
                .chain((0..127).map(|n| dir.path().join(format!("never-open-{n}"))))
                .collect();
            let (entered, wait) = tokio::sync::oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            let barrier = Arc::new(Mutex::new(Some((entered, blocked))));
            *core.outbox.source_hook.lock().unwrap() = Some(Arc::new(move |_| {
                let (entered, release) = barrier.lock().unwrap().take().unwrap();
                let _ = entered.send(());
                release.recv_timeout(Duration::from_secs(10)).unwrap();
            }));
            let reply = core
                .send_files(paths, crate::outbox::Targets::All("all".into()))
                .await?;
            wait.await?;
            let operation: Id = serde_json::from_value(reply["operation"].clone())?;
            core.cancel_transfer(operation.clone(), per_device.then_some(device))
                .await?;
            assert_eq!(
                core.operation_status(operation.clone()).await?["state"],
                "cancelled"
            );
            release.send(())?;
            tokio::time::timeout(Duration::from_secs(5), core.file_admissions.drain()).await?;
            let status = core.operation_status(operation.clone()).await?;
            assert_eq!(status["state"], "cancelled");
            assert_eq!(status["rejected"], serde_json::json!([]));
            assert_eq!(status["accepted"], serde_json::json!([]));
            assert!(
                core.ui_snapshot()
                    .transfers
                    .iter()
                    .all(|r| r.operation != operation)
            );
            assert_eq!(
                std::fs::read_dir(dir.path().join("data/outbox"))?.count(),
                0
            );
            core.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn artifact_transfer_commands_preserve_artifact_scope_through_ipc_dispatch() -> Result<()>
    {
        use crate::ipc::Command;
        let (dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _link = admit_ui_test_phone(&core, &device, 1)?;
        core.sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&device)
            .unwrap()
            .capabilities
            .push("files".into());
        let authority = file_test_peer(&core, &device).await?;
        let operation = Id::from_bytes([80; 16]);
        let source = dir.path().join("source");
        std::fs::write(&source, b"artifact IPC")?;
        let a = core
            .outbox
            .admit(
                operation.clone(),
                source.clone(),
                vec![authority.clone()],
                100,
            )
            .await?;
        let b = core
            .outbox
            .admit(operation.clone(), source, vec![authority.clone()], 100)
            .await?;
        let failed = core
            .outbox
            .claim(device.clone(), authority.generation, 1)
            .await?
            .unwrap();
        core.outbox.fail(failed, "injected failure".into()).await?;
        let sibling = core
            .outbox
            .claim(device.clone(), authority.generation, 1)
            .await?
            .unwrap();
        assert_eq!(sibling.begin.transfer_id, b);
        let (listener, _socket) = crate::ipc::bind(&core.paths.socket)?;
        let (stop, rx) = watch::channel(false);
        let ipc = tokio::spawn(crate::ipc::serve(core.clone(), listener, rx));
        crate::ipc::request(
            &core.paths.socket,
            Command::RetryTransfer {
                operation: operation.clone(),
                artifact: Some(a.clone()),
                device: device.clone(),
            },
        )
        .await?;
        assert!(core.outbox.current(&sibling));
        assert!(
            core.ui_snapshot()
                .transfers
                .iter()
                .any(|r| r.transfer_id == a && r.state == "queued")
        );
        crate::ipc::request(
            &core.paths.socket,
            Command::CancelTransfer {
                operation: operation.clone(),
                artifact: Some(a.clone()),
                device: Some(device.clone()),
            },
        )
        .await?;
        assert!(core.outbox.current(&sibling));
        crate::ipc::request(
            &core.paths.socket,
            Command::DismissTransfer {
                operation: operation.clone(),
                artifact: Some(a.clone()),
            },
        )
        .await?;
        let status = crate::ipc::request(&core.paths.socket, Command::Status).await?;
        let rows = status["transfers"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["transfer_id"], serde_json::json!(b));
        for command in [
            Command::DismissTransfer {
                operation: operation.clone(),
                artifact: Some(Id::from_bytes([99; 16])),
            },
            Command::CancelTransfer {
                operation: operation.clone(),
                artifact: Some(Id::from_bytes([99; 16])),
                device: None,
            },
            // Validate artifact membership before looking up even an unknown device.
            Command::RetryTransfer {
                operation: operation.clone(),
                artifact: Some(Id::from_bytes([99; 16])),
                device: Id::from_bytes([99; 16]),
            },
        ] {
            let error = crate::ipc::request(&core.paths.socket, command)
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("artifact does not belong to operation"),
                "{error}"
            );
            assert!(core.outbox.current(&sibling));
        }
        // An older client's omitted artifact retains batch cancellation.
        let legacy: Command = serde_json::from_value(
            serde_json::json!({"command":"cancel_transfer", "operation":operation}),
        )?;
        crate::ipc::request(&core.paths.socket, legacy).await?;
        assert!(!core.outbox.current(&sibling));
        stop.send(true)?;
        ipc.await??;
        core.shutdown_actions().await?;
        Ok(())
    }
    include!("resume_tests.rs");
    include!("folder_tests.rs");
}

#[cfg(all(test, windows))]
mod windows_network_tests {
    use super::*;
    #[tokio::test]
    #[ignore = "opt-in: requires an active LAN with multicast; run with --ignored"]
    async fn native_mdns_advertisement_registers_and_shuts_down() -> Result<()> {
        let listener = crate::net::listen(0).await?;
        let advertisement = Advertisement::register(
            &format!("Pneu-m2b-net-{}", std::process::id()),
            &"0".repeat(64),
            listener.local_addr()?.port(),
            crate::discovery::candidates()?,
        )?;
        eprintln!(
            "mDNS registered {} on {:?}",
            advertisement.fullname, advertisement.addresses
        );
        // Register is asynchronous. Wait for the daemon's actual announcement,
        // rather than treating queueing the command as successful registration.
        let monitor = advertisement.daemon.monitor()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut announced = false;
        while std::time::Instant::now() < deadline {
            if let Ok(event) = monitor.recv_timeout(Duration::from_millis(200)) {
                eprintln!("mDNS monitor: {event:?}");
                match event {
                    mdns_sd::DaemonEvent::Announce(_, _) => {
                        announced = true;
                        break;
                    }
                    mdns_sd::DaemonEvent::Error(error) => anyhow::bail!("mDNS: {error}"),
                    _ => {}
                }
            }
        }
        assert!(announced, "mDNS did not announce on the VM's candidates");
        let status = advertisement
            .daemon
            .shutdown()?
            .recv_timeout(Duration::from_secs(3))?;
        assert_eq!(status, mdns_sd::DaemonStatus::Shutdown);
        eprintln!("mDNS shutdown: {status:?}");
        drop(advertisement);
        Ok(())
    }
}
