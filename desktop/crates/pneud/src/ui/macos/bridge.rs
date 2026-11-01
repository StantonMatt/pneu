//! Capability-authenticated daemon/UI extension of local IPC. The only endpoint
//! is an inherited, same-uid socketpair; readiness is revoked on EOF or failure.
use crate::ui::child_channel as transport;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::File,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Notification {
    pub id: u32,
    pub namespace: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>,
    pub revision: String,
    pub transient: bool,
    pub expires: i32,
    pub issued_at_ms: u64,
}
impl Notification {
    pub fn lifetime(&self) -> Option<Duration> {
        let millis = if self.expires > 0 {
            self.expires as u64
        } else if self.transient {
            4000
        } else {
            return None;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Some(Duration::from_millis(
            self.issued_at_ms.saturating_add(millis).saturating_sub(now),
        ))
    }
    pub fn identifier(&self) -> String {
        format!("pneu-{}-{}-{}", self.namespace, self.revision, self.id)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Request {
    PhoneNotification {
        id: String,
        notice: Option<crate::ui::notifications::PhoneNotice>,
    },
    WithdrawPhoneNotifications {
        ids: Vec<String>,
    },
    Notify(Notification),
    Open {
        name: String,
        preview: bool,
    },
    Reveal {
        paths: Vec<std::path::PathBuf>,
    },
    OpenUrl(String),
    Withdraw {
        id: u32,
        namespace: String,
        revision: String,
    },
    ClearStale {
        namespace: String,
    },
    Test(String),
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Frame {
    Hello {
        token: String,
    },
    Ready {
        token: String,
    },
    Request {
        id: u64,
        request: Request,
    },
    Reply {
        id: u64,
        error: Option<String>,
    },
    NotificationAction {
        id: u32,
        namespace: String,
        action: String,
    },
    Cancel {
        id: u64,
    },
}
struct PendingRequest {
    reply: Option<oneshot::Sender<Result<()>>>,
    deadline: tokio::time::Instant,
}
type Pending = Arc<Mutex<HashMap<u64, PendingRequest>>>;
type Outgoing = (Frame, Option<File>);
static ACTIVE: OnceLock<Mutex<Option<Arc<Session>>>> = OnceLock::new();
fn active() -> &'static Mutex<Option<Arc<Session>>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

pub(crate) struct Session {
    pub ready: Arc<AtomicBool>,
    output: mpsc::SyncSender<Outgoing>,
    pending: Pending,
    sequence: AtomicU64,
    socket: UnixStream,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    health: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Session {
    pub fn start(
        socket: UnixStream,
        token: String,
        events: tokio::sync::mpsc::Sender<(u32, String, String)>,
    ) -> Result<Arc<Self>> {
        transport::configure(&socket)?;
        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
        let (output, receiver) = mpsc::sync_channel::<Outgoing>(32);
        let pending: Pending = Arc::default();
        let ready = Arc::new(AtomicBool::new(false));
        let mut writer = socket.try_clone()?;
        let mut reader = socket.try_clone()?;
        let ready_write = ready.clone();
        let pending_write = pending.clone();
        let write_thread = std::thread::Builder::new()
            .name("pneu-ui-write".into())
            .spawn(move || {
                while let Ok((frame, file)) = receiver.recv() {
                    if transport::send(&mut writer, &frame, file.as_ref()).is_err() {
                        break;
                    }
                }
                ready_write.store(false, Ordering::Release);
                pending_write.lock().expect("UI replies").clear();
                let _ = writer.shutdown(Shutdown::Both);
            })?;
        let ready_read = ready.clone();
        let pending_read = pending.clone();
        let hello = token.clone();
        output.try_send((Frame::Hello { token: hello }, None))?;
        let read_thread = std::thread::Builder::new()
            .name("pneu-ui-read".into())
            .spawn(move || {
                let receive = || -> Result<()> {
                    let (frame, file) = transport::receive::<Frame>(&mut reader)?;
                    ensure!(file.is_none(), "unexpected child descriptor");
                    ensure!(
                        matches!(frame, Frame::Ready {token: ref value} if value == &token),
                        "UI handshake failed"
                    );
                    ready_read.store(true, Ordering::Release);
                    loop {
                        let (frame, file) = transport::receive::<Frame>(&mut reader)?;
                        ensure!(file.is_none(), "unexpected child descriptor");
                        match frame {
                            Frame::Reply { id, error } => {
                                if let Some(reply) =
                                    pending_read.lock().expect("UI replies").remove(&id)
                                    && let Some(reply) = reply.reply
                                {
                                    let _ = reply
                                        .send(error.map_or(Ok(()), |e| Err(anyhow::anyhow!(e))));
                                }
                            }
                            Frame::NotificationAction {
                                id,
                                namespace,
                                action,
                            } => {
                                events
                                    .try_send((id, namespace, action))
                                    .context("UI action queue full")?;
                            }
                            _ => anyhow::bail!("unexpected child frame"),
                        }
                    }
                };
                if let Err(error) = {
                    let mut receive = receive;
                    receive()
                } {
                    eprintln!("pneud: UI channel: {error:#}");
                }
                ready_read.store(false, Ordering::Release);
                pending_read.lock().expect("UI replies").clear();
                let _ = reader.shutdown(Shutdown::Both);
            })?;
        let session = Arc::new(Self {
            ready,
            output,
            pending,
            sequence: AtomicU64::new(1),
            socket,
            threads: Mutex::new(vec![write_thread, read_thread]),
            health: Mutex::new(None),
        });
        let weak = Arc::downgrade(&session);
        *session.health.lock().expect("UI health") = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let Some(session) = weak.upgrade() else {
                    break;
                };
                if session
                    .pending
                    .lock()
                    .expect("UI replies")
                    .values()
                    .any(|p| tokio::time::Instant::now() >= p.deadline)
                {
                    session.ready.store(false, Ordering::Release);
                    let _ = session.socket.shutdown(Shutdown::Both);
                    session.pending.lock().expect("UI replies").clear();
                    break;
                }
            }
        }));
        Ok(session)
    }
    #[cfg(target_os = "macos")]
    pub fn activate(self: &Arc<Self>) {
        *active().lock().expect("UI session") = Some(self.clone());
    }
    pub fn close(&self) {
        if let Some(health) = self.health.lock().expect("UI health").take() {
            health.abort();
        }
        self.ready.store(false, Ordering::Release);
        let _ = self.socket.shutdown(Shutdown::Both);
        self.pending.lock().expect("UI replies").clear();
        // Wake the idle writer after shutdown. A full queue is already awake.
        let _ = self.output.try_send((Frame::Cancel { id: 0 }, None));
        for thread in self.threads.lock().expect("UI threads").drain(..) {
            let _ = thread.join();
        }
    }
}
pub(crate) fn require_ui() -> Result<()> {
    ensure!(
        active()
            .lock()
            .expect("UI session")
            .as_ref()
            .is_some_and(|s| s.ready.load(Ordering::Acquire)),
        "Pneu's menu bar is unavailable; waiting for the UI child"
    );
    Ok(())
}
/// Reserve and enqueue promptly. The acknowledgement wait is an Interaction,
/// so viewer/permission UI never holds Core's action or binding gates.
#[cfg(target_os = "macos")]
pub(crate) fn begin(
    request: Request,
    file: Option<File>,
) -> Result<crate::ui::portal::Interaction> {
    require_ui()?;
    let session = active()
        .lock()
        .expect("UI session")
        .clone()
        .context("UI unavailable")?;
    session.begin(request, file)
}
impl Session {
    pub fn begin(
        self: &Arc<Self>,
        request: Request,
        file: Option<File>,
    ) -> Result<crate::ui::portal::Interaction> {
        ensure!(self.ready.load(Ordering::Acquire), "UI child is not ready");
        let session = self.clone();
        let deadline = tokio::time::Instant::now()
            + if matches!(request, Request::Open { .. }) {
                Duration::from_secs(120)
            } else {
                Duration::from_secs(5)
            };
        let id = session.sequence.fetch_add(1, Ordering::Relaxed);
        let mirror = matches!(
            request,
            Request::PhoneNotification {
                notice: Some(_),
                ..
            }
        );
        let (sender, reply) = oneshot::channel();
        {
            let mut pending = session.pending.lock().expect("UI replies");
            ensure!(pending.len() < 32, "UI request queue full");
            pending.insert(
                id,
                PendingRequest {
                    reply: Some(sender),
                    deadline,
                },
            );
        }
        if let Err(error) = session
            .output
            .try_send((Frame::Request { id, request }, file))
        {
            session.pending.lock().expect("UI replies").remove(&id);
            anyhow::bail!("UI request queue unavailable: {error}");
        }
        struct Guard {
            session: Arc<Session>,
            id: u64,
            mirror: bool,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                // A dropped caller still leaves a health deadline. Only a
                // child acknowledgement (including cancellation) settles it.
                let pending = self
                    .session
                    .pending
                    .lock()
                    .expect("UI replies")
                    .get_mut(&self.id)
                    .and_then(|p| p.reply.take())
                    .is_some();
                // Mirrored notices remain cancellation-owned even after the
                // channel reader has consumed their successful acknowledgement.
                // Only the worker accepting that result transfers ownership.
                if (pending || self.mirror)
                    && self
                        .session
                        .output
                        .try_send((Frame::Cancel { id: self.id }, None))
                        .is_err()
                {
                    self.session.ready.store(false, Ordering::Release);
                    let _ = self.session.socket.shutdown(Shutdown::Both);
                }
            }
        }
        let guard = Guard {
            session,
            id,
            mirror,
        };
        Ok(Box::pin(async move {
            let mut guard = guard;
            let result = reply
                .await
                .context("UI acknowledgement timed out or child exited")?;
            if result.is_ok() {
                guard.mirror = false;
            }
            result
        }))
    }
}

#[cfg(target_os = "macos")]
fn child_requests() -> &'static Mutex<HashMap<u64, Arc<crate::ui::notice_lifecycle::Lifecycle>>> {
    static REQUESTS: OnceLock<Mutex<HashMap<u64, Arc<crate::ui::notice_lifecycle::Lifecycle>>>> =
        OnceLock::new();
    REQUESTS.get_or_init(Mutex::default)
}
#[cfg(target_os = "macos")]
pub(crate) struct Child {
    pub writer: Arc<Mutex<UnixStream>>,
    thread: Option<std::thread::JoinHandle<()>>,
    token: Option<String>,
    _lease: Option<File>,
}
#[cfg(target_os = "macos")]
impl Child {
    pub fn start() -> Result<Self> {
        use std::os::fd::FromRawFd;
        let lease = if let Some(lease_fd) = std::env::var_os("PNEU_UI_VIEW_LEASE_FD") {
            let lease_fd = lease_fd
                .to_str()
                .context("invalid viewer lease fd")?
                .parse::<i32>()?;
            ensure!(lease_fd >= 3, "invalid viewer lease descriptor");
            let lease = unsafe { File::from_raw_fd(lease_fd) };
            ensure!(
                unsafe { libc::fcntl(lease_fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
                "lease CLOEXEC failed"
            );
            crate::platform::private::check(&lease)?;
            Some(lease)
        } else {
            None
        };
        let fd = std::env::var("PNEU_UI_FD")
            .context("ui-window must be supervised by pneud run")?
            .parse::<i32>()?;
        ensure!(fd >= 3, "invalid UI channel descriptor");
        let token = std::env::var("PNEU_UI_TOKEN").context("missing UI capability")?;
        ensure!(token.len() == 64, "invalid UI capability");
        // The descriptor is passed only by the daemon's pre_exec hook.
        let socket = unsafe { UnixStream::from_raw_fd(fd) };
        transport::configure(&socket)?;
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "UI channel CLOEXEC failed"
        );
        socket.set_read_timeout(Some(Duration::from_secs(10)))?;
        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut reader = socket.try_clone()?;
        let (hello, file) = transport::receive::<Frame>(&mut reader)?;
        ensure!(
            file.is_none() && matches!(hello, Frame::Hello {token: ref value} if value == &token),
            "daemon handshake failed"
        );
        socket.set_read_timeout(None)?;
        let writer = Arc::new(Mutex::new(socket));
        let output = writer.clone();
        let thread = std::thread::Builder::new()
            .name("pneu-ui-channel".into())
            .spawn(move || {
                while let Ok((frame, file)) = transport::receive::<Frame>(&mut reader) {
                    match frame {
                        Frame::Request { id, request } => {
                            let lifetime = match &request {
                                Request::Notify(n) => n.lifetime(),
                                _ => None,
                            };
                            let life =
                                Arc::new(crate::ui::notice_lifecycle::Lifecycle::new(lifetime));
                            {
                                let mut pending = child_requests().lock().expect("child requests");
                                if pending.len() >= 32 {
                                    break;
                                }
                                pending.insert(id, life.clone());
                            }
                            let output = output.clone();
                            let guard = crate::ui::notice_lifecycle::RequestGuard::new(
                                life,
                                move |result| {
                                    Child::reply(&output, id, result);
                                },
                            );
                            if slint::invoke_from_event_loop(move || {
                                crate::ui::macos::native::request(id, request, file, guard)
                            })
                            .is_err()
                            {
                                break;
                            }
                        }
                        Frame::Cancel { id } if file.is_none() => {
                            if let Some(life) =
                                child_requests().lock().expect("child requests").get(&id)
                            {
                                life.cancel();
                            }
                            // ACK on the channel thread even if main-thread cleanup is queued.
                            Child::reply(&output, id, Err(anyhow::anyhow!("UI request cancelled")));
                            let _ = slint::invoke_from_event_loop(move || {
                                crate::ui::macos::native::cancel_request(id);
                            });
                        }
                        _ => break,
                    }
                }
                for (_, life) in child_requests().lock().expect("child requests").drain() {
                    life.cancel();
                }
                let _ = slint::quit_event_loop();
            })?;
        Ok(Self {
            writer,
            thread: Some(thread),
            token: Some(token),
            _lease: lease,
        })
    }
    pub fn ready(&mut self) -> Result<()> {
        if let Some(token) = self.token.take() {
            transport::send(
                &mut self.writer.lock().expect("UI writer"),
                &Frame::Ready { token },
                None,
            )?;
        }
        Ok(())
    }
    pub fn reply(writer: &Arc<Mutex<UnixStream>>, id: u64, result: Result<()>) {
        // Cancellation and a late native completion may race. Only the first
        // completion consumes the request and emits a wire acknowledgement.
        if child_requests()
            .lock()
            .expect("child requests")
            .remove(&id)
            .is_none()
        {
            return;
        }
        let error = result.err().map(|e| format!("{e:#}"));
        let _ = transport::send(
            &mut writer.lock().expect("UI writer"),
            &Frame::Reply { id, error },
            None,
        );
    }
    pub fn action(&self, id: u32, namespace: String, action: String) {
        let _ = transport::send(
            &mut self.writer.lock().expect("UI writer"),
            &Frame::NotificationAction {
                id,
                namespace,
                action,
            },
            None,
        );
    }
}
#[cfg(target_os = "macos")]
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self
            .writer
            .lock()
            .expect("UI writer")
            .shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn cancellation_and_late_native_completion_emit_one_ack() -> Result<()> {
        let _serial = SERIAL.lock().await;
        let (writer, mut reader) = UnixStream::pair()?;
        reader.set_read_timeout(Some(Duration::from_millis(50)))?;
        let writer = Arc::new(Mutex::new(writer));
        let id = u64::MAX - 1;
        let life = Arc::new(crate::ui::notice_lifecycle::Lifecycle::new(None));
        child_requests().lock().unwrap().insert(id, life.clone());
        let output = writer.clone();
        let guard = crate::ui::notice_lifecycle::RequestGuard::new(life.clone(), move |r| {
            Child::reply(&output, id, r);
        });
        life.cancel();
        Child::reply(&writer, id, Err(anyhow::anyhow!("cancelled")));
        guard.finish(Ok(()));
        drop(guard);
        let (frame, _) = transport::receive::<Frame>(&mut reader)?;
        assert!(matches!(frame, Frame::Reply { id: reply_id, error: Some(_) } if reply_id == id));
        assert!(transport::receive::<Frame>(&mut reader).is_err());
        Ok(())
    }
    fn handshake(child: &mut UnixStream, token: &str) -> Result<()> {
        let (hello, file) = transport::receive::<Frame>(child)?;
        ensure!(
            file.is_none() && matches!(hello,Frame::Hello {token: ref value} if value==token),
            "daemon hello"
        );
        transport::send(
            child,
            &Frame::Ready {
                token: token.into(),
            },
            None,
        )
    }
    async fn ready(session: &Session) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session.ready.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    pub(crate) fn test_notice() -> Notification {
        Notification {
            id: 1,
            namespace: "session".into(),
            revision: "revision".into(),
            summary: "Pneu test".into(),
            body: String::new(),
            actions: Vec::new(),
            transient: true,
            expires: 4000,
            issued_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }
    }
    #[test]
    fn notification_wire_preserves_expiry_and_distinguishes_replacements() -> Result<()> {
        let notice = test_notice();
        let wire = serde_json::to_vec(&notice)?;
        let decoded: Notification = serde_json::from_slice(&wire)?;
        assert!(decoded.transient);
        assert_eq!(decoded.expires, 4000);
        assert!(decoded.lifetime().unwrap() <= Duration::from_secs(4));
        let mut replacement = decoded.clone();
        replacement.revision = "new".into();
        assert_ne!(decoded.identifier(), replacement.identifier());
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn caller_timeout_does_not_disarm_child_health_deadline() -> Result<()> {
        let _serial = SERIAL.lock().await;
        let (parent, mut child) = UnixStream::pair()?;
        let (events, _) = tokio::sync::mpsc::channel(2);
        let session = Session::start(parent, "e".repeat(64), events)?;
        handshake(&mut child, &"e".repeat(64))?;
        ready(&session).await;
        let wait = session.begin(Request::Notify(test_notice()), None)?;
        let _: (Frame, _) = transport::receive(&mut child)?;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), wait)
                .await
                .is_err()
        );
        let (frame, _) = transport::receive::<Frame>(&mut child)?;
        assert!(matches!(frame, Frame::Cancel { .. }));
        assert_eq!(session.pending.lock().unwrap().len(), 1);
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert!(!session.ready.load(Ordering::Acquire));
        assert!(session.begin(Request::Test("ping".into()), None).is_err());
        session.close();
        Ok(())
    }
    #[tokio::test]
    async fn phone_notifications_bulk_withdrawal_fits_one_bridge_slot() -> Result<()> {
        let _serial = SERIAL.lock().await;
        let (parent, mut child) = UnixStream::pair()?;
        let (events, _) = tokio::sync::mpsc::channel(2);
        let session = Session::start(parent, "b".repeat(64), events)?;
        handshake(&mut child, &"b".repeat(64))?;
        ready(&session).await;
        let mut waits = Vec::new();
        // Keep 31 requests outstanding, leaving exactly one admission slot.
        for _ in 0..31 {
            waits.push(session.begin(Request::Test("busy".into()), None)?);
            let _: (Frame, _) = transport::receive(&mut child)?;
        }
        let ids: Vec<_> = (0..128).map(|key| format!("session-{key}")).collect();
        let wait = session.begin(
            Request::WithdrawPhoneNotifications { ids: ids.clone() },
            None,
        )?;
        let (
            Frame::Request {
                id,
                request: Request::WithdrawPhoneNotifications { ids: received },
            },
            file,
        ) = transport::receive::<Frame>(&mut child)?
        else {
            panic!("bulk withdrawal request")
        };
        assert!(file.is_none());
        assert_eq!(received, ids);
        assert_eq!(session.pending.lock().unwrap().len(), 32);
        transport::send(&mut child, &Frame::Reply { id, error: None }, None)?;
        wait.await?;
        session.close();
        drop(waits);
        Ok(())
    }
    #[tokio::test]
    async fn phone_notifications_unobserved_success_still_cancels_native_notice() -> Result<()> {
        let _serial = SERIAL.lock().await;
        let (parent, mut child) = UnixStream::pair()?;
        child.set_read_timeout(Some(Duration::from_secs(1)))?;
        let (events, _) = tokio::sync::mpsc::channel(2);
        let session = Session::start(parent, "f".repeat(64), events)?;
        handshake(&mut child, &"f".repeat(64))?;
        ready(&session).await;
        let wait = session.begin(
            Request::PhoneNotification {
                id: "session-key".into(),
                notice: Some(crate::ui::notifications::PhoneNotice {
                    app: "Chat".into(),
                    title: None,
                    text: None,
                    icon: None,
                }),
            },
            None,
        )?;
        let (Frame::Request { id, .. }, _) = transport::receive::<Frame>(&mut child)? else {
            panic!("phone request")
        };
        transport::send(&mut child, &Frame::Reply { id, error: None }, None)?;
        // Force the reader to consume success before its owner is disconnected.
        tokio::time::timeout(Duration::from_secs(2), async {
            while session.pending.lock().unwrap().contains_key(&id) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        drop(wait);
        let cancelled = transport::receive::<Frame>(&mut child);
        session.close();
        assert!(matches!(cancelled?, (Frame::Cancel { id: cancelled }, None) if cancelled == id));
        Ok(())
    }
    #[tokio::test]
    async fn child_channel_authenticates_acknowledges_and_attributes_actions() -> Result<()> {
        let _serial = SERIAL.lock().await;
        let (parent, mut child) = UnixStream::pair()?;
        let (events, mut actions) = tokio::sync::mpsc::channel(2);
        let session = Session::start(parent, "a".repeat(64), events)?;
        assert!(require_ui().is_err());
        handshake(&mut child, &"a".repeat(64))?;
        ready(&session).await;
        assert!(session.ready.load(Ordering::Acquire));
        let file = tempfile::tempfile()?;
        let wait = session.begin(
            Request::Open {
                name: "test.txt".into(),
                preview: false,
            },
            Some(file),
        )?;
        let (frame, file) = transport::receive::<Frame>(&mut child)?;
        let Frame::Request {
            id,
            request: Request::Open { name, .. },
        } = frame
        else {
            panic!("open request")
        };
        assert_eq!(name, "test.txt");
        assert!(file.is_some());
        transport::send(
            &mut child,
            &Frame::Reply {
                id,
                error: Some("viewer refused".into()),
            },
            None,
        )?;
        assert_eq!(wait.await.unwrap_err().to_string(), "viewer refused");
        transport::send(
            &mut child,
            &Frame::NotificationAction {
                id: 42,
                namespace: "test-epoch".into(),
                action: "open".into(),
            },
            None,
        )?;
        assert_eq!(
            actions.recv().await,
            Some((42, "test-epoch".into(), "open".into()))
        );
        let wait = session.begin(Request::Test("ping".into()), None)?;
        let (frame, _) = transport::receive::<Frame>(&mut child)?;
        let Frame::Request { id, .. } = frame else {
            panic!("request")
        };
        drop(wait);
        let (frame, file) = transport::receive::<Frame>(&mut child)?;
        assert!(matches!(frame,Frame::Cancel {id:value} if value==id));
        assert!(file.is_none());
        let wait = session.begin(Request::Test("ping".into()), None)?;
        drop(child);
        assert!(wait.await.is_err());
        session.close();
        assert!(require_ui().is_err());
        Ok(())
    }
    #[tokio::test]
    async fn wrong_capability_never_enables_ui_and_restarts_get_fresh_authority() -> Result<()> {
        let _serial = SERIAL.lock().await;
        let (parent, mut child) = UnixStream::pair()?;
        let (events, _) = tokio::sync::mpsc::channel(2);
        let session = Session::start(parent, "a".repeat(64), events)?;
        let _: (Frame, _) = transport::receive(&mut child)?;
        transport::send(
            &mut child,
            &Frame::Ready {
                token: "b".repeat(64),
            },
            None,
        )?;
        drop(child);
        session.close();
        assert!(require_ui().is_err());
        let (parent, mut child) = UnixStream::pair()?;
        let (events, _) = tokio::sync::mpsc::channel(2);
        let replacement = Session::start(parent, "c".repeat(64), events)?;
        handshake(&mut child, &"c".repeat(64))?;
        ready(&replacement).await;
        assert!(replacement.ready.load(Ordering::Acquire));
        replacement.close();
        assert!(require_ui().is_err());
        Ok(())
    }
    #[test]
    fn fake_ui_child() -> Result<()> {
        if std::env::var_os("PNEU_UI_FD").is_none() {
            return Ok(());
        }
        use std::os::fd::FromRawFd;
        let fd = std::env::var("PNEU_UI_FD")?.parse::<i32>()?;
        ensure!(fd >= 3, "invalid child descriptor");
        let mut socket = unsafe { UnixStream::from_raw_fd(fd) };
        transport::configure(&socket)?;
        let token = std::env::var("PNEU_UI_TOKEN")?;
        handshake(&mut socket, &token)?;
        while let Ok((frame, file)) = transport::receive::<Frame>(&mut socket) {
            ensure!(file.is_none(), "fake child has no file reader");
            if let Frame::Request { id, .. } = frame {
                transport::send(&mut socket, &Frame::Reply { id, error: None }, None)?;
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn killing_ui_child_revokes_readiness_and_a_replacement_acknowledges() -> Result<()> {
        let _serial = SERIAL.lock().await;
        let executable = std::env::current_exe()?;
        let args = [
            "--exact",
            "ui::child_bridge::tests::fake_ui_child",
            "--nocapture",
        ];
        let (mut process, socket, token) =
            crate::ui::child_supervision::spawn(&executable, &args, 0, None)?;
        let old_id = process.id();
        let (events, _) = tokio::sync::mpsc::channel(2);
        let session = Session::start(socket, token.clone(), events)?;
        ready(&session).await;
        session.begin(Request::Test("ping".into()), None)?.await?;
        process.kill().await?;
        process.wait().await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            while session.ready.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(require_ui().is_err());
        session.close();
        let (mut process, socket, new_token) =
            crate::ui::child_supervision::spawn(&executable, &args, 0, None)?;
        assert_ne!(old_id, process.id());
        assert_ne!(token, new_token);
        let (events, _) = tokio::sync::mpsc::channel(2);
        let replacement = Session::start(socket, new_token, events)?;
        ready(&replacement).await;
        replacement
            .begin(Request::Test("ping".into()), None)?
            .await?;
        replacement.close();
        process.kill().await?;
        process.wait().await?;
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn missing_acknowledgement_revokes_ui_readiness() -> Result<()> {
        let _serial = SERIAL.lock().await;
        let (parent, mut child) = UnixStream::pair()?;
        let (events, _) = tokio::sync::mpsc::channel(2);
        let session = Session::start(parent, "d".repeat(64), events)?;
        handshake(&mut child, &"d".repeat(64))?;
        ready(&session).await;
        let wait = session.begin(Request::Test("ping".into()), None)?;
        let _: (Frame, _) = transport::receive(&mut child)?;
        assert!(
            wait.await
                .unwrap_err()
                .to_string()
                .contains("acknowledgement timed out")
        );
        assert!(!session.ready.load(Ordering::Acquire));
        assert!(session.begin(Request::Test("ping".into()), None).is_err());
        session.close();
        Ok(())
    }
}
