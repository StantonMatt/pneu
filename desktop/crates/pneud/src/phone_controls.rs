//! RAM-only state, fenced by the live authenticated connection. No outbox lane.
use super::*;

const BATTERY_TTL: Duration = Duration::from_secs(10 * 60);
const RING_RESPONSE: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct RingAuthority {
    device: Id,
    binding: u64,
    connection: u64,
}
/// Keep the physical writer's authority with its queue, including when normal
/// frames are drained between file/image chunks.
pub(super) struct FrameQueue {
    messages: mpsc::Receiver<Message>,
    ring: Option<RingAuthority>,
}
impl From<mpsc::Receiver<Message>> for FrameQueue {
    fn from(messages: mpsc::Receiver<Message>) -> Self {
        Self {
            messages,
            ring: None,
        }
    }
}
impl FrameQueue {
    pub(super) fn new(messages: mpsc::Receiver<Message>, hello: &Hello, store: &Store) -> Self {
        Self {
            messages,
            ring: store
                .pairing_generation(&hello.device_id)
                .zip(store.connection_id)
                .map(|(binding, connection)| RingAuthority {
                    device: hello.device_id.clone(),
                    binding,
                    connection,
                }),
        }
    }
    pub(super) async fn recv(&mut self) -> Option<Message> {
        self.messages.recv().await
    }
    pub(super) fn try_recv(&mut self) -> std::result::Result<Message, mpsc::error::TryRecvError> {
        self.messages.try_recv()
    }
    pub(super) async fn send<W: tokio::io::AsyncWrite + Unpin>(
        &self,
        core: &Core,
        writer: &mut W,
        message: &Message,
    ) -> Result<()> {
        let Message::Ring(ring) = message else {
            return send(writer, message).await;
        };
        let authority = self.ring.as_ref().context("Ring writer has no authority")?;
        let mut guarded = RingWriter {
            core,
            writer,
            authority,
            action: ring.action,
            started: false,
            canceled: false,
        };
        let result = send(&mut guarded, message).await;
        if guarded.canceled {
            // No frame byte was accepted; the stream remains usable.
            return Ok(());
        }
        result
    }
}
struct RingWriter<'a, W> {
    core: &'a Core,
    writer: &'a mut W,
    authority: &'a RingAuthority,
    action: RingAction,
    started: bool,
    canceled: bool,
}
impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for RingWriter<'_, W> {
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
        if !this
            .core
            .ring_current_locked(&sessions, this.authority, this.action)
        {
            this.canceled = true;
            return std::task::Poll::Ready(Err(std::io::Error::other("Ring authority changed")));
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
#[derive(Default)]
pub(super) struct Controls(HashMap<Id, Phone>);
struct Phone {
    binding: u64,
    connection: u64,
    ring_supported: bool,
    battery: Option<(BatteryState, Instant)>,
    ringing: bool,
    pending: Option<(RingAction, Instant)>,
    writer: Option<mpsc::Sender<Message>>,
}
pub(super) fn eligible(hello: &Hello, capability: &str) -> bool {
    hello.role == Some(DeviceRole::Phone)
        && hello.capabilities.iter().any(|c| c == "mesh")
        && (capability == CAP_BATTERY || hello.capabilities.iter().any(|c| c == capability))
}
impl Phone {
    fn clear(&mut self) {
        self.battery = None;
        self.ringing = false;
        self.pending = None;
    }
    fn presentation(&self, connected: bool) -> crate::ui::PhoneControl {
        let sample = self
            .battery
            .as_ref()
            .filter(|(_, at)| connected && at.elapsed() < BATTERY_TTL);
        crate::ui::PhoneControl {
            battery: sample.and_then(|(s, _)| s.level),
            charging: sample.and_then(|(s, _)| s.charging).unwrap_or(false),
            ring_supported: self.ring_supported,
            ringing: connected && self.ringing,
            connection: connected.then_some(self.connection),
        }
    }
}
impl Core {
    fn ring_current_locked(
        &self,
        sessions: &Connections,
        authority: &RingAuthority,
        action: RingAction,
    ) -> bool {
        !sessions.unpairing.contains(&authority.device)
            && self.store.pairing_generation(&authority.device) == Some(authority.binding)
            && sessions.sessions.get(&authority.device).is_some_and(|s| {
                s.id == authority.connection
                    && s.role == "phone"
                    && s.mesh
                    && (action == RingAction::Stop || !s.paused)
                    && s.close.borrow().is_none()
                    && !s.close.is_closed()
                    && s.capabilities.iter().any(|c| c == CAP_RING)
            })
    }
    pub(super) fn attach_phone_control(
        &self,
        hello: &Hello,
        store: &Store,
        writer: mpsc::Sender<Message>,
    ) {
        // Lock in the same order as session teardown and snapshots.
        let sessions = self.sessions.lock().expect("sessions");
        let Some(session) = sessions
            .sessions
            .get(&hello.device_id)
            .filter(|s| Some(s.id) == store.connection_id)
        else {
            return;
        };
        if !eligible(hello, CAP_BATTERY) {
            self.phone_controls
                .lock()
                .expect("phone controls")
                .0
                .remove(&hello.device_id);
            return;
        }
        let Some(binding) = store.pairing_generation(&hello.device_id) else {
            return;
        };
        self.phone_controls
            .lock()
            .expect("phone controls")
            .0
            .insert(
                hello.device_id.clone(),
                Phone {
                    binding,
                    connection: session.id,
                    ring_supported: eligible(hello, CAP_RING),
                    battery: None,
                    ringing: false,
                    pending: None,
                    writer: Some(writer),
                },
            );
    }
    pub(super) fn clear_phone_control(&self, device: &Id, connection: u64) {
        if let Some(phone) = self
            .phone_controls
            .lock()
            .expect("phone controls")
            .0
            .get_mut(device)
            .filter(|p| p.connection == connection)
        {
            phone.clear();
        }
    }
    pub(super) fn end_phone_control(&self, device: &Id, connection: u64) {
        if let Some(phone) = self
            .phone_controls
            .lock()
            .expect("phone controls")
            .0
            .get_mut(device)
            .filter(|p| p.connection == connection)
        {
            phone.clear();
            phone.writer = None;
        }
    }
    pub(super) fn phone_control_snapshot(
        &self,
        sessions: &Connections,
        admission: &Admission,
    ) -> HashMap<Id, crate::ui::PhoneControl> {
        let mut controls = self.phone_controls.lock().expect("phone controls");
        controls.0.retain(|device, phone| {
            admission.peers.values().any(|id| id == device)
                && self.store.pairing_generation(device) == Some(phone.binding)
        });
        controls
            .0
            .iter_mut()
            .map(|(id, phone)| {
                let connected = sessions.sessions.get(id).is_some_and(|s| {
                    s.id == phone.connection
                        && !s.paused
                        && s.close.borrow().is_none()
                        && !s.close.is_closed()
                });
                if !connected {
                    phone.clear();
                }
                if sessions
                    .sessions
                    .get(id)
                    .is_none_or(|s| s.id != phone.connection)
                {
                    phone.writer = None;
                }
                (id.clone(), phone.presentation(connected))
            })
            .collect()
    }
    pub(super) fn receive_phone_battery(&self, store: &Store, hello: &Hello, sample: BatteryState) {
        let sessions = self.sessions.lock().expect("sessions");
        let Some(session) = sessions
            .sessions
            .get(&hello.device_id)
            .filter(|s| Some(s.id) == store.connection_id && s.close.borrow().is_none())
        else {
            return;
        };
        if let Some(phone) = self
            .phone_controls
            .lock()
            .expect("phone controls")
            .0
            .get_mut(&hello.device_id)
            .filter(|p| p.connection == session.id)
        {
            phone.battery =
                (!session.paused && sample.level.is_some()).then_some((sample, Instant::now()));
        }
        self.event(Event::ConfigChanged);
    }
    pub(super) fn receive_phone_ring(&self, store: &Store, hello: &Hello, state: RingState) {
        let sessions = self.sessions.lock().expect("sessions");
        let Some(session) = sessions
            .sessions
            .get(&hello.device_id)
            .filter(|s| Some(s.id) == store.connection_id && s.close.borrow().is_none())
        else {
            return;
        };
        if let Some(phone) = self
            .phone_controls
            .lock()
            .expect("phone controls")
            .0
            .get_mut(&hello.device_id)
            .filter(|p| p.connection == session.id)
        {
            phone.ringing = !session.paused && state.ringing;
            // A false snapshot is also a useful reply (the phone may deny a start).
            phone.pending = None;
        }
        self.event(Event::ConfigChanged);
    }
    pub(crate) fn ring_phone(&self, device: Id, connection: u64, action: RingAction) -> Result<()> {
        let sessions = self.sessions.lock().expect("sessions");
        let authority = RingAuthority {
            device: device.clone(),
            binding: self
                .store
                .pairing_generation(&device)
                .context("Phone isn't paired")?,
            connection,
        };
        ensure!(
            self.ring_current_locked(&sessions, &authority, action),
            "Phone isn't connected or doesn't allow ringing"
        );
        let session = sessions
            .sessions
            .get(&device)
            .context("Phone isn't connected or doesn't allow ringing")?;
        let mut controls = self.phone_controls.lock().expect("phone controls");
        let phone = controls
            .0
            .get_mut(&device)
            .filter(|p| {
                p.connection == connection
                    && Some(p.binding) == self.store.pairing_generation(&device)
            })
            .context("Phone connection changed")?;
        // A bounded live writer only. Never defer to reconnect, retry, or persist.
        phone
            .writer
            .as_ref()
            .context("Phone isn't connected")?
            .try_send(Message::Ring(Ring { action }))
            .context("Phone connection is busy")?;
        phone.ringing = action == RingAction::Start;
        phone.pending = Some((action, Instant::now() + RING_RESPONSE));
        session.wake.notify_one();
        self.event(Event::ConfigChanged);
        Ok(())
    }
    pub(super) async fn expire_phone_control(&self, hello: &Hello, store: &Store) {
        let failure = {
            let sessions = self.sessions.lock().expect("sessions");
            let Some(session) = sessions
                .sessions
                .get(&hello.device_id)
                .filter(|s| Some(s.id) == store.connection_id && s.close.borrow().is_none())
            else {
                return;
            };
            let mut controls = self.phone_controls.lock().expect("phone controls");
            let Some(phone) = controls
                .0
                .get_mut(&hello.device_id)
                .filter(|p| p.connection == session.id)
            else {
                return;
            };
            if phone
                .battery
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= BATTERY_TTL)
            {
                phone.battery = None;
                self.event(Event::ConfigChanged);
            }
            if let Some((action, deadline)) = phone.pending
                && deadline <= Instant::now()
            {
                phone.pending = None;
                phone.ringing = false;
                self.event(Event::ConfigChanged);
                action == RingAction::Start
            } else {
                false
            }
        };
        if failure {
            let name = crate::ui::window::display_peer_text(&hello.name);
            if self.window_visible() {
                let mut ui = self.ui.lock().expect("UI state");
                let revision = ui
                    .snapshot
                    .ring_failure
                    .as_ref()
                    .map_or(1, |f| f.revision.wrapping_add(1));
                ui.snapshot.ring_failure = Some(crate::ui::RingFailure {
                    request: self.window_request(),
                    revision,
                    message: format!("{name} didn't respond"),
                });
            } else if let Err(error) = self.actions.notify_ring_failure(&name).await {
                self.error(
                    None,
                    format!("ring failure notification unavailable: {error}"),
                );
            }
        }
    }
}
