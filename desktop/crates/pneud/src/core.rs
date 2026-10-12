use crate::{
    Config, Paths,
    actions::{PcActions, ReceivedFile},
    identity::Identity,
    storage::{Stage, VerifiedFile},
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
}
const NOTIFICATION_LIMIT: usize = 16;
const EXPLICIT_IMAGE_LIMIT: usize = 8;
#[derive(Default)]
struct PendingActions {
    automatic: Option<ActionJob>,
    explicit: VecDeque<ActionJob>,
    notifications: VecDeque<(ActionJob, DesktopAction)>,
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
            if job.config.notification_preview {
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
        if pending.notifications.len() == NOTIFICATION_LIMIT {
            pending.notifications.pop_front();
        }
        pending.notifications.push_back((job, action));
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
        pending
            .notifications
            .retain(|(j, _)| j.scope.allows(&j.origin));
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
    config: watch::Sender<Config>,
    events: broadcast::Sender<Event>,
    store: Store,
    admission: Arc<Mutex<Admission>>,
    provisional: Arc<AtomicBool>,
    pairing_gate: Arc<tokio::sync::Mutex<()>>,
    config_gate: Arc<tokio::sync::Mutex<()>>,
    clipboard_gate: Arc<tokio::sync::Mutex<()>>,
    next_connection: AtomicU64,
    sessions: Arc<Mutex<Connections>>,
    actions: Arc<dyn PcActions>,
    action_queue: Arc<ActionQueue>,
    action_scope: Arc<Mutex<Option<(Id, ActionScope)>>>,
    action_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    shutdown: watch::Sender<bool>,
    port: AtomicU16,
    #[cfg(test)]
    file_write_delay: Mutex<Duration>,
}
impl Core {
    pub async fn new(
        paths: Paths,
        config: Config,
        actions: Arc<dyn PcActions>,
    ) -> Result<Arc<Self>> {
        config.validate()?;
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
        let action_queue = Arc::new(ActionQueue::default());
        let action_scope = admission
            .lock()
            .expect("admission")
            .peers
            .values()
            .next()
            .map(|id| (id.clone(), ActionScope::new()));
        let core = Arc::new(Self {
            identity,
            paths,
            config,
            events,
            store,
            admission,
            provisional: Arc::new(AtomicBool::new(false)),
            pairing_gate: Arc::new(tokio::sync::Mutex::new(())),
            config_gate: Arc::new(tokio::sync::Mutex::new(())),
            sessions: Arc::new(Mutex::new(Connections::default())),
            clipboard_gate: Arc::new(tokio::sync::Mutex::new(())),
            next_connection: AtomicU64::new(1),
            actions,
            action_queue: action_queue.clone(),
            action_scope: Arc::new(Mutex::new(action_scope)),
            action_worker: Mutex::new(None),
            shutdown,
            port: AtomicU16::new(port),
            #[cfg(test)]
            file_write_delay: Mutex::new(Duration::ZERO),
        });
        let weak = Arc::downgrade(&core);
        let stop = core.shutdown.subscribe();
        *core.action_worker.lock().expect("actions") =
            Some(tokio::spawn(action_loop(weak, action_queue, stop)));
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
            s.paused = paused;
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
    fn event(&self, event: Event) {
        let _ = self.events.send(event);
    }
    fn error(&self, device: Option<Id>, message: impl Into<String>) {
        self.event(Event::Error {
            device,
            message: message.into(),
        });
    }
    pub async fn start_pairing(&self) -> Result<PairingUri> {
        let addresses: Vec<String> = crate::discovery::candidates()?
            .into_iter()
            .map(|i| SocketAddr::new(i.ip, self.bound_port()).to_string())
            .collect();
        ensure!(!addresses.is_empty(), "no reachable interface for pairing");
        self.start_pairing_with_addresses(addresses).await
    }
    pub async fn start_pairing_with_addresses(&self, addresses: Vec<String>) -> Result<PairingUri> {
        let _guard = self.pairing_gate.lock().await;
        let uri = PairingUri {
            version: 1,
            name: self.config().name,
            fingerprint: self.identity.fingerprint.clone(),
            addresses,
            token: Hash::new(crate::random_hex(32))?,
            expires_at: crate::now_ms() + 300_000,
        };
        uri.validate()?;
        self.admission.lock().expect("admission").invitation = Some(Invitation {
            token: uri.token.clone(),
            expires_at: uri.expires_at,
            deadline: std::time::Instant::now() + Duration::from_secs(300),
        });
        Ok(uri)
    }
    pub async fn close_pairing(&self) {
        let _guard = self.pairing_gate.lock().await;
        self.admission.lock().expect("admission").invitation = None;
    }
    pub async fn unpair_device(&self, device: Id) -> Result<()> {
        self.forget_pairing(device, CloseReason::Unpair, None).await
    }
    async fn forget_pairing(
        &self,
        device: Id,
        reason: CloseReason,
        expected: Option<u64>,
    ) -> Result<()> {
        let guard = self.pairing_gate.clone().lock_owned().await;
        // A stale local command for the phone replaced by a new pairing must
        // not mark the current phone unpairing or disturb its connection.
        if !self.paired_devices().contains(&device) {
            return Ok(());
        }
        {
            let mut state = self.sessions.lock().expect("sessions");
            if expected.is_some()
                && state
                    .session
                    .as_ref()
                    .is_none_or(|s| Some(s.id) != expected)
            {
                return Ok(());
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
                if let Some(session) = state.session.take().filter(|s| s.device == device) {
                    session.close.send_replace(Some(if persisted {
                        reason
                    } else {
                        CloseReason::Error(ErrorCode::Closing)
                    }));
                }
                state.unpairing = false;
                if persisted {
                    let _ = events.send(Event::Unpaired {
                        device: device.clone(),
                    });
                } else {
                    let _ = events.send(Event::Error {
                        device: Some(device.clone()),
                        message: "unpair persistence failed; pairing retained".into(),
                    });
                }
                let _ = events.send(Event::ConnectionChanged {
                    device,
                    connected: false,
                });
            })
            .await?;
        if let Some(scope) = scope {
            let _idle = scope.active.lock().await;
        }
        Ok(())
    }
    pub async fn update_config(&self, config: Config) -> Result<()> {
        let guard = self.config_gate.clone().lock_owned().await;
        config.validate()?;
        ensure!(
            config.port == self.config().port,
            "changing the port takes effect after restarting pneud"
        );
        let path = self.paths.config.clone();
        let settings = self.config.clone();
        let events = self.events.clone();
        self.store
            .blocking(move |_| {
                let _guard = guard;
                config.save(&path)?;
                settings.send_replace(config);
                let _ = events.send(Event::ConfigChanged);
                Ok(())
            })
            .await
    }
    pub async fn send_clipboard(&self, device: Id, text: String) -> Result<Id> {
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
        // The store assigns the timestamp and replaces the pending value in
        // one transaction, so concurrent IPC jobs cannot reverse their order.
        let text = store.enqueue_clipboard(device.clone(), text).await?;
        self.wake(&device);
        Ok(text.transfer_id)
    }
    /// Discard the latest pending text.
    pub async fn discard_clipboard(&self, device: Id) -> Result<()> {
        let store = {
            let _guard = self.pairing_gate.lock().await;
            self.store.with_current_pairing()
        };
        store.discard_outgoing(device.clone()).await?;
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
    pub fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }
    pub async fn shutdown_actions(&self) -> Result<()> {
        let task = self.action_worker.lock().expect("actions").take();
        if let Some(task) = task {
            task.abort();
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
                        tasks.spawn(async move { let _permit = permit; let mut stop = core.shutdown.subscribe(); if *stop.borrow() { return; } let outcome = tokio::select! { biased; _ = stop.changed() => Ok(()), outcome = core.clone().connection(stream) => outcome }; if let Err(e) = outcome { let reason = if let Some(protocol) = e.downcast_ref::<ProtocolError>() { format!("protocol {:?}",decode_code(protocol)) } else if let Some(io) = e.downcast_ref::<std::io::Error>() { format!("transport {:?}",io.kind()) } else { "transport or local state failure".into() }; core.error(None,format!("connection closed: {reason}")); } });
                    }
                }
            }
        }
        drop(mdns);
        if let Some(session) = &self.sessions.lock().expect("sessions").session {
            session
                .close
                .send_replace(Some(CloseReason::Error(ErrorCode::Closing)));
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
                for entry in std::fs::read_dir(cache)? {
                    let entry = entry?;
                    if entry.file_name().to_string_lossy().starts_with("received-")
                        && entry.file_type()?.is_file()
                        && entry.metadata()?.modified()? < cutoff
                    {
                        std::fs::remove_file(entry.path())?;
                    }
                }
                Ok(())
            })
            .await
    }
    async fn connection(self: Arc<Self>, stream: TcpStream) -> Result<()> {
        stream.set_nodelay(true)?;
        let verifier = Arc::new(ClientVerifier::new(
            self.admission.clone(),
            self.provisional.clone(),
        ));
        let config = crate::tls::server_config(&self.identity, verifier.clone())?;
        let mut tls = tokio::time::timeout(
            Duration::from_secs(10),
            TlsAcceptor::from(Arc::new(config)).accept(stream),
        )
        .await??;
        let fp = certificate_fingerprint(
            tls.get_ref()
                .1
                .peer_certificates()
                .context("missing client cert")?[0]
                .as_ref(),
        );
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
            let result = self.finish_pairing(&hello.device_id, &fp, &token).await;
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
        drop(verifier);
        if current { result } else { Ok(()) }
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
            if invitation.expires_at <= crate::now_ms()
                || invitation.deadline <= std::time::Instant::now()
            {
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
        let device = device.clone();
        let fp = fp.clone();
        self.store
            .pair_then(device.clone(), fp.clone(), move || {
                let _guard = guard;
                let mut connections = sessions.lock().expect("sessions");
                connections.unpairing = false;
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
                    let _ = events.send(Event::Unpaired {
                        device: old.clone(),
                    });
                }
                state.peers.clear();
                state.peers.insert(fp, device.clone());
                state.invitation = None;
                drop(state);
                let _ = events.send(Event::Paired { device });
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
        let ReadyState {
            store,
            allow_recovery,
            action_scope,
        } = state;
        let mut config = self.config.subscribe();
        let (mut reader, mut writer) = tokio::io::split(tls);
        let (write_tx, mut write_rx) = mpsc::channel::<Message>(32);
        let (priority_tx, mut priority_rx) = mpsc::channel::<Message>(8);
        let (read_tx, mut read_rx) = mpsc::channel::<Message>(8);
        let (file_tx, file_rx) = mpsc::channel::<Message>(4);
        let (text_tx, mut text_rx) = mpsc::channel::<Message>(2);
        let mut writer_close = close.clone();
        let writer_task = tokio::spawn(async move {
            loop {
                let reason = *writer_close.borrow();
                if let Some(reason) = reason {
                    return close_transport(&mut writer, reason).await;
                }
                let message = tokio::select! {biased;
                    _=writer_close.changed()=>{let reason=writer_close.borrow().unwrap_or(CloseReason::Error(ErrorCode::Closing));return close_transport(&mut writer,reason).await;},
                    message=priority_rx.recv()=>message,
                    message=write_rx.recv()=>message,
                };
                let Some(message) = message else {
                    break;
                };
                if matches!(message, Message::Error(_) | Message::Unpair(_)) {
                    return crate::transport::close(&mut writer, &[message]).await;
                }
                let sent = tokio::select! { biased;
                    _ = writer_close.changed() => None,
                    sent = send(&mut writer, &message) => Some(sent),
                };
                match sent {
                    Some(result) => result?,
                    // A canceled frame may be partial. Do not append ERROR or
                    // UNPAIR to that frame; close the unusable stream promptly.
                    None => return crate::transport::close(&mut writer, &[]).await,
                }
            }
            crate::transport::close(&mut writer, &[]).await
        });
        let reader_signal = close_tx.clone();
        let mut reader_close = close.clone();
        let reader_task = tokio::spawn(async move {
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
        });
        let file_core = self.clone();
        let device = hello.device_id.clone();
        let file_writer = write_tx.clone();
        let file_close = close_tx.clone();
        let file_store = store.clone();
        let file_scope = action_scope.clone();
        let file_task = tokio::spawn(async move {
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
        });
        let text_core = self.clone();
        let device = hello.device_id.clone();
        let text_writer = write_tx.clone();
        let text_close = close_tx.clone();
        let text_store = store.clone();
        let text_task = tokio::spawn(async move {
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
        });
        let mut workers = ConnectionTasks {
            reader: reader_task,
            writer: writer_task,
            file: file_task,
            text: text_task,
        };
        let mut writer_finished = false;
        let mut applications_finished = false;
        let mut pending: Option<(i64, Id, Instant)> = None;
        let mut retry_at: Option<Instant> = None;
        let mut query_outgoing = true;
        let mut recovered_pairing = !allow_recovery;
        let mut outgoing_query = None;
        let result:Result<()>=async {
            loop {
                if close.borrow().is_some() || !self.connection_current(&store, &hello.device_id) {break;}
                if outgoing_query.is_none() && query_outgoing && pending.is_none()
                    && self.phone_paused(&hello.device_id)==Some(false)
                    && self.config().clipboard_sync_to_phone
                    && hello.capabilities.iter().any(|c|c=="clipboard_text") {
                    let store = store.clone();
                    let device = hello.device_id.clone();
                    outgoing_query = Some(Box::pin(async move { store.first_outgoing(device).await }));
                    query_outgoing = false;
                }
                let receipt_deadline=pending.as_ref().map(|(_,_,deadline)|*deadline).unwrap_or_else(||Instant::now()+Duration::from_secs(86400));
                tokio::select! {
                    biased;
                    _=close.changed()=>break,
                    writer=&mut workers.writer=>{writer_finished=true;if close.borrow().is_none() {writer??;}break;},
                    _=tokio::time::sleep_until(receipt_deadline),if pending.is_some()=>{let _=close_tx.send(Some(CloseReason::Error(ErrorCode::Timeout)));bail!("outgoing receipt timeout");},
                    _=wake.notified()=>query_outgoing=true,
                    _=config.changed()=>query_outgoing=true,
                    _=retry_timer(retry_at),if !query_outgoing=>{retry_at=None;query_outgoing=true;},
                    outgoing=async { outgoing_query.as_mut().expect("outgoing query").await },if outgoing_query.is_some()=>{
                        outgoing_query=None;retry_at=None;
                        if self.phone_paused(&hello.device_id)!=Some(false) || !self.config().clipboard_sync_to_phone {query_outgoing=true;continue;}
                        if query_outgoing { continue; }
                        if let Some((seq,text,next))=outgoing? {
                            if next as u64>crate::now_ms() {retry_at=Some(Instant::now()+Duration::from_millis((next as u64).saturating_sub(crate::now_ms())));}
                            else {
                                if !enqueue(&write_tx,Message::Clipboard(text.clone()),&mut close,Some(Instant::now()+Duration::from_secs(60))).await? {break;}
                                pending=Some((seq,text.transfer_id,Instant::now()+Duration::from_secs(120)));
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
                                self.forget_pairing(hello.device_id.clone(), CloseReason::Error(ErrorCode::Closing), store.connection_id).await?;break;
                            },
                            Message::PairRequest(_) if !recovered_pairing=>{
                                // TLS + HELLO binding were checked before READY.
                                // A consumed/expired token is irrelevant to this recovery.
                                recovered_pairing=true;
                                if !enqueue(&write_tx,Message::PairResult(PairResult {accepted:true,code:None,message:None}),&mut close,Some(receipt_deadline.min(Instant::now()+Duration::from_secs(60)))).await? {break;}
                            },
                            Message::Ack(ack)=>{
                                let Some((seq,id,deadline))=pending.take() else {bail!("unsolicited ACK");};ensure!(id==ack.transfer_id,"wrong ACK ID");
                                tokio::time::timeout_at(deadline,store.outgoing_receipt(hello.device_id.clone(),seq,false,true)).await??;
                                self.connection_event(&store,&hello.device_id,Event::ClipboardSent {device:hello.device_id.clone(),transfer_id:id});query_outgoing=true;
                            },
                            Message::Nack(nack)=>{
                                let Some((seq,id,deadline))=pending.take() else {bail!("unsolicited NACK");};ensure!(id==nack.transfer_id,"wrong NACK ID");
                                let blocked=!nack.retryable || matches!(nack.code,NackCode::HashMismatch|NackCode::SizeMismatch);
                                tokio::time::timeout_at(deadline,store.outgoing_receipt(hello.device_id.clone(),seq,blocked,false)).await??;
                                query_outgoing=true;
                                self.error(Some(hello.device_id.clone()),format!("clipboard delivery rejected: {:?}",nack.code));
                            },
                            m @ (Message::FileBegin(_)|Message::FileChunk(_)|Message::FileEnd(_))=>{if !enqueue(&file_tx,m,&mut close,pending.as_ref().map(|(_,_,deadline)|*deadline)).await? {break;}},
                            m @ Message::Clipboard(_)=>{if !enqueue(&text_tx,m,&mut close,pending.as_ref().map(|(_,_,deadline)|*deadline)).await? {break;}},
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
        if !writer_finished
            && tokio::time::timeout(Duration::from_secs(2), &mut workers.writer)
                .await
                .is_err()
        {
            workers.writer.abort();
            let _ = (&mut workers.writer).await;
        }
        result
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
                    deduplicated: false,
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
                    if let Some(file) = &mut state.file
                        && let Err(error) = file.write_all(&chunk.data).await
                    {
                        state.file.take();
                        state.receipt = Some(crate::store::save_failure_code(&error.into()));
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
                        store
                            .commit_file(
                                device.clone(),
                                verified,
                                state.stage.clone().context("missing staging handle")?,
                            )
                            .await
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
                        state.receipt = Some(code);
                    }
                }
                state.file.take();
                if let Some(stage) = state.stage.take() {
                    store.discard_stage(stage).await?;
                }
                if let Some(code) = state.receipt {
                    self.connection_event(
                        store,
                        device,
                        Event::Error {
                            device: Some(device.clone()),
                            message: format!(
                                "file transfer {} rejected: {code:?}",
                                end.transfer_id
                            ),
                        },
                    );
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
                            );
                        }
                    }
                    tx.send(Message::Ack(Ack {
                        transfer_id: end.transfer_id.clone(),
                        deduplicated: state.deduplicated,
                    }))
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
            let prefix =
                if state.begin.kind == FileKind::ClipboardImage || !state.config.save_to_folder {
                    "received-"
                } else {
                    ""
                };
            let destination = format!(
                "{prefix}{}-{}",
                crate::random_hex(16),
                safe_basename(&state.begin.file_name, 200)
            );
            let size = state.begin.size;
            let stage = store.create_stage(folder, destination, size).await?;
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
                .effect(&job.origin, || self.actions.notify_clipboard_failure())
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
            if let Some(Err(error)) = job
                .scope
                .effect(&job.origin, || {
                    self.actions.notify_received(
                        &job.file,
                        if image {
                            &job.metadata.mime
                        } else {
                            "application/octet-stream"
                        },
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
) {
    let mut cancelled = job.scope.cancelled.subscribe();
    if *cancelled.borrow() || *stop.borrow() {
        return;
    }
    tokio::select! { biased;
        _ = stop.changed() => {},
        _ = cancelled.changed() => {},
        result = tokio::time::timeout(Duration::from_secs(15), async {
            if matches!(action, DesktopAction::Viewer) { job.scope.effect(&job.origin, || core.actions.open_received(&job.file)).await.unwrap_or(Ok(())) }
            else { core.file_actions(job, image, action).await; Ok(()) }
        }) => {
            if !matches!(result, Ok(Ok(()))) {
                core.error(Some(job.origin.device.clone()), if matches!(action, DesktopAction::Viewer) { "viewer unavailable" } else { "desktop action timeout" });
            }
        }
    }
}
async fn action_loop(
    core: std::sync::Weak<Core>,
    jobs: Arc<ActionQueue>,
    mut stop: watch::Receiver<bool>,
) {
    let mut latest: Option<ActionJob> = None;
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
            run_action(
                &core,
                &latest.take().expect("latest image"),
                &mut stop,
                DesktopAction::Viewer,
                true,
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
        if let Some((job, action)) = notification {
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
            run_action(&core, &job, &mut stop, action, image).await;
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
struct Incoming {
    begin: FileBegin,
    received: u64,
    hash: Sha256,
    file: Option<tokio::fs::File>,
    stage: Option<Stage>,
    destination: Option<PathBuf>,
    receipt: Option<NackCode>,
    deduplicated: bool,
    config: Config,
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

fn safe_basename(name: &str, max: usize) -> String {
    let split = name
        .rsplit_once('.')
        .filter(|(stem, ext)| !stem.is_empty() && ext.len() < 32);
    let (stem, suffix) = split
        .map(|(stem, ext)| (stem, format!(".{ext}")))
        .unwrap_or((name, String::new()));
    let budget = max.saturating_sub(suffix.len());
    let mut end = stem.len().min(budget);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &stem[..end])
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
    async fn disk_full_on_flush_nacks_and_discards_received_copy() -> Result<()> {
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
    async fn canceled_end_flush_retains_cleanup_ownership() -> Result<()> {
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
        )
        .await?;
        let timed_out = tokio::time::timeout(
            Duration::from_millis(20),
            core.receive_test(
                &core.store,
                &device,
                Message::FileEnd(FileEnd {
                    transfer_id: begin.transfer_id.clone(),
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
            let error = tokio::time::timeout(Duration::from_secs(2), listening)
                .await??
                .unwrap_err();
            assert!(
                error.to_string().contains("Closing"),
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
