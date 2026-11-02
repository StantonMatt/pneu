//! Computer links. Peer address hints supply bounded routes, never authority.
use super::*;
use crate::store::PeerInfo;
use rand::Rng;
use serde_json::{Value, json};
use tokio_rustls::TlsConnector;

#[derive(Clone, Copy, PartialEq, Eq)]
enum JoinPhase {
    Preview,
    Pending,
    Finished,
}
#[derive(Clone)]
struct JoinAuthority {
    peer: Id,
    generation: Option<u64>,
    admission: Option<Id>,
}
#[derive(Clone)]
struct Join {
    attempt: String,
    uri: PairingUri,
    state: String,
    phase: JoinPhase,
    authority: Option<JoinAuthority>,
    requested: bool,
    installed_generation: Arc<AtomicU64>,
    retry_at: Option<Instant>,
    ready: Arc<AtomicBool>,
    setup_scope: bool,
}
#[cfg(test)]
type ConfirmHook = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(test)]
type RequestHook = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[derive(Default)]
pub(super) struct Mesh {
    peers: Mutex<Vec<PeerInfo>>,
    join: Mutex<Option<Join>>,
    gate: tokio::sync::Mutex<()>,
    pub(super) changed: Arc<Notify>,
    links: Mutex<HashMap<Id, LinkState>>,
    connecting: Mutex<HashSet<Id>>,
    #[cfg(test)]
    panic_join: AtomicBool,
    #[cfg(test)]
    panic_dial: AtomicBool,
    #[cfg(test)]
    panic_supervisor: AtomicBool,
    #[cfg(test)]
    confirm_hook: Mutex<Option<ConfirmHook>>,
    #[cfg(test)]
    retry_hook: Mutex<Option<ConfirmHook>>,
    #[cfg(test)]
    request_hook: Mutex<Option<RequestHook>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LinkProblem {
    UpdateRequired,
    FingerprintMismatch,
    BindingMismatch,
    Unauthorized,
}
impl LinkProblem {
    fn state(self) -> &'static str {
        match self {
            Self::UpdateRequired => "update_required",
            Self::FingerprintMismatch => "fingerprint_mismatch",
            Self::BindingMismatch => "binding_mismatch",
            Self::Unauthorized => "unauthorized",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct LinkEpoch {
    registration: u64,
    issue: u64,
}
#[derive(Default)]
struct LinkState {
    epoch: LinkEpoch,
    issue: Option<LinkIssue>,
}
struct LinkIssue {
    generation: u64,
    problem: LinkProblem,
    since: u64,
    retry_at: Instant,
}
fn link_problem(error: &anyhow::Error) -> Option<LinkProblem> {
    if let Some(rejected) = error.downcast_ref::<crate::client::PcRejection>() {
        return match rejected.0.code {
            ErrorCode::VersionMismatch => Some(LinkProblem::UpdateRequired),
            ErrorCode::Unauthorized => Some(LinkProblem::Unauthorized),
            _ => None,
        };
    }
    match error.to_string().as_str() {
        "fingerprint_mismatch" => Some(LinkProblem::FingerprintMismatch),
        "device binding mismatch" => Some(LinkProblem::BindingMismatch),
        _ => None,
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub device: Id,
    pub name: String,
    pub role: String,
    pub state: String,
    pub last_seen: u64,
    pub dialer: Option<Id>,
    #[serde(default)]
    pub route: Option<String>,
}
impl Mesh {
    pub(super) fn cancel_withdrawn_join(&self) {
        if let Some(join) = self.join.lock().expect("join").as_mut()
            && join.phase != JoinPhase::Finished
        {
            join.phase = JoinPhase::Finished;
            join.state = "cancelled".into();
        }
    }
    pub(super) fn cancel_removed(&self, keys: &[(Id, Hash)]) {
        if let Some(join) = self.join.lock().expect("join").as_mut()
            && join.phase != JoinPhase::Finished
            && keys.iter().any(|(id, pin)| {
                join.uri.fingerprint == *pin
                    && join.authority.as_ref().is_none_or(|a| a.peer == *id)
            })
        {
            join.phase = JoinPhase::Finished;
            join.state = "cancelled".into();
            self.changed.notify_waiters();
        }
    }
    pub(super) fn join_scope(&self, pin: &Hash) -> bool {
        self.join
            .lock()
            .expect("join")
            .as_ref()
            .filter(|j| j.uri.fingerprint == *pin && j.phase != JoinPhase::Finished)
            .is_none_or(|j| j.setup_scope)
    }
    pub(super) fn new(peers: Vec<PeerInfo>) -> Self {
        Self {
            peers: Mutex::new(peers),
            ..Default::default()
        }
    }
}
const DIAL_SWEEP_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CACHED_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
struct DialCandidate {
    address: String,
    connect_timeout: Duration,
}
impl DialCandidate {
    fn current(address: String) -> Self {
        Self {
            address,
            connect_timeout: CONNECT_TIMEOUT,
        }
    }
    fn cached(address: String) -> Self {
        Self {
            address,
            connect_timeout: CACHED_CONNECT_TIMEOUT,
        }
    }
}
impl Core {
    pub(super) fn setup_attempts(&self) -> Vec<(Option<Id>, Hash)> {
        let mut targets = self
            .setup_incoming
            .lock()
            .expect("setup incoming")
            .values()
            .map(|v| (v.peer.clone(), v.pin.clone()))
            .collect::<Vec<_>>();
        if let Some(join) = self
            .mesh
            .join
            .lock()
            .expect("join")
            .as_ref()
            .filter(|j| j.phase != JoinPhase::Finished)
        {
            targets.push((
                join.authority.as_ref().map(|a| a.peer.clone()),
                join.uri.fingerprint.clone(),
            ));
        }
        targets
    }
    pub(super) fn computer_hello_for(
        &self,
        pin: &Hash,
        expected: Option<&Id>,
        outbound: bool,
    ) -> Hello {
        let mut hello = self.computer_hello();
        let peer = expected.cloned().or_else(|| {
            self.admission
                .lock()
                .expect("admission")
                .peers
                .get(pin)
                .cloned()
        });
        let default = if if outbound {
            self.mesh.join_scope(pin)
        } else {
            self.setup_invitation_scope.load(Ordering::SeqCst)
        } {
            SetupScope::Enabled
        } else {
            SetupScope::Disabled
        };
        if hello.capabilities.iter().any(|c| c == "setup") {
            hello.setup = Some(self.store.setup_hello(peer.as_ref(), default));
        }
        hello
    }
    pub(super) fn computer_hello(&self) -> Hello {
        let addresses = crate::discovery::pairing_candidates()
            .unwrap_or_default()
            .into_iter()
            .map(|i| SocketAddr::new(i.ip, self.bound_port()).to_string())
            .collect();
        let hello = Hello {
            setup: Some(self.store.setup_hello(None, SetupScope::Enabled)),
            version: 1,
            device_id: self.identity.device_id.clone(),
            name: self.config().name,
            role: Some(DeviceRole::Computer),
            addresses: Some(addresses),
            paused: None,
            capabilities: vec![
                "files".into(),
                "file_resume_v1".into(),
                "folder_v1".into(),
                "clipboard_text".into(),
                "clipboard_image".into(),
                "mesh".into(),
                "setup".into(),
            ],
        };
        #[cfg(test)]
        let hello = if self.setup_test_capability.load(Ordering::SeqCst) {
            hello
        } else {
            let mut h = hello;
            h.capabilities.retain(|c| c != "setup");
            h.setup = None;
            h
        };
        hello
    }
    pub(super) fn device_snapshots(
        &self,
        sessions: &Connections,
        admission: &Admission,
        ui: &crate::ui::State,
    ) -> Vec<DeviceSnapshot> {
        let setup = self.store.setup_state();
        let peers = self.mesh.peers.lock().expect("mesh peers");
        let mut devices: Vec<_> = admission
            .peers
            .values()
            .map(|id| {
                let peer = peers.iter().find(|p| {
                    &p.id == id && Some(p.generation) == self.store.pairing_generation(id)
                });
                let session = sessions.sessions.get(id);
                let phone = ui.phones.get(id);
                DeviceSnapshot {
                    device: id.clone(),
                    name: setup
                        .roster
                        .iter()
                        .find(|m| {
                            &m.device_id == id
                                && m.status == MemberStatus::Member
                                && admission.peers.get(&m.fingerprint) == Some(id)
                        })
                        .map(|m| m.name.clone())
                        .or_else(|| phone.map(|p| p.name.clone()))
                        .or_else(|| peer.map(|p| p.name.clone()))
                        .unwrap_or_default(),
                    role: session
                        .map(|s| s.role.as_str())
                        .or_else(|| peer.map(|p| p.role.as_str()))
                        .unwrap_or("phone")
                        .into(),
                    state: if let Some(problem) = self.link_problem(id) {
                        problem.state()
                    } else if session.is_some() {
                        "connected"
                    } else if self
                        .mesh
                        .connecting
                        .lock()
                        .expect("connecting")
                        .contains(id)
                    {
                        "connecting"
                    } else {
                        "disconnected"
                    }
                    .into(),
                    last_seen: phone
                        .or_else(|| session.map(|s| &s.phone))
                        .map(|p| p.last_seen)
                        .unwrap_or(0),
                    route: session
                        .map(|s| s.phone.route.clone())
                        .filter(|r| !r.is_empty()),
                    dialer: session.map(|s| {
                        if s.outbound {
                            self.identity.device_id.clone()
                        } else {
                            id.clone()
                        }
                    }),
                }
            })
            .collect();
        devices.sort_by_key(|d| {
            (
                d.state != "connected",
                std::cmp::Reverse(d.last_seen),
                d.device.as_str().to_owned(),
            )
        });
        devices
    }
    pub(super) fn link_problem(&self, device: &Id) -> Option<LinkProblem> {
        self.mesh
            .links
            .lock()
            .expect("link state")
            .get(device)
            .and_then(|link| link.issue.as_ref())
            .filter(|issue| Some(issue.generation) == self.store.pairing_generation(device))
            .map(|issue| issue.problem)
    }
    fn link_epoch(&self, device: &Id) -> LinkEpoch {
        let _sessions = self.sessions.lock().expect("sessions");
        self.link_epoch_locked(device)
    }
    // Call with sessions held, so registration cannot advance across the check.
    pub(super) fn link_epoch_locked(&self, device: &Id) -> LinkEpoch {
        self.mesh
            .links
            .lock()
            .expect("link state")
            .get(device)
            .map(|link| link.epoch)
            .unwrap_or_default()
    }
    // Before inbound TLS/HELLO the device is unknown. Freeze each peer's own
    // decisions, then select its entry once the certificate identifies it.
    pub(super) fn link_epochs(&self) -> HashMap<Id, LinkEpoch> {
        let _sessions = self.sessions.lock().expect("sessions");
        self.mesh
            .links
            .lock()
            .expect("link state")
            .iter()
            .map(|(device, link)| (device.clone(), link.epoch))
            .collect()
    }
    pub(super) fn record_link_registration(&self, device: &Id, registration: u64) {
        self.mesh
            .links
            .lock()
            .expect("link state")
            .entry(device.clone())
            .or_default()
            .epoch
            .registration = registration;
    }
    pub(super) fn set_link_problem_if(
        &self,
        device: &Id,
        generation: u64,
        epoch: LinkEpoch,
        problem: Option<LinkProblem>,
    ) {
        let _sessions = self.sessions.lock().expect("sessions");
        self.set_link_problem_checked(device, generation, Some(epoch), problem);
    }
    pub(super) fn setup_refusal_since(&self) -> Option<u64> {
        self.mesh
            .links
            .lock()
            .expect("link state")
            .iter()
            .filter_map(|(id, link)| {
                let issue = link.issue.as_ref()?;
                (issue.problem == LinkProblem::Unauthorized
                    && self.store.pairing_generation(id) == Some(issue.generation))
                .then_some(issue.since)
            })
            .min()
    }
    pub(super) fn set_link_problem(
        &self,
        device: &Id,
        generation: u64,
        problem: Option<LinkProblem>,
    ) {
        self.set_link_problem_checked(device, generation, None, problem);
    }
    fn set_link_problem_checked(
        &self,
        device: &Id,
        generation: u64,
        epoch: Option<LinkEpoch>,
        problem: Option<LinkProblem>,
    ) {
        if self.store.pairing_generation(device) != Some(generation) {
            return;
        }
        let mut links = self.mesh.links.lock().expect("link state");
        let link = links.entry(device.clone()).or_default();
        if epoch.is_some_and(|epoch| epoch != link.epoch) {
            return;
        }
        link.epoch.issue += 1;
        if let Some(problem) = problem {
            let since = link
                .issue
                .as_ref()
                .filter(|i| i.generation == generation && i.problem == problem)
                .map(|i| i.since)
                .unwrap_or_else(crate::now_ms);
            link.issue = Some(LinkIssue {
                generation,
                problem,
                since,
                retry_at: Instant::now() + Duration::from_secs(300),
            });
        } else if link
            .issue
            .as_ref()
            .is_some_and(|issue| issue.generation == generation)
        {
            link.issue = None;
            // Keep revision history after recovery and disconnection, until
            // this peer is removed. An old result cannot resurrect its issue.
            self.mesh.changed.notify_waiters();
        }
    }
    /// Parse only. No socket is opened until the UI confirms this opaque attempt.
    pub async fn join(&self, text: &str) -> Result<Value> {
        let _guard = self.mesh.gate.lock().await;
        ensure!(
            self.mesh
                .join
                .lock()
                .expect("join")
                .as_ref()
                .is_none_or(|j| j.phase == JoinPhase::Finished),
            "pairing in progress; complete or cancel the existing attempt"
        );
        if !text.trim().starts_with("pneu://pair") {
            return Ok(json!({"state":"no_link"}));
        }
        let uri = match PairingUri::parse(text.trim()) {
            Ok(uri) if uri.version == 1 => uri,
            Ok(_) => return Ok(json!({"state":"update_required"})),
            Err(_) => return Ok(json!({"state":"garbled"})),
        };
        let _pairing = self.pairing_gate.lock().await;
        let paired = self
            .admission
            .lock()
            .expect("admission")
            .peers
            .get(&uri.fingerprint)
            .cloned();
        let state = if uri.fingerprint == self.identity.fingerprint
            || paired.as_ref() == Some(&self.identity.device_id)
        {
            "own_code"
        } else if uri.expires_at <= crate::now_ms() {
            "expired"
        } else if paired.is_some_and(|id| self.connected_devices().contains(&id)) {
            "already_paired"
        } else {
            "confirmation"
        };
        if state != "confirmation" {
            return Ok(json!({"state":state,"name":uri.name,"fingerprint":uri.fingerprint}));
        }
        let join = Join {
            attempt: crate::random_hex(16),
            uri,
            state: state.into(),
            phase: JoinPhase::Preview,
            authority: None,
            requested: false,
            installed_generation: Arc::new(AtomicU64::new(0)),
            retry_at: None,
            ready: Default::default(),
            setup_scope: true,
        };
        let value = join_value(&join);
        *self.mesh.join.lock().expect("join") = Some(join);
        Ok(value)
    }
    pub fn join_status(&self, attempt: &str) -> Result<Value> {
        let state = self.mesh.join.lock().expect("join");
        let join = state
            .as_ref()
            .filter(|j| j.attempt == attempt)
            .context("unknown join attempt")?;
        let mut value = join_value(join);
        if join.state == "paired" {
            if let Some(authority) = &join.authority {
                value["device"] = json!(authority.peer);
            } else if let Some(device) = self
                .admission
                .lock()
                .expect("admission")
                .peers
                .get(&join.uri.fingerprint)
            {
                value["device"] = json!(device);
            }
        }
        Ok(value)
    }
    pub async fn confirm_join(&self, attempt: &str) -> Result<Value> {
        self.confirm_join_scoped(attempt, true).await
    }
    pub async fn confirm_join_scoped(&self, attempt: &str, enabled: bool) -> Result<Value> {
        let _guard = self.mesh.gate.lock().await;
        let mut state = self.mesh.join.lock().expect("join");
        let join = state
            .as_mut()
            .filter(|j| j.attempt == attempt)
            .context("unknown join attempt")?;
        #[cfg(test)]
        if let Some((entered, release)) =
            self.mesh.confirm_hook.lock().expect("confirm hook").take()
        {
            let _ = entered.send(());
            let _ = release.recv();
        }
        ensure!(
            join.phase != JoinPhase::Finished,
            "join cannot be confirmed"
        );
        if join.phase == JoinPhase::Preview {
            join.setup_scope = enabled;
        } else {
            ensure!(join.setup_scope == enabled, "pairing scope is frozen");
        }
        if join.phase == JoinPhase::Preview && join.uri.expires_at <= crate::now_ms() {
            join.state = "expired".into();
            join.phase = JoinPhase::Finished;
        } else {
            join.phase = JoinPhase::Pending;
            join.state = "pairing".into();
            join.retry_at = None;
        }
        let value = join_value(join);
        self.mesh.changed.notify_waiters();
        Ok(value)
    }
    pub async fn cancel_join(&self, attempt: &str) -> Result<()> {
        let _guard = self.mesh.gate.lock().await;
        let mut join = self
            .mesh
            .join
            .lock()
            .expect("join")
            .as_ref()
            .filter(|j| j.attempt == attempt)
            .cloned()
            .context("unknown join attempt")?;
        if join.phase == JoinPhase::Finished {
            return Ok(());
        }
        join.state = "cancelled".into();
        join.phase = JoinPhase::Finished;
        *self.mesh.join.lock().expect("join") = Some(join);
        self.mesh.changed.notify_waiters();
        Ok(())
    }
    fn joining(&self, attempt: &str) -> bool {
        self.mesh
            .join
            .lock()
            .expect("join")
            .as_ref()
            .is_some_and(|j| j.attempt == attempt && j.phase == JoinPhase::Pending)
    }
    async fn connect_computer(
        &self,
        addresses: &[String],
        pin: Hash,
        expected: Option<&Id>,
    ) -> Result<ComputerLink> {
        let candidates: Vec<_> = addresses
            .iter()
            .cloned()
            .map(DialCandidate::current)
            .collect();
        self.connect_computer_candidates(&candidates, pin, expected)
            .await
    }
    async fn connect_computer_candidates(
        &self,
        candidates: &[DialCandidate],
        pin: Hash,
        expected: Option<&Id>,
    ) -> Result<ComputerLink> {
        let connector = TlsConnector::from(Arc::new(crate::tls::client_config(
            &self.identity,
            pin.clone(),
        )?));
        let deadline = Instant::now() + DIAL_SWEEP_TIMEOUT;
        let mut mismatch = false;
        for candidate in candidates {
            let address = &candidate.address;
            // One fixed sweep deadline includes fair admission, DNS, TCP, TLS
            // and HELLO. Cancellation releases candidate-owned permits/socket.
            let result: Result<_> = match tokio::time::timeout_at(deadline, async {
                validate_address(address)?;
                // Admission belongs to one candidate, never to its address
                // sweep or retry delay. Failed candidates release both permits
                // before joining the fair queue for the next address.
                let permit = self.outbound_ingress.clone().acquire_owned().await?;
                let transport = self.transports.clone().acquire_owned().await?;
                let tcp = tokio::time::timeout(candidate.connect_timeout, TcpStream::connect(address)).await??;
                tcp.set_nodelay(true)?;
                let remote = tcp.peer_addr()?;
                let tls = tokio::time::timeout(Duration::from_secs(10), connector.connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)).await?;
                let mut tls: TlsStream<TcpStream> = match tls {
                    Ok(tls) => tls.into(), Err(error) => {
                        mismatch |= matches!(error.get_ref().and_then(|e| e.downcast_ref::<rustls::Error>()), Some(rustls::Error::General(message)) if message == "Pneu certificate authentication failed");
                        return Err(error.into());
                    }
                };
                let (local_hello,setup_revision)={let _gate=self.pairing_gate.lock().await;(self.computer_hello_for(&pin,expected,true),self.store.setup_state().generation)};
                let setup_sent=local_hello.setup.clone().map(|h|(h,setup_revision));
                let sent = send(&mut tls, &Message::Hello(local_hello)).await;
                let mut hello = match read_handshake(&mut tls).await? {
                    Message::Hello(h) => h,
                    Message::Error(error) => return Err(crate::client::PcRejection(error).into()),
                    _ => bail!("unexpected HELLO"),
                };
                sent?;
                if hello.device_id == self.identity.device_id {
                    let _ = send_error(&mut tls, ErrorCode::Unauthorized).await;
                    return Err(crate::client::PcRejection(ErrorMessage { code: ErrorCode::Unauthorized, message: Some("remote claimed local device ID".into()), supported_version: None }).into());
                }
                if hello.version != 1 {
                    let _ = send_error(&mut tls, ErrorCode::VersionMismatch).await;
                    return Err(crate::client::PcRejection(ErrorMessage { code: ErrorCode::VersionMismatch, message: None, supported_version: Some(1) }).into());
                }
                ensure!(expected.is_none_or(|id| *id == hello.device_id), "device binding mismatch");
                if hello.capabilities.iter().any(|c| c == "mesh") && hello.role != Some(DeviceRole::Computer) {
                    let _ = send_error(&mut tls, ErrorCode::MalformedMessage).await;
                    return Err(crate::client::PcRejection(ErrorMessage { code: ErrorCode::MalformedMessage, message: Some("invalid computer role".into()), supported_version: None }).into());
                }
                normalize_legacy_hello(&mut hello, true);
                Ok(ComputerLink { tls, hello, remote, address: address.clone(), permit, transport, setup_sent })
            }).await {
                Ok(result) => result,
                Err(_) => break,
            };
            match result {
                Ok(link) => return Ok(link),
                Err(e)
                    if e.downcast_ref::<crate::client::PcRejection>().is_some()
                        || e.downcast_ref::<ProtocolError>().is_some()
                        || e.to_string() == "device binding mismatch" =>
                {
                    return Err(e);
                }
                Err(_) => {}
            }
        }
        bail!(if mismatch {
            "fingerprint_mismatch"
        } else {
            "unreachable"
        })
    }
    async fn join_worker(self: Arc<Self>, join: Join) -> Result<()> {
        let candidate = self.next_candidate.fetch_add(1, Ordering::SeqCst);
        if !join.requested && join.uri.expires_at <= crate::now_ms() {
            return Err(PairRejected("expired").into());
        }
        let ComputerLink {
            mut tls,
            hello,
            remote,
            address,
            permit,
            transport: _transport,
            setup_sent,
        } = self
            .connect_computer(
                &join.uri.addresses,
                join.uri.fingerprint.clone(),
                join.authority.as_ref().map(|a| &a.peer),
            )
            .await?;
        // Capture authority once, immediately before the first PAIR_REQUEST.
        // Retries retain the original device ID, tombstone and admission entry.
        {
            let _guard = self.mesh.gate.lock().await;
            ensure!(self.joining(&join.attempt), "cancelled");
            if !join.requested && join.uri.expires_at <= crate::now_ms() {
                return Err(PairRejected("expired").into());
            }
            #[cfg(test)]
            {
                let hook = self.mesh.request_hook.lock().expect("request hook").take();
                if let Some((entered, release)) = hook {
                    let _ = entered.send(());
                    let _ = release.await;
                }
            }
            let _pairing = self.pairing_gate.lock().await;
            let mut state = self.mesh.join.lock().expect("join");
            let current = state
                .as_mut()
                .filter(|j| j.attempt == join.attempt && j.phase == JoinPhase::Pending)
                .context("cancelled")?;
            if current.authority.is_none() {
                current.authority = Some(JoinAuthority {
                    peer: hello.device_id.clone(),
                    generation: self.store.pairing_generation(&hello.device_id),
                    admission: self
                        .admission
                        .lock()
                        .expect("admission")
                        .peers
                        .get(&join.uri.fingerprint)
                        .cloned(),
                });
            }
            current.requested = true;
        }
        send(
            &mut tls,
            &Message::PairRequest(PairRequest {
                token: join.uri.token.clone(),
            }),
        )
        .await?;
        #[cfg(test)]
        if self.mesh.panic_join.swap(false, Ordering::SeqCst) {
            panic!("injected join failure after PAIR_REQUEST");
        }
        let mut buffered = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match read_frame(&mut tls).await?.context("pairing EOF")? {
                    Message::PairResult(r) if r.accepted => break,
                    Message::PairResult(r) => {
                        let state = match r.code {
                            Some(PairCode::ExpiredToken) => "expired",
                            Some(PairCode::InvalidToken) => "garbled",
                            Some(PairCode::AlreadyPaired) => "already_paired",
                            Some(PairCode::PairingClosed) => "already_used",
                            Some(PairCode::StorageFailed) | None => bail!("unreachable"),
                        };
                        return Err(PairRejected(state).into());
                    }
                    Message::Ping(p) => send(&mut tls, &Message::Pong(p)).await?,
                    m @ (Message::Clipboard(_)
                    | Message::Status(_)
                    | Message::FileBegin(_)
                    | Message::FileChunk(_)
                    | Message::FileEnd(_)
                    | Message::PeerSettings(_)
                    | Message::Members(_)) => {
                        if matches!(m, Message::Members(_)) {
                            ensure!(
                                !buffered.iter().any(|m| matches!(m, Message::Members(_))),
                                "too many buffered MEMBERS"
                            );
                        }
                        ensure!(buffered.len() < 8, "too much data before pairing result");
                        buffered.push(m);
                    }
                    Message::Error(error) => return Err(crate::client::PcRejection(error).into()),
                    _ => bail!("unexpected pairing frame"),
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let gate = self.mesh.gate.lock().await;
        ensure!(self.joining(&join.attempt), "cancelled");
        let guard = self.pairing_gate.clone().lock_owned().await;
        ensure!(self.joining(&join.attempt), "cancelled");
        let (authority, installed_generation) = self
            .mesh
            .join
            .lock()
            .expect("join")
            .as_ref()
            .and_then(|j| {
                j.authority.clone().map(|a| {
                    let generation = j.installed_generation.load(Ordering::SeqCst);
                    (a, (generation != 0).then_some(generation))
                })
            })
            .context("missing join authority")?;
        let admission = self
            .admission
            .lock()
            .expect("admission")
            .peers
            .get(&join.uri.fingerprint)
            .cloned();
        if authority.peer != hello.device_id
            || ((authority.generation != self.store.pairing_generation(&hello.device_id)
                || authority.admission != admission)
                && !(installed_generation.is_some()
                    && installed_generation == self.store.pairing_generation(&hello.device_id)
                    && admission.as_ref() == Some(&hello.device_id)))
        {
            // A newer local decision owns this peer. Finish without changing
            // either endpoint's binding; UNPAIR has peer-wide semantics.
            let mut state = self.mesh.join.lock().expect("join");
            let current = state.as_mut().context("join disappeared")?;
            current.phase = JoinPhase::Finished;
            current.state = if admission.as_ref() == Some(&hello.device_id) {
                "paired"
            } else {
                "cancelled"
            }
            .into();
            return Ok(());
        }
        let captured = if admission.as_ref() == Some(&hello.device_id) {
            let captured = self.store.with_current_pairing();
            captured
                .setup_fresh_local_recovery(
                    hello.device_id.clone(),
                    self.self_member(),
                    self.setup_publish(guard, None, None),
                )
                .await?;
            captured
        } else {
            let mut initial = hello.clone();
            let mut addresses = vec![address.clone()];
            for candidate in hello
                .addresses
                .iter()
                .flatten()
                .chain(join.uri.addresses.iter())
            {
                if addresses.len() < 16 && !addresses.contains(candidate) {
                    addresses.push(candidate.clone());
                }
            }
            initial.role = Some(DeviceRole::Computer);
            initial.addresses = Some(addresses);
            self.persist_pairing_hello(
                &hello.device_id,
                &join.uri.fingerprint,
                guard,
                false,
                Some(initial),
                Some(join.installed_generation.clone()),
            )
            .await?
        };
        if let Some(j) = self.mesh.join.lock().expect("join").as_mut() {
            j.state = "paired".into();
            j.phase = JoinPhase::Finished;
        }
        join.ready.store(true, Ordering::SeqCst);
        drop(gate);
        let _route = self.persist_working_route(captured.clone(), hello.device_id.clone(), address);
        self.register_buffered_setup(
            tls,
            hello,
            join.uri.fingerprint,
            remote,
            permit,
            captured,
            true,
            false,
            buffered,
            Some(candidate),
            setup_sent,
        )
        .await
    }
    fn persist_working_route(self: &Arc<Self>, store: Store, device: Id, address: String) -> Task {
        let core = self.clone();
        Task::spawn(async move {
            loop {
                match store.remember_route(device.clone(), address.clone()).await {
                    Ok(()) => return Ok(()),
                    Err(error) => core.error(
                        Some(device.clone()),
                        format!("computer route deferred: {error}"),
                    ),
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
    }
    pub(super) fn persist_readiness(
        self: &Arc<Self>,
        store: Store,
        device: Id,
        evidence: Option<Arc<Notify>>,
    ) -> Task {
        let core = self.clone();
        Task::spawn(async move {
            if let Some(evidence) = evidence {
                evidence.notified().await;
            }
            loop {
                match store.mark_peer_established(device.clone()).await {
                    Ok(()) => return Ok(()),
                    Err(error) => core.error(
                        Some(device.clone()),
                        format!("readiness persistence deferred: {error}"),
                    ),
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
    }
    async fn confirm_peer_ready(&self, tls: &mut TlsStream<TcpStream>) -> Result<Vec<Message>> {
        let nonce = Nonce::new(crate::random_hex(8))?;
        send(
            tls,
            &Message::Ping(Ping {
                nonce: nonce.clone(),
            }),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut buffered = Vec::new();
            loop {
                match read_frame(tls).await?.context("READY EOF")? {
                    Message::Pong(p) if p.nonce == nonce => return Ok(buffered),
                    Message::Ping(p) => send(tls, &Message::Pong(p)).await?,
                    Message::Error(e) => return Err(crate::client::PcRejection(e).into()),
                    m @ (Message::Clipboard(_)
                    | Message::Status(_)
                    | Message::FileBegin(_)
                    | Message::FileChunk(_)
                    | Message::FileEnd(_)
                    | Message::PeerSettings(_)
                    | Message::Members(_)) => {
                        if matches!(m, Message::Members(_)) {
                            ensure!(
                                !buffered.iter().any(|m| matches!(m, Message::Members(_))),
                                "too many buffered MEMBERS"
                            );
                        }
                        ensure!(buffered.len() < 8, "too much data before READY");
                        buffered.push(m);
                    }
                    _ => bail!("unexpected READY confirmation"),
                }
            }
        })
        .await?
    }
    async fn dial_worker(
        self: Arc<Self>,
        peer: PeerInfo,
        mut cancelled: watch::Receiver<bool>,
    ) -> Result<()> {
        let captured = {
            let _guard = self.pairing_gate.lock().await;
            self.store.with_current_pairing()
        };
        if captured.pairing_generation(&peer.id) != Some(peer.generation) {
            return Ok(());
        }
        let scope = self
            .action_scope
            .lock()
            .expect("actions")
            .get(&peer.id)
            .cloned()
            .context("peer revoked")?;
        let mut revoked = scope.cancelled.subscribe();
        let mut shutdown = self.shutdown.subscribe();
        let mut backoff = 2u64;
        loop {
            // Register before inspecting state to avoid losing recovery/drop
            // notifications between the check and the wait.
            let changed = self.mesh.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if *cancelled.borrow() || *revoked.borrow() || *self.shutdown.borrow() {
                break;
            }
            let retry_at = self
                .mesh
                .links
                .lock()
                .expect("link state")
                .get(&peer.id)
                .and_then(|link| link.issue.as_ref())
                .filter(|issue| issue.generation == peer.generation)
                .map(|issue| issue.retry_at);
            if let Some(retry_at) = retry_at.filter(|at| *at > Instant::now()) {
                tokio::select! { biased;
                    _ = cancelled.changed() => break,
                    _ = revoked.changed() => break,
                    _ = shutdown.changed() => break,
                    _ = &mut changed => { backoff = 2; continue; }
                    _ = tokio::time::sleep_until(retry_at) => {}
                }
            }
            let connected = self.connected_devices().contains(&peer.id);
            // Keep a higher-ID lone dial indefinitely; only actual duplicates are ranked.
            if !connected {
                // Capture before any candidate starts; never upgrade speculative
                // revocation authority using evidence learned during this attempt.
                let epoch = self.link_epoch(&peer.id);
                let candidate = self.next_candidate.fetch_add(1, Ordering::SeqCst);
                let may_revoke = match captured.peer_established(peer.id.clone()).await {
                    Ok(established) => established,
                    Err(error) => {
                        self.error(
                            Some(peer.id.clone()),
                            format!("readiness evidence unavailable: {error}"),
                        );
                        false
                    }
                };
                let reconnect = crate::ui::ScopeReconnect::new(self.ui.clone(), peer.id.clone());
                let dial = async {
                    let addresses = self.dial_candidates(&peer);
                    let ComputerLink {
                        mut tls,
                        hello,
                        remote,
                        address,
                        permit,
                        transport: _transport,
                        setup_sent,
                    } = self
                        .connect_computer_candidates(
                            &addresses,
                            peer.fingerprint.clone(),
                            Some(&peer.id),
                        )
                        .await?;
                    let buffered = self.confirm_peer_ready(&mut tls).await?;
                    // Persist reverse-dial evidence and the working route
                    // without holding the healthy socket hostage to storage.
                    let _established =
                        self.persist_readiness(captured.clone(), peer.id.clone(), None);
                    let _route =
                        self.persist_working_route(captured.clone(), peer.id.clone(), address);
                    self.clone()
                        .register_buffered_setup(
                            tls,
                            hello,
                            peer.fingerprint.clone(),
                            remote,
                            permit,
                            captured.clone(),
                            true,
                            false,
                            buffered,
                            Some(candidate),
                            setup_sent,
                        )
                        .await
                };
                let _connecting = Connecting::new(self.clone(), peer.id.clone());
                #[cfg(test)]
                if self.mesh.panic_dial.swap(false, Ordering::SeqCst) {
                    panic!("injected dial failure");
                }
                tokio::pin!(dial);
                let result = tokio::select! { biased;
                    _ = cancelled.changed() => {
                        // A lifecycle commit has already fenced this binding
                        // and published its close reason. Let an owned READY
                        // transport flush its final control notification before
                        // dropping it; connecting candidates remain bounded.
                        let _ = tokio::time::timeout(Duration::from_secs(3), &mut dial).await;
                        break;
                    },
                    _ = revoked.changed() => {
                        let _ = tokio::time::timeout(Duration::from_secs(3), &mut dial).await;
                        break;
                    },
                    _ = shutdown.changed() => break,
                    result = &mut dial => result
                };
                drop(reconnect);
                if let Err(error) = &result
                    && error
                        .downcast_ref::<crate::client::PcRejection>()
                        .is_some_and(|e| e.0.code == ErrorCode::Unauthorized)
                    && may_revoke
                    && !self.store.setup_protected(&peer.id)
                {
                    // Revoke only this captured generation, serialized by lifecycle admission.
                    self.forget_pairing_generation_checked(
                        peer.id.clone(),
                        CloseReason::Error(ErrorCode::Closing),
                        None,
                        None,
                        Some(peer.generation),
                        Some(epoch),
                    )
                    .await?;
                    break;
                }
                if let Err(error) = &result
                    && let Some(problem) = link_problem(error)
                {
                    self.set_link_problem_if(&peer.id, peer.generation, epoch, Some(problem));
                }
                if result.is_ok() {
                    backoff = 2;
                }
            }
            let delay = if connected {
                Duration::from_millis(250)
            } else {
                let millis = rand::rng().random_range(backoff * 800..=backoff * 1200);
                backoff = (backoff * 2).min(60);
                Duration::from_millis(millis)
            };
            tokio::select! { _ = cancelled.changed() => break, _ = revoked.changed() => break, _ = shutdown.changed() => break, _ = &mut changed => backoff = 2, _ = tokio::time::sleep(delay) => {} }
        }
        Ok(())
    }
    fn dial_candidates(&self, peer: &PeerInfo) -> Vec<DialCandidate> {
        let latest = self
            .mesh
            .peers
            .lock()
            .expect("mesh peers")
            .iter()
            .find(|p| p.id == peer.id && p.generation == peer.generation)
            .cloned()
            .unwrap_or_else(|| peer.clone());
        let mut seen = HashSet::new();
        // The route cache is ordered last-good, HELLO hints, invitation hints.
        // Secondary LAN routes use short attempts before the Tailscale fallback.
        latest
            .addresses
            .first()
            .filter(|a| latest.route_proven && !tailscale_endpoint(a))
            .cloned()
            .map(DialCandidate::current)
            .into_iter()
            .chain(
                latest
                    .addresses
                    .iter()
                    .skip(usize::from(latest.route_proven))
                    .filter(|a| !tailscale_endpoint(a))
                    .cloned()
                    .map(DialCandidate::cached),
            )
            .chain(
                latest
                    .addresses
                    .iter()
                    .filter(|a| tailscale_endpoint(a))
                    .cloned()
                    .map(DialCandidate::current),
            )
            .filter(|candidate| {
                validate_address(&candidate.address).is_ok()
                    && seen.insert(candidate.address.clone())
            })
            .take(24)
            .collect()
    }
    pub(super) async fn mesh_supervise(self: Arc<Self>) -> Result<()> {
        type DialWorker = (u64, watch::Sender<bool>, Task);
        let mut workers: HashMap<Id, DialWorker> = HashMap::new();
        let mut join_task: Option<(Join, Task)> = None;
        let mut links: Vec<Task> = Vec::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut stop = self.shutdown.subscribe();
        let mut reach_refresh = Instant::now();
        loop {
            tokio::select! { biased; _ = stop.changed() => break, _ = tick.tick() => {} }
            if *stop.borrow() {
                break;
            }
            #[cfg(test)]
            if self.mesh.panic_supervisor.swap(false, Ordering::SeqCst) {
                panic!("injected supervisor failure");
            }
            if self.store.setup_state().pending_link {
                let guard = self.pairing_gate.clone().lock_owned().await;
                if let Err(error) = self
                    .store
                    .setup_discovery_tick(
                        self.self_member(),
                        crate::now_ms(),
                        self.setup_publish(guard, None, None),
                    )
                    .await
                {
                    self.error(None, format!("setup discovery deferred: {error}"));
                }
            }
            if self.store.setup_state().setup_id.is_some() && Instant::now() >= reach_refresh {
                reach_refresh = Instant::now() + Duration::from_secs(30);
                if let Err(error) = self.store.setup_refresh_self(self.self_member()).await {
                    self.error(None, format!("setup routes unavailable: {error}"));
                }
            }
            if let Err(error) = self.reconcile_setup_candidates().await {
                self.error(None, format!("membership reconciliation deferred: {error}"));
            }
            let peers = match self.store.peer_info().await {
                Ok(peers) => peers,
                Err(_) => {
                    self.error(None, "computer link metadata unavailable");
                    continue;
                }
            };
            *self.mesh.peers.lock().expect("mesh peers") = peers.clone();
            {
                // Use live admission under the lifecycle's session lock, not
                // the possibly stale disk snapshot: pruning a newly registered
                // peer would erase its fence while a handshake is in flight.
                let _sessions = self.sessions.lock().expect("sessions");
                let admission = self.admission.lock().expect("admission");
                self.mesh
                    .links
                    .lock()
                    .expect("link state")
                    .retain(|id, _| admission.peers.values().any(|peer| peer == id));
            }
            for (id, (generation, cancel, _)) in &workers {
                if !peers
                    .iter()
                    .any(|p| &p.id == id && p.generation == *generation && p.role == "computer")
                {
                    cancel.send_replace(true);
                }
            }
            let done: Vec<_> = workers
                .iter()
                .filter(|(_, (_, _, h))| h.is_finished())
                .map(|(id, _)| id.clone())
                .collect();
            for id in done {
                if let Some((_, _, task)) = workers.remove(&id) {
                    let _ = task.await;
                }
            }
            for peer in peers.into_iter().filter(|p| p.role == "computer") {
                if !workers.contains_key(&peer.id) {
                    let (cancel, rx) = watch::channel(false);
                    workers.insert(
                        peer.id.clone(),
                        (
                            peer.generation,
                            cancel,
                            Task::spawn(self.clone().dial_worker(peer, rx)),
                        ),
                    );
                }
            }
            let mut i = 0;
            while i < links.len() {
                if links[i].is_finished() {
                    let _ = links.swap_remove(i).await;
                } else {
                    i += 1;
                }
            }
            if join_task
                .as_ref()
                .is_some_and(|(join, _)| join.ready.load(Ordering::SeqCst))
            {
                links.push(join_task.take().expect("join task").1);
            }
            if let Some((attempt, task)) = &join_task {
                if task.is_finished() {
                    let attempt = attempt.attempt.clone();
                    let (_, task) = join_task.take().expect("join task");
                    let outcome = task.await;
                    if self.joining(&attempt) {
                        let rejected = matches!(&outcome, Ok(Err(e)) if e.downcast_ref::<PairRejected>().is_some());
                        let state = match outcome {
                            Ok(Err(e)) => join_error(&e),
                            _ => "unreachable",
                        };
                        let _guard = self.mesh.gate.lock().await;
                        let mut join = self.mesh.join.lock().expect("join");
                        let Some(pending) = join
                            .as_mut()
                            .filter(|j| j.attempt == attempt && j.phase == JoinPhase::Pending)
                        else {
                            continue;
                        };
                        #[cfg(test)]
                        if let Some((entered, release)) =
                            self.mesh.retry_hook.lock().expect("retry hook").take()
                        {
                            let _ = entered.send(());
                            let _ = release.recv();
                        }
                        if rejected {
                            pending.phase = JoinPhase::Finished;
                            pending.state = state.into();
                        } else {
                            pending.state = state.into();
                            pending.retry_at = Some(
                                Instant::now()
                                    + Duration::from_secs(
                                        if matches!(
                                            state,
                                            "update_required"
                                                | "fingerprint_mismatch"
                                                | "binding_mismatch"
                                        ) {
                                            300
                                        } else {
                                            2
                                        },
                                    ),
                            );
                        }
                    }
                } else if !self.joining(&attempt.attempt) {
                    // Successful joins continue as ordinary READY workers; unpair closes them.
                    let cancelled = self
                        .mesh
                        .join
                        .lock()
                        .expect("join")
                        .as_ref()
                        .is_none_or(|j| j.attempt != attempt.attempt || j.state == "cancelled");
                    if cancelled {
                        task.abort();
                        let (_, task) = join_task.take().expect("join task");
                        let _ = task.await;
                    }
                }
            }
            if join_task.is_none()
                && let Some(join) = self
                    .mesh
                    .join
                    .lock()
                    .expect("join")
                    .as_ref()
                    .filter(|j| {
                        j.phase == JoinPhase::Pending
                            && j.retry_at.is_none_or(|at| at <= Instant::now())
                    })
                    .cloned()
            {
                join_task = Some((join.clone(), Task::spawn(self.clone().join_worker(join))));
            }
        }
        for (_, cancel, _) in workers.values() {
            cancel.send_replace(true);
        }
        for task in links {
            task.abort();
            let _ = task.await;
        }
        for (_, (_, _, task)) in workers {
            let _ = task.await;
        }
        if let Some((_, task)) = join_task {
            task.abort();
            let _ = task.await;
        }
        Ok(())
    }
}
fn join_value(join: &Join) -> Value {
    json!({"attempt":join.attempt,"state":join.state,"name":join.uri.name,"fingerprint":join.uri.fingerprint,"role":"computer","setup_enabled":join.setup_scope})
}
fn join_error(error: &anyhow::Error) -> &'static str {
    if let Some(rejected) = error.downcast_ref::<PairRejected>() {
        return rejected.0;
    }
    if let Some(problem) = link_problem(error)
        && problem != LinkProblem::Unauthorized
    {
        return problem.state();
    }
    // Generic authorization failures also cover temporary unknown-client
    // throttling/provisional capacity. They prove nothing about token usage.
    if error.downcast_ref::<crate::client::PcRejection>().is_some() {
        return "unreachable";
    }
    match error.to_string().as_str() {
        "expired" => "expired",
        "already_used" => "already_used",
        "already_paired" => "already_paired",
        "garbled" => "garbled",
        "fingerprint_mismatch" => "fingerprint_mismatch",
        _ => "unreachable",
    }
}
fn tailscale_endpoint(address: &str) -> bool {
    address.parse::<SocketAddr>().is_ok_and(|a| matches!(a.ip(), std::net::IpAddr::V4(ip) if ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1])))
}

#[derive(Debug)]
struct PairRejected(&'static str);
impl std::fmt::Display for PairRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for PairRejected {}

/// A supervisor owns its children even during unwind/cancellation. Dropping a
/// naked JoinHandle detaches it; this wrapper aborts instead.
pub(super) struct Task(tokio::task::JoinHandle<Result<()>>);
impl Task {
    pub(super) fn spawn(
        future: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) -> Self {
        Self(tokio::spawn(future))
    }
}
impl std::ops::Deref for Task {
    type Target = tokio::task::JoinHandle<Result<()>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::future::Future for Task {
    type Output = std::result::Result<Result<()>, tokio::task::JoinError>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct ComputerLink {
    tls: TlsStream<TcpStream>,
    hello: Hello,
    remote: SocketAddr,
    address: String,
    permit: tokio::sync::OwnedSemaphorePermit,
    transport: tokio::sync::OwnedSemaphorePermit,
    setup_sent: Option<(SetupHello, u64)>,
}
struct Connecting {
    core: Arc<Core>,
    device: Id,
}
impl Connecting {
    fn new(core: Arc<Core>, device: Id) -> Self {
        core.mesh
            .connecting
            .lock()
            .expect("connecting")
            .insert(device.clone());
        Self { core, device }
    }
}
impl Drop for Connecting {
    fn drop(&mut self) {
        self.core
            .mesh
            .connecting
            .lock()
            .expect("connecting")
            .remove(&self.device);
    }
}

/// Cancellation of TCP/HELLO/READY futures cannot strand a published session.
pub(super) struct Registration {
    pub core: Arc<Core>,
    pub device: Id,
    pub id: u64,
}
impl Drop for Registration {
    fn drop(&mut self) {
        if self.core.finish_connection(&self.device, self.id) {
            // This schedules a coalesced uncancelable continuation immediately;
            // cancellation must persist the same last-seen timestamp it publishes.
            drop(self.core.persist_phone(self.id));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::store::members::SetupFence;
    use async_trait::async_trait;
    struct Mock;
    #[async_trait]
    impl PcActions for Mock {
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
    struct Fixture {
        directory: tempfile::TempDir,
        // Core advertises its configured port even when no server is running.
        // Keep the socket bound after a test drops/moves its serving handle, so
        // another parallel fixture cannot inherit a still-advertised address.
        listeners: Vec<std::net::TcpListener>,
    }
    impl Fixture {
        fn path(&self) -> &std::path::Path {
            self.directory.path()
        }
        fn listen(&mut self) -> Result<TcpListener> {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            listener.set_nonblocking(true)?;
            let serving = TcpListener::from_std(listener.try_clone()?)?;
            self.listeners.push(listener);
            Ok(serving)
        }
    }
    async fn core() -> Result<(Fixture, Arc<Core>, TcpListener)> {
        let mut dir = Fixture {
            directory: crate::test_support::scratch()?,
            listeners: Vec::new(),
        };
        let listener = dir.listen()?;
        let paths = Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: dir.path().join("pneud.sock"),
        };
        let config = Config {
            port: listener.local_addr()?.port(),
            save_path: dir.path().join("saved"),
            ..Config::default()
        };
        let core = Core::new(paths, config, Arc::new(Mock)).await?;
        // These protocol mocks implement mesh/PEER_SETTINGS, not membership.
        core.setup_test_capability.store(false, Ordering::SeqCst);
        eprintln!(
            "mock Core PID={} port={} directory={} (no mDNS)",
            std::process::id(),
            core.bound_port(),
            dir.path().display()
        );
        Ok((dir, core, listener))
    }
    #[tokio::test]
    async fn fixture_reserves_advertised_port_after_listener_drop_and_server_shutdown() -> Result<()>
    {
        for serve in [false, true] {
            let (_dir, core, listener) = core().await?;
            let address = listener.local_addr()?;
            assert_eq!(address.port(), core.bound_port());
            if serve {
                let server = Task::spawn(core.clone().serve(listener, false));
                core.request_shutdown();
                server.await??;
            } else {
                drop(listener);
                core.shutdown_actions().await?;
            }
            // Model a competing fixture claiming this port at precisely the
            // old drop-then-dial gap, without depending on kernel allocation.
            let error = TcpListener::bind(address).await.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        }
        Ok(())
    }
    // Real mesh peers now also exchange settings. These mocks acknowledge
    // those independent frames while tests inspect pairing/heartbeat controls.
    async fn read_frame<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        tls: &mut T,
    ) -> Result<Option<Message>> {
        loop {
            match super::read_frame(tls).await? {
                Some(Message::PeerSettings(snapshot)) => {
                    send(
                        tls,
                        &Message::Ack(Ack {
                            transfer_id: snapshot.transfer_id,
                            deduplicated: false,
                        }),
                    )
                    .await?
                }
                message => return Ok(message),
            }
        }
    }
    async fn bind(a: &Core, b: &Core) -> Result<Store> {
        let guard = a.pairing_gate.clone().lock_owned().await;
        a.persist_pairing(&b.identity.device_id, &b.identity.fingerprint, guard, false)
            .await
    }
    #[tokio::test]
    async fn setup_refusal_timestamp_survives_retry_and_clears_on_ready_or_repair() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let (_db, b, _lb) = core().await?;
        bind(&a, &b).await?;
        let device = &b.identity.device_id;
        let generation = a.store.pairing_generation(device).unwrap();
        a.set_link_problem(device, generation, Some(LinkProblem::Unauthorized));
        // Model an earlier authenticated refusal without a wall-clock sleep.
        a.mesh
            .links
            .lock()
            .unwrap()
            .get_mut(device)
            .unwrap()
            .issue
            .as_mut()
            .unwrap()
            .since = 42;
        a.set_link_problem(device, generation, Some(LinkProblem::Unauthorized));
        assert_eq!(a.ui_snapshot().setup_refusal_since, Some(42));
        a.set_link_problem(device, generation, None);
        assert_eq!(a.ui_snapshot().setup_refusal_since, None);
        a.set_link_problem(device, generation, Some(LinkProblem::Unauthorized));
        bind(&a, &b).await?;
        assert_eq!(a.ui_snapshot().setup_refusal_since, None);
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }

    #[tokio::test]
    async fn own_device_id_inbound_hello_rejected_before_pair_request() -> Result<()> {
        for known in [false, true] {
            for role in [None, Some(DeviceRole::Phone), Some(DeviceRole::Computer)] {
                let (_da, a, la) = core().await?;
                let (_db, b, lb) = core().await?;
                drop(lb);
                if known {
                    bind(&a, &b).await?;
                }
                let before = format!("{:?}", a.store.peer_info().await?);
                let address = la.local_addr()?.to_string();
                let invitation = a
                    .start_pairing_with_addresses(vec![address.clone()])
                    .await?;
                let server = Task::spawn(a.clone().serve(la, false));
                let config =
                    crate::tls::client_config(&b.identity, a.identity.fingerprint.clone())?;
                let mut tls = TlsConnector::from(Arc::new(config))
                    .connect(
                        rustls::pki_types::ServerName::try_from("pneu.local")?,
                        TcpStream::connect(address).await?,
                    )
                    .await?;
                let mut forged = b.computer_hello();
                forged.device_id = a.identity.device_id.clone();
                forged.role = role;
                if role.is_none() {
                    forged.capabilities.retain(|c| c != "mesh");
                }
                // Pipeline a valid token to prove HELLO rejects before pairing.
                let mut wire = encode_frame(&Message::Hello(forged))?;
                wire.extend(encode_frame(&Message::PairRequest(PairRequest {
                    token: invitation.token.clone(),
                }))?);
                tokio::io::AsyncWriteExt::write_all(&mut tls, &wire).await?;
                assert!(
                    matches!(read_frame(&mut tls).await?, Some(Message::Error(e)) if e.code == ErrorCode::Unauthorized)
                );
                assert_eq!(format!("{:?}", a.store.peer_info().await?), before);
                assert!(a.connected_devices().is_empty());
                assert_eq!(a.store.pairing_generation(&a.identity.device_id), None);
                assert!(
                    a.admission
                        .lock()
                        .unwrap()
                        .invitation
                        .as_ref()
                        .is_some_and(|i| i.token == invitation.token)
                );
                a.request_shutdown();
                server.await??;
                b.shutdown_actions().await?;
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn own_device_id_pair_request_and_binding_install_leave_invitation_intact() -> Result<()>
    {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let result = a
            .finish_pairing_captured(&a.identity.device_id, &b.identity.fingerprint, &uri.token)
            .await;
        assert!(matches!(result, Err(PairCode::InvalidToken)));
        let guard = a.pairing_gate.clone().lock_owned().await;
        assert!(
            a.persist_pairing(&a.identity.device_id, &b.identity.fingerprint, guard, true)
                .await
                .is_err()
        );
        assert_eq!(a.store.pairing_generation(&a.identity.device_id), None);
        assert!(a.store.peer_info().await?.is_empty());
        assert!(a.paired_devices().is_empty());
        // The rejected request neither consumes the invitation nor poisons it.
        a.finish_pairing_captured(&b.identity.device_id, &b.identity.fingerprint, &uri.token)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(
            a.paired_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        drop((la, lb));
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn own_device_id_outbound_dial_and_join_rejected_before_pair_request() -> Result<()> {
        for join in [false, true] {
            let (_da, a, la) = core().await?;
            let (_db, b, lb) = core().await?;
            let lb = MockListener::new(lb);
            drop(la);
            let address = lb.local_addr()?.to_string();
            let uri = b
                .start_pairing_with_addresses(vec![address.clone()])
                .await?;
            let peer = a.identity.device_id.clone();
            let mock = Task::spawn(async move {
                let mut tls = accept_client_hello(&b, &lb).await?;
                let mut forged = b.computer_hello();
                forged.device_id = peer;
                send(&mut tls, &Message::Hello(forged)).await?;
                assert!(
                    matches!(read_frame(&mut tls).await?, Some(Message::Error(e)) if e.code == ErrorCode::Unauthorized)
                );
                closed_without_unpair(&mut tls).await?;
                b.shutdown_actions().await
            });
            let result = if join {
                let preview = a.join(&uri.build()?).await?;
                let attempt = preview["attempt"].as_str().unwrap();
                a.confirm_join(attempt).await?;
                let pending = a.mesh.join.lock().unwrap().as_ref().unwrap().clone();
                let result = a.clone().join_worker(pending).await;
                assert!(!a.mesh.join.lock().unwrap().as_ref().unwrap().requested);
                result
            } else {
                a.connect_computer(&[address], uri.fingerprint, Some(&a.identity.device_id))
                    .await
                    .map(|_| ())
            };
            let mock = mock_result(mock).await;
            let outcome = format!("dial: {result:?}; mock: {mock:?}");
            assert!(
                result
                    .err()
                    .context(outcome.clone())?
                    .downcast_ref::<crate::client::PcRejection>()
                    .is_some_and(|e| e.0.code == ErrorCode::Unauthorized),
                "{outcome}"
            );
            assert!(a.paired_devices().is_empty());
            assert!(a.store.peer_info().await?.is_empty());
            assert_eq!(a.store.pairing_generation(&a.identity.device_id), None);
            mock?;
            a.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn own_invitation_preview_is_rejected_without_creating_attempt() -> Result<()> {
        let (_dir, a, listener) = core().await?;
        let uri = a
            .start_pairing_with_addresses(vec![listener.local_addr()?.to_string()])
            .await?;
        assert_eq!(a.join(&uri.build()?).await?["state"], "own_code");
        assert!(a.mesh.join.lock().unwrap().is_none());
        assert!(a.paired_devices().is_empty());
        let mut mapped = uri.clone();
        mapped.fingerprint = Hash::new("a".repeat(64))?;
        a.admission
            .lock()
            .unwrap()
            .peers
            .insert(mapped.fingerprint.clone(), a.identity.device_id.clone());
        assert_eq!(a.join(&mapped.build()?).await?["state"], "own_code");
        assert!(a.mesh.join.lock().unwrap().is_none());
        a.shutdown_actions().await
    }
    #[tokio::test]
    async fn own_device_id_registration_rejected_before_session_state_changes() -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        drop(la);
        let address = lb.local_addr()?.to_string();
        let pin = b.identity.fingerprint.clone();
        let mock = Task::spawn(async move {
            let mut tls = accept_hello(&b, &lb, None).await?;
            assert!(
                matches!(read_frame(&mut tls).await?, Some(Message::Error(e)) if e.code == ErrorCode::Unauthorized)
            );
            b.shutdown_actions().await
        });
        let ComputerLink {
            tls,
            mut hello,
            remote,
            permit,
            transport,
            ..
        } = a.connect_computer(&[address], pin.clone(), None).await?;
        hello.device_id = a.identity.device_id.clone();
        a.clone()
            .register(
                tls,
                hello,
                pin,
                remote,
                permit,
                a.store.with_current_pairing(),
                true,
                false,
            )
            .await?;
        drop(transport);
        assert!(a.connected_devices().is_empty());
        assert!(a.sessions.lock().unwrap().sessions.is_empty());
        assert!(a.store.peer_info().await?.is_empty());
        mock.await??;
        a.shutdown_actions().await
    }
    #[tokio::test]
    async fn settings_reconnect_wire_replay_is_deduplicated_and_unblocks_content_after_restart()
    -> Result<()> {
        let (mut da, mut a, mut la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(lb);
        bind(&a, &b).await?;
        bind(&b, &a).await?;
        let mut received = None;
        for reconnect in 0..3 {
            let address = la.local_addr()?.to_string();
            let server = Task::spawn(a.clone().serve(la, false));
            let ComputerLink { mut tls, .. } = b
                .connect_computer(
                    &[address],
                    a.identity.fingerprint.clone(),
                    Some(&a.identity.device_id),
                )
                .await?;
            let snapshot = b
                .store
                .settings_outgoing(
                    b.identity.device_id.clone(),
                    a.identity.device_id.clone(),
                    false,
                )
                .await?;
            send(&mut tls, &Message::PeerSettings(snapshot.clone())).await?;
            let mut acknowledged = false;
            let mut sent = None;
            tokio::time::timeout(Duration::from_secs(3), async {
                while !acknowledged || sent.is_none() {
                    match super::read_frame(&mut tls).await?.context("settings EOF")? {
                        Message::PeerSettings(outgoing) => {
                            let (deduplicated, _) = b
                                .store
                                .settings_merge(
                                    b.identity.device_id.clone(),
                                    a.identity.device_id.clone(),
                                    false,
                                    outgoing.clone(),
                                )
                                .await?;
                            assert_eq!(deduplicated, reconnect != 0);
                            send(
                                &mut tls,
                                &Message::Ack(Ack {
                                    transfer_id: outgoing.transfer_id.clone(),
                                    deduplicated,
                                }),
                            )
                            .await?;
                            sent = Some(outgoing);
                        }
                        Message::Ack(ack) => {
                            assert_eq!(ack.transfer_id, snapshot.transfer_id);
                            assert_eq!(ack.deduplicated, reconnect != 0);
                            b.store
                                .settings_ack(a.identity.device_id.clone(), ack.transfer_id)
                                .await?;
                            acknowledged = true;
                        }
                        Message::Ping(p) => send(&mut tls, &Message::Pong(p)).await?,
                        other => bail!("unexpected settings exchange: {other:?}"),
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            let sent = sent.unwrap();
            if let Some(previous) = &received {
                assert_eq!(
                    &sent, previous,
                    "acknowledged outgoing ID survives a Core restart"
                );
            }
            received = Some(sent);
            until(|| {
                a.settings_ready
                    .lock()
                    .unwrap()
                    .contains_key(&b.identity.device_id)
            })
            .await?;
            // Complete shutdown/reopen with the same database and identity.
            let paths = a.paths.clone();
            let mut config = a.config();
            a.request_shutdown();
            server.await??;
            drop((tls, a));
            la = da.listen()?;
            config.port = la.local_addr()?.port();
            a = Core::new(paths, config, Arc::new(Mock)).await?;
            a.setup_test_capability.store(false, Ordering::SeqCst);
        }
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    async fn wait_established(core: &Core, device: &Id) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !core.store.peer_established(device.clone()).await? {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }
    async fn until(mut condition: impl FnMut() -> bool) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    }
    async fn closed_without_unpair(tls: &mut TlsStream<TcpStream>) -> Result<()> {
        match tokio::time::timeout(Duration::from_secs(2), read_frame(tls)).await? {
            Ok(None) => Ok(()),
            Err(e) if e.to_string().contains("peer closed connection") => Ok(()),
            other => bail!("expected local close with no UNPAIR: {other:?}"),
        }
    }
    async fn accept_join_request(
        a: &Core,
        b: &Core,
        la: &MockListener,
    ) -> Result<TlsStream<TcpStream>> {
        let mut tls = accept_hello(a, la, None).await?;
        let token = match read_frame(&mut tls).await? {
            Some(Message::PairRequest(p)) => p.token,
            m => bail!("PAIR_REQUEST expected: {m:?}"),
        };
        if a.admission
            .lock()
            .unwrap()
            .peers
            .get(&b.identity.fingerprint)
            != Some(&b.identity.device_id)
        {
            a.finish_pairing_captured(&b.identity.device_id, &b.identity.fingerprint, &token)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        Ok(tls)
    }
    async fn accepted(tls: &mut TlsStream<TcpStream>) -> Result<()> {
        send(
            tls,
            &Message::PairResult(PairResult {
                accepted: true,
                code: None,
                message: None,
            }),
        )
        .await
    }
    #[tokio::test]
    async fn pairing_connection_pong_is_not_established_evidence() -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, lb) = core().await?;
        drop(lb);
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        let pending = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let worker = Task::spawn(b.clone().join_worker(pending));
        let mut tls = accept_join_request(&a, &b, &la).await?;
        let nonce = Nonce::new(crate::random_hex(8))?;
        send(
            &mut tls,
            &Message::Ping(Ping {
                nonce: nonce.clone(),
            }),
        )
        .await?;
        assert!(matches!(read_frame(&mut tls).await?, Some(Message::Pong(p)) if p.nonce == nonce));
        assert!(
            !a.store
                .peer_established(b.identity.device_id.clone())
                .await?
        );
        assert!(b.paired_devices().is_empty());
        accepted(&mut tls).await?;
        until(|| b.connected_devices().contains(&a.identity.device_id)).await?;
        assert!(
            !b.store
                .peer_established(a.identity.device_id.clone())
                .await?
        );
        // Drain READY setup and synchronize the writer before advancing time.
        let barrier = Nonce::new(crate::random_hex(8))?;
        send(
            &mut tls,
            &Message::Ping(Ping {
                nonce: barrier.clone(),
            }),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match read_frame(&mut tls).await? {
                    Some(Message::Pong(p)) if p.nonce == barrier => break,
                    Some(Message::Status(_)) => {}
                    m => bail!("READY barrier expected: {m:?}"),
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        // Even a verified PONG to the joiner's READY heartbeat proves nothing
        // new about the joiner. Only its peer's acceptor readiness can do so.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::time::resume();
        let ping = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match read_frame(&mut tls).await? {
                    Some(Message::Ping(p)) => return Ok::<_, anyhow::Error>(p),
                    Some(Message::Status(_)) => {}
                    m => bail!("heartbeat expected: {m:?}"),
                }
            }
        })
        .await??;
        send(&mut tls, &Message::Pong(ping)).await?;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(
            !b.store
                .peer_established(a.identity.device_id.clone())
                .await?
        );
        assert!(
            !a.store
                .peer_established(b.identity.device_id.clone())
                .await?
        );
        assert_eq!(
            a.paired_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        assert_eq!(
            b.paired_devices(),
            std::slice::from_ref(&a.identity.device_id)
        );
        worker.abort();
        let _ = worker.await;
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn joiner_ready_ping_establishes_inviter_binding() -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(lb);
        let address = la.local_addr()?.to_string();
        let uri = a
            .start_pairing_with_addresses(vec![address.clone()])
            .await?;
        let server = Task::spawn(a.clone().serve(la, false));
        let ComputerLink { mut tls, .. } = b
            .connect_computer(&[address], uri.fingerprint, None)
            .await?;
        send(
            &mut tls,
            &Message::PairRequest(PairRequest { token: uri.token }),
        )
        .await?;
        assert!(matches!(read_frame(&mut tls).await?, Some(Message::PairResult(r)) if r.accepted));
        assert!(
            !a.store
                .peer_established(b.identity.device_id.clone())
                .await?
        );
        assert!(b.paired_devices().is_empty());
        bind(&b, &a).await?;
        let nonce = Nonce::new(crate::random_hex(8))?;
        send(
            &mut tls,
            &Message::Ping(Ping {
                nonce: nonce.clone(),
            }),
        )
        .await?;
        assert!(matches!(read_frame(&mut tls).await?, Some(Message::Pong(p)) if p.nonce == nonce));
        wait_established(&a, &b.identity.device_id).await?;
        assert!(
            !b.store
                .peer_established(a.identity.device_id.clone())
                .await?
        );
        assert_eq!(
            a.paired_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        assert_eq!(
            b.paired_devices(),
            std::slice::from_ref(&a.identity.device_id)
        );
        drop(tls);
        a.request_shutdown();
        server.await??;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn cancel_before_request_preserves_both_endpoints_other_bindings() -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, lb) = core().await?;
        let (_dc, c, lc) = core().await?;
        drop(lb);
        drop(lc);
        bind(&a, &c).await?;
        bind(&b, &c).await?;
        let a_before = format!("{:?}", a.store.peer_info().await?);
        let b_before = format!("{:?}", b.store.peer_info().await?);
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        let supervisor = Task::spawn(b.clone().mesh_supervise());
        // Stall before TLS/HELLO, so cancellation races a live connection but
        // no PAIR_REQUEST can have been sent.
        let (mut tcp, _) = accept_tls_tcp(&la).await?;
        b.cancel_join(attempt).await?;
        assert_eq!(b.join_status(attempt)?["state"], "cancelled");
        until(|| b.transports.available_permits() == TRANSPORT_LIMIT).await?;
        let mut bytes = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut tcp, &mut bytes),
        )
        .await??;
        assert_eq!(format!("{:?}", a.store.peer_info().await?), a_before);
        assert_eq!(format!("{:?}", b.store.peer_info().await?), b_before);
        assert!(a.pairing_status().1.is_some());
        b.cancel_join(attempt).await?;
        b.request_shutdown();
        supervisor.await??;
        a.shutdown_actions().await?;
        b.shutdown_actions().await?;
        c.shutdown_actions().await
    }
    #[tokio::test]
    async fn incoming_replacement_survives_old_attempt_retry_on_both_endpoints() -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, lb) = core().await?;
        drop(lb);
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        let pending = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let first = Task::spawn(b.clone().join_worker(pending));
        let tls = accept_join_request(&a, &b, &la).await?;
        drop(tls); // Lost PAIR_RESULT; A has installed B, B has not installed A.
        assert!(first.await?.is_err());
        assert!(b.paired_devices().is_empty());
        // An incoming pairing on B replaces the pending attempt's authority.
        let incoming = b
            .start_pairing_with_addresses(vec!["127.0.0.1:54321".into()])
            .await?;
        b.finish_pairing_captured(
            &a.identity.device_id,
            &a.identity.fingerprint,
            &incoming.token,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        bind(&a, &b).await?;
        let a_before = format!("{:?}", a.store.peer_info().await?);
        let b_before = format!("{:?}", b.store.peer_info().await?);
        let pending = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let retry = Task::spawn(b.clone().join_worker(pending));
        let mut tls = accept_join_request(&a, &b, &la).await?;
        accepted(&mut tls).await?;
        retry.await??;
        closed_without_unpair(&mut tls).await?;
        assert_eq!(b.join_status(attempt)?["state"], "paired");
        assert_eq!(format!("{:?}", a.store.peer_info().await?), a_before);
        assert_eq!(format!("{:?}", b.store.peer_info().await?), b_before);
        assert_eq!(
            a.paired_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        assert_eq!(
            b.paired_devices(),
            std::slice::from_ref(&a.identity.device_id)
        );
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn unrelated_pairing_does_not_void_install_fence() -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, lb) = core().await?;
        let (_dc, c, lc) = core().await?;
        drop(lb);
        drop(lc);
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        let pending = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let worker = Task::spawn(b.clone().join_worker(pending));
        let mut tls = accept_join_request(&a, &b, &la).await?;
        bind(&b, &c).await?;
        let c_generation = b.store.pairing_generation(&c.identity.device_id);
        accepted(&mut tls).await?;
        until(|| b.join_status(attempt).unwrap()["state"] == "paired").await?;
        assert_eq!(
            a.paired_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        assert!(b.paired_devices().contains(&a.identity.device_id));
        assert!(b.paired_devices().contains(&c.identity.device_id));
        assert_eq!(
            b.store.pairing_generation(&c.identity.device_id),
            c_generation
        );
        worker.abort();
        let _ = worker.await;
        a.shutdown_actions().await?;
        b.shutdown_actions().await?;
        c.shutdown_actions().await
    }
    #[tokio::test]
    async fn restart_abandons_attempt_and_fresh_invitation_reuses_unestablished_binding()
    -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, mut b, lb) = core().await?;
        drop(lb);
        let address = la.local_addr()?.to_string();
        let uri = a
            .start_pairing_with_addresses(vec![address.clone()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        let pending = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let first = Task::spawn(b.clone().join_worker(pending));
        let tls = accept_join_request(&a, &b, &la).await?;
        drop(tls);
        assert!(first.await?.is_err());
        let generation = a.store.pairing_generation(&b.identity.device_id);
        assert!(
            !a.store
                .peer_established(b.identity.device_id.clone())
                .await?
        );
        assert!(b.paired_devices().is_empty());
        let paths = b.paths.clone();
        let config = b.config();
        b.shutdown_actions().await?;
        b.store.drain().await;
        drop(b);
        b = Core::new(paths, config, Arc::new(Mock)).await?;
        b.setup_test_capability.store(false, Ordering::SeqCst);
        assert!(b.join_status(attempt).is_err());
        assert!(!b.paths.data.join("join-pending.json").exists());
        let fresh = a.start_pairing_with_addresses(vec![address]).await?;
        assert_ne!(uri.token, fresh.token);
        let server = Task::spawn(a.clone().serve(la.into_inner(), false));
        let preview = b.join(&fresh.build()?).await?;
        let next = preview["attempt"].as_str().unwrap();
        b.confirm_join(next).await?;
        let pending = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let worker = Task::spawn(b.clone().join_worker(pending));
        until(|| b.join_status(next).unwrap()["state"] == "paired" || worker.is_finished()).await?;
        if b.join_status(next)?["state"] != "paired" {
            worker.await??;
            bail!("join worker ended before installation");
        }
        assert_eq!(
            a.paired_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        assert_eq!(
            b.paired_devices(),
            std::slice::from_ref(&a.identity.device_id)
        );
        assert_eq!(
            a.store.pairing_generation(&b.identity.device_id),
            generation
        );
        worker.abort();
        let _ = worker.await;
        a.request_shutdown();
        server.await??;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn local_join_storage_error_preserves_supervisor_and_other_link() -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, lb) = core().await?;
        let (_dc, c, lc) = core().await?;
        drop(lb);
        computer_route(&b, &c, lc.local_addr()?.to_string()).await?;
        bind(&c, &b).await?;
        let c_server = Task::spawn(c.clone().serve(lc, false));
        let supervisor = Task::spawn(b.clone().mesh_supervise());
        until(|| b.connected_devices().contains(&c.identity.device_id)).await?;
        let b_generation = b.store.pairing_generation(&c.identity.device_id);
        let c_generation = c.store.pairing_generation(&b.identity.device_id);
        let peer = a.identity.device_id.as_str().to_owned();
        b.store.call(move |db| {
            db.execute_batch(&format!("CREATE TRIGGER fail_join BEFORE INSERT ON peers WHEN NEW.id='{peer}' BEGIN SELECT RAISE(ABORT,'injected join storage failure'); END;"))?;
            Ok(())
        }).await?;
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        let mut tls = accept_join_request(&a, &b, &la).await?;
        accepted(&mut tls).await?;
        until(|| b.join_status(attempt).unwrap()["state"] == "unreachable").await?;
        closed_without_unpair(&mut tls).await?;
        assert!(!supervisor.is_finished());
        assert_eq!(
            b.connected_devices(),
            std::slice::from_ref(&c.identity.device_id)
        );
        assert_eq!(
            c.connected_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        assert_eq!(
            b.store.pairing_generation(&c.identity.device_id),
            b_generation
        );
        assert_eq!(
            c.store.pairing_generation(&b.identity.device_id),
            c_generation
        );
        assert_eq!(
            a.paired_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        b.store
            .call(|db| {
                db.execute_batch("DROP TRIGGER fail_join;")?;
                Ok(())
            })
            .await?;
        b.confirm_join(attempt).await?;
        let a_server = Task::spawn(a.clone().serve(la.into_inner(), false));
        until(|| b.join_status(attempt).unwrap()["state"] == "paired").await?;
        assert!(b.connected_devices().contains(&c.identity.device_id));
        assert!(b.paired_devices().contains(&a.identity.device_id));
        a.request_shutdown();
        b.request_shutdown();
        c.request_shutdown();
        supervisor.await??;
        a_server.await??;
        c_server.await??;
        Ok(())
    }
    #[tokio::test]
    async fn cancellation_after_request_finishes_locally_without_unpair() -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, lb) = core().await?;
        drop(lb);
        let (_dc, c, lc) = core().await?;
        drop(lc);
        bind(&a, &c).await?;
        bind(&b, &c).await?;
        let a_generation = a.store.pairing_generation(&c.identity.device_id);
        let b_generation = b.store.pairing_generation(&c.identity.device_id);
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        let join = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let worker = Task::spawn(b.clone().join_worker(join));
        let mut tls = accept_hello(&a, &la, None).await?;
        let token = match read_frame(&mut tls).await? {
            Some(Message::PairRequest(p)) => p.token,
            m => panic!("PAIR_REQUEST expected: {m:?}"),
        };
        a.finish_pairing_captured(&b.identity.device_id, &b.identity.fingerprint, &token)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        b.cancel_join(attempt).await?;
        assert_eq!(b.join_status(attempt)?["state"], "cancelled");
        send(
            &mut tls,
            &Message::PairResult(PairResult {
                accepted: true,
                code: None,
                message: None,
            }),
        )
        .await?;
        assert!(worker.await?.is_err());
        closed_without_unpair(&mut tls).await?;
        assert!(a.paired_devices().contains(&b.identity.device_id));
        assert!(a.paired_devices().contains(&c.identity.device_id));
        assert_eq!(
            b.paired_devices(),
            std::slice::from_ref(&c.identity.device_id)
        );
        assert_eq!(
            a.store.pairing_generation(&c.identity.device_id),
            a_generation
        );
        assert_eq!(
            b.store.pairing_generation(&c.identity.device_id),
            b_generation
        );
        c.shutdown_actions().await?;
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn lifecycle_change_while_pair_result_is_in_flight_cannot_install_trust() -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, lb) = core().await?;
        drop(lb);
        bind(&b, &a).await?;
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        b.confirm_join(preview["attempt"].as_str().unwrap()).await?;
        let join = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let worker = Task::spawn(b.clone().join_worker(join));
        let mut tls = accept_join_request(&a, &b, &la).await?;
        let generation = a.store.pairing_generation(&b.identity.device_id);
        b.unpair_device(a.identity.device_id.clone()).await?;
        send(
            &mut tls,
            &Message::PairResult(PairResult {
                accepted: true,
                code: None,
                message: None,
            }),
        )
        .await?;
        assert!(worker.await?.is_ok());
        closed_without_unpair(&mut tls).await?;
        drop(tls);
        assert!(b.paired_devices().is_empty());
        assert_eq!(
            a.paired_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        assert_eq!(
            a.store.pairing_generation(&b.identity.device_id),
            generation
        );
        assert_eq!(
            b.join_status(preview["attempt"].as_str().unwrap())?["state"],
            "cancelled"
        );
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn storage_denial_and_missing_rejection_code_keep_same_attempt_retryable() -> Result<()> {
        for code in [Some(PairCode::StorageFailed), None] {
            let (_da, a, la) = core().await?;
            let la = MockListener::new(la);
            let (_db, b, lb) = core().await?;
            drop(lb);
            let uri = a
                .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
                .await?;
            let preview = b.join(&uri.build()?).await?;
            let attempt = preview["attempt"].as_str().unwrap();
            b.confirm_join(attempt).await?;
            let supervisor = Task::spawn(b.clone().mesh_supervise());
            let mut tls = accept_hello(&a, &la, None).await?;
            assert!(matches!(
                read_frame(&mut tls).await?,
                Some(Message::PairRequest(_))
            ));
            if code.is_none() {
                let payload = br#"{"accepted":false}"#;
                tls.write_all(&((payload.len() + 1) as u32).to_be_bytes())
                    .await?;
                tls.write_all(&[3]).await?;
                tls.write_all(payload).await?;
            } else {
                send(
                    &mut tls,
                    &Message::PairResult(PairResult {
                        accepted: false,
                        code,
                        message: None,
                    }),
                )
                .await?;
            }
            drop(tls);
            until(|| b.join_status(attempt).unwrap()["state"] == "unreachable").await?;
            assert!(!b.paths.data.join("join-pending.json").exists());
            assert!(b.join(&uri.build()?).await.is_err());
            b.request_shutdown();
            supervisor.await??;
            a.shutdown_actions().await?;
            b.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn simultaneous_computers_converge_and_stale_dial_cannot_replace_repair() -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        let aa = la.local_addr()?.to_string();
        let ab = lb.local_addr()?.to_string();
        let captured_a = bind(&a, &b).await?;
        let captured_b = bind(&b, &a).await?;
        let sa = tokio::spawn(a.clone().serve(la, false));
        let sb = tokio::spawn(b.clone().serve(lb, false));
        // Both initiators complete HELLO before either publishes its outbound
        // READY. Each acceptor has already registered the opposite transport.
        let addrs_a = [ab.clone()];
        let addrs_b = [aa];
        let (ca, cb) = tokio::join!(
            a.connect_computer(
                &addrs_a,
                b.identity.fingerprint.clone(),
                Some(&b.identity.device_id)
            ),
            b.connect_computer(
                &addrs_b,
                a.identity.fingerprint.clone(),
                Some(&a.identity.device_id)
            )
        );
        let ComputerLink {
            tls: ta,
            hello: ha,
            remote: ra,
            permit: pa,
            transport: transport_a,
            ..
        } = ca?;
        let ComputerLink {
            tls: tb,
            hello: hb,
            remote: rb,
            permit: pb,
            transport: transport_b,
            ..
        } = cb?;
        until(|| a.connected_devices().len() == 1 && b.connected_devices().len() == 1).await?;
        let a_is_lower = a.identity.device_id.as_str() < b.identity.device_id.as_str();
        // Keep the lower ID's inbound socket alive until the higher ID has
        // finished rejecting its outbound duplicate. Topology convergence alone
        // does not prove that rejection finished: replacing the inbound socket
        // or shutting down first can make its ERROR write fail with BrokenPipe.
        let registration_gate = if a_is_lower { &a } else { &b }.pairing_gate.lock().await;
        let ca = a.clone();
        let fa = b.identity.fingerprint.clone();
        let old = captured_a.clone();
        let wa = tokio::spawn(async move {
            let _transport = transport_a;
            ca.register(ta, ha, fa, ra, pa, captured_a, true, true)
                .await
        });
        let cb = b.clone();
        let fb = a.identity.fingerprint.clone();
        let wb = tokio::spawn(async move {
            let _transport = transport_b;
            cb.register(tb, hb, fb, rb, pb, captured_b, true, true)
                .await
        });
        let winner = if a_is_lower {
            tokio::time::timeout(Duration::from_secs(5), wb).await???;
            wa
        } else {
            tokio::time::timeout(Duration::from_secs(5), wa).await???;
            wb
        };
        drop(registration_gate);
        let lower = if a_is_lower {
            a.identity.device_id.clone()
        } else {
            b.identity.device_id.clone()
        };
        until(|| {
            [a.clone(), b.clone()].iter().all(|c| {
                c.ui_snapshot()
                    .devices
                    .first()
                    .is_some_and(|d| d.dialer.as_ref() == Some(&lower))
            })
        })
        .await?;
        assert_eq!(a.sessions.lock().unwrap().sessions.len(), 1);
        assert_eq!(b.sessions.lock().unwrap().sessions.len(), 1);
        assert_eq!(a.paired_devices().len(), 1);
        // A published session can still be writing its initial settings.
        // Finish that exchange before re-pairing closes the winning transport.
        until(|| {
            [(&a, &b.identity.device_id), (&b, &a.identity.device_id)]
                .iter()
                .all(|(core, peer)| {
                    let sessions = core.sessions.lock().unwrap();
                    sessions.sessions.get(peer).is_some_and(|session| {
                        core.settings_ready.lock().unwrap().get(peer) == Some(&session.id)
                    })
                })
        })
        .await?;
        let replacement = bind(&a, &b).await?;
        assert_ne!(
            old.pairing_generation(&b.identity.device_id),
            replacement.pairing_generation(&b.identity.device_id)
        );
        assert!(
            old.remember_route(b.identity.device_id.clone(), ab)
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(5), winner).await???;
        a.request_shutdown();
        b.request_shutdown();
        sa.await??;
        sb.await??;
        Ok(())
    }
    #[tokio::test]
    async fn higher_id_one_way_dial_survives_and_unpair_stops_worker() -> Result<()> {
        let (_da, mut a, la) = core().await?;
        let (_db, mut b, lb) = core().await?;
        // Either randomly generated identity may be lower; choose the dialer.
        let (listener_a, listener_b) =
            if a.identity.device_id.as_str() > b.identity.device_id.as_str() {
                (la, lb)
            } else {
                std::mem::swap(&mut a, &mut b);
                (lb, la)
            };
        bind(&a, &b).await?;
        bind(&b, &a).await?;
        let captured = a.store.with_current_pairing();
        captured
            .remember_route(
                b.identity.device_id.clone(),
                listener_b.local_addr()?.to_string(),
            )
            .await?;
        a.store
            .call(|c| {
                c.execute("UPDATE peers SET role='computer'", [])?;
                Ok(())
            })
            .await?;
        let peer = a.store.peer_info().await?.remove(0);
        // Only B serves: A keeps its advertised port reserved but never accepts
        // reverse dials. This deliberately exercises a lone higher dial.
        let sb = tokio::spawn(b.clone().serve(listener_b, false));
        drop(listener_a);
        let (cancel, rx) = watch::channel(false);
        let worker = tokio::spawn(a.clone().dial_worker(peer, rx));
        until(|| a.connected_devices().len() == 1 && b.connected_devices().len() == 1).await?;
        assert_eq!(
            b.ui_snapshot().devices[0].dialer.as_ref(),
            Some(&a.identity.device_id)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        b.unpair_device(a.identity.device_id.clone()).await?;
        until(|| a.paired_devices().is_empty()).await?;
        tokio::time::timeout(Duration::from_secs(2), worker).await???;
        assert!(a.connected_devices().is_empty());
        assert!(cancel.receiver_count() == 0);
        b.request_shutdown();
        sb.await??;
        a.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn dial_failures_keep_bindings_expose_ipc_states_and_back_off() -> Result<()> {
        for (failure, state) in [
            ("hello_version", "update_required"),
            ("error_version", "update_required"),
            ("ready_version", "update_required"),
            ("unauthorized", "unauthorized"),
            ("pin", "fingerprint_mismatch"),
            ("binding", "binding_mismatch"),
        ] {
            let (_da, a, la) = core().await?;
            let (_db, b, lb) = core().await?;
            let lb = MockListener::new(lb);
            let (_dc, c, lc) = core().await?;
            drop(la);
            drop(lc);
            computer_route(&a, &b, lb.local_addr()?.to_string()).await?;
            bind(&b, &a).await?;
            let generation = a.store.pairing_generation(&b.identity.device_id);
            a.store
                .enqueue_clipboard(b.identity.device_id.clone(), "queued text".into())
                .await?;
            let peer = a.store.peer_info().await?.remove(0);
            let (cancel, rx) = watch::channel(false);
            let worker = Task::spawn(a.clone().dial_worker(peer, rx));
            let mut tls = if failure == "pin" {
                assert!(accept_client_hello(&c, &lb).await.is_err());
                None
            } else {
                let mut tls = accept_client_hello(&b, &lb).await?;
                match failure {
                    "error_version" => send_error(&mut tls, ErrorCode::VersionMismatch).await?,
                    "unauthorized" => send_error(&mut tls, ErrorCode::Unauthorized).await?,
                    _ => {
                        let mut hello = b.computer_hello();
                        if failure == "hello_version" {
                            hello.version = 2;
                        }
                        if failure == "binding" {
                            hello.device_id = c.identity.device_id.clone();
                        }
                        send(&mut tls, &Message::Hello(hello)).await?;
                        if failure == "ready_version" {
                            assert!(matches!(
                                read_frame(&mut tls).await?,
                                Some(Message::Ping(_))
                            ));
                            send_error(&mut tls, ErrorCode::VersionMismatch).await?;
                        }
                    }
                }
                Some(tls)
            };
            until(|| a.ui_snapshot().devices[0].state == state).await?;
            assert_eq!(
                a.paired_devices(),
                std::slice::from_ref(&b.identity.device_id),
                "{failure}"
            );
            assert_eq!(
                a.store.pairing_generation(&b.identity.device_id),
                generation
            );
            assert!(
                a.store
                    .first_outgoing(b.identity.device_id.clone())
                    .await?
                    .is_some()
            );
            // Both IPC consumers retain the same distinct state.
            let (listener, _owner) = crate::ipc::bind(&a.paths.socket)?;
            let ipc = Task::spawn(crate::ipc::serve(
                a.clone(),
                listener,
                a.shutdown.subscribe(),
            ));
            let status = crate::ipc::request(&a.paths.socket, crate::ipc::Command::Status).await?;
            assert_eq!(status["devices"][0]["state"], state);
            let status =
                crate::ipc::request(&a.paths.socket, crate::ipc::Command::UiStatus).await?;
            assert_eq!(status["snapshot"]["devices"][0]["state"], state);
            drop(tls.take());
            // A metadata notification must not turn a five-minute retry into
            // an immediate reconnect. Exercise time, not a delay constant.
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(120)).await;
            tokio::time::resume();
            a.mesh.changed.notify_waiters();
            no_tls_dial(&lb, Duration::from_millis(50))
                .await
                .with_context(|| format!("{failure} retried too soon"))?;
            cancel.send_replace(true);
            worker.await??;
            a.request_shutdown();
            ipc.await??;
            a.shutdown_actions().await?;
            b.shutdown_actions().await?;
            c.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn inbound_version_mismatch_is_authenticated_visible_and_cleared_on_recovery()
    -> Result<()> {
        for role in [DeviceRole::Phone, DeviceRole::Computer] {
            for variant in ["hello", "error", "forged_identity"] {
                let (_da, a, la) = core().await?;
                let (_db, b, lb) = core().await?;
                drop(lb);
                bind(&a, &b).await?;
                let address = la.local_addr()?.to_string();
                let server = Task::spawn(a.clone().serve(la, false));
                let config =
                    crate::tls::client_config(&b.identity, a.identity.fingerprint.clone())?;
                let tcp = TcpStream::connect(&address).await?;
                let mut tls: TlsStream<TcpStream> = TlsConnector::from(Arc::new(config))
                    .connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)
                    .await?
                    .into();
                if variant == "error" {
                    send_error(&mut tls, ErrorCode::VersionMismatch).await?;
                    until(|| a.ui_snapshot().devices[0].state == "update_required").await?;
                } else {
                    let mut hello = b.computer_hello();
                    hello.role = Some(role);
                    hello.version = 2;
                    if variant == "forged_identity" {
                        hello.device_id = Id::new(crate::random_hex(16))?;
                    }
                    send(&mut tls, &Message::Hello(hello)).await?;
                    assert!(
                        matches!(read_frame(&mut tls).await?, Some(Message::Error(e)) if e.code == ErrorCode::VersionMismatch)
                    );
                }
                assert_eq!(
                    a.ui_snapshot().devices[0].state,
                    if variant == "forged_identity" {
                        "disconnected"
                    } else {
                        "update_required"
                    }
                );
                assert_eq!(a.paired_devices(), vec![b.identity.device_id.clone()]);
                drop(tls);
                // A valid subsequent READY clears the presentation failure.
                let ComputerLink { mut tls, .. } = b
                    .connect_computer(
                        &[address],
                        a.identity.fingerprint.clone(),
                        Some(&a.identity.device_id),
                    )
                    .await?;
                b.confirm_peer_ready(&mut tls).await?;
                assert_eq!(a.ui_snapshot().devices[0].state, "connected");
                // PONG confirms the reader is live; durable readiness is
                // persisted independently and may still be in flight.
                wait_established(&a, &b.identity.device_id).await?;
                assert!(
                    a.store
                        .peer_established(b.identity.device_id.clone())
                        .await?
                );
                drop(tls);
                a.request_shutdown();
                server.await??;
                b.shutdown_actions().await?;
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn established_remote_revocation_is_detected_after_restart() -> Result<()> {
        let (_da, mut a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        drop(la);
        computer_route(&a, &b, lb.local_addr()?.to_string()).await?;
        bind(&b, &a).await?;
        let peer = a.store.peer_info().await?.remove(0);
        let (cancel, rx) = watch::channel(false);
        let worker = Task::spawn(a.clone().dial_worker(peer, rx));
        let tls = accept_ready(&b, &lb, None).await?;
        until(|| !a.connected_devices().is_empty()).await?;
        wait_established(&a, &b.identity.device_id).await?;
        assert_eq!(
            b.paired_devices(),
            std::slice::from_ref(&a.identity.device_id)
        );
        cancel.send_replace(true);
        worker.await??;
        drop(tls);
        let id = a.identity.device_id.clone();
        let paths = a.paths.clone();
        let config = a.config();
        a.shutdown_actions().await?;
        a.store.drain().await;
        drop(a);
        // Remote revocation occurs while A is stopped.
        b.unpair_device(id).await?;
        a = Core::new(paths, config, Arc::new(Mock)).await?;
        a.setup_test_capability.store(false, Ordering::SeqCst);
        let server = Task::spawn(b.clone().serve(lb.into_inner(), false));
        let supervisor = Task::spawn(a.clone().mesh_supervise());
        until(|| a.paired_devices().is_empty()).await?;
        assert!(b.paired_devices().is_empty());
        a.request_shutdown();
        b.request_shutdown();
        supervisor.await??;
        server.await??;
        a.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn unconfirmed_reverse_dial_denial_after_restart_keeps_binding() -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        drop(la);
        computer_route(&a, &b, lb.local_addr()?.to_string()).await?;
        let paths = a.paths.clone();
        let config = a.config();
        let generation = a.store.pairing_generation(&b.identity.device_id);
        a.shutdown_actions().await?;
        a.store.drain().await;
        drop(a);
        let a = Core::new(paths, config, Arc::new(Mock)).await?;
        a.setup_test_capability.store(false, Ordering::SeqCst);
        let peer = a.store.peer_info().await?.remove(0);
        let (cancel, rx) = watch::channel(false);
        let worker = Task::spawn(a.clone().dial_worker(peer, rx));
        let mut tls = accept_client_hello(&b, &lb).await?;
        send_error(&mut tls, ErrorCode::Unauthorized).await?;
        until(|| a.ui_snapshot().devices[0].state == "unauthorized").await?;
        assert_eq!(
            a.store.pairing_generation(&b.identity.device_id),
            generation
        );
        assert_eq!(
            a.paired_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        assert!(b.paired_devices().is_empty());
        assert!(
            !a.store
                .peer_established(b.identity.device_id.clone())
                .await?
        );
        cancel.send_replace(true);
        worker.await??;
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    async fn accept_hello(
        core: &Core,
        listener: &MockListener,
        addresses: Option<Vec<String>>,
    ) -> Result<TlsStream<TcpStream>> {
        accept_hello_version(core, listener, addresses, 1).await
    }
    async fn accept_hello_version(
        core: &Core,
        listener: &MockListener,
        addresses: Option<Vec<String>>,
        version: u32,
    ) -> Result<TlsStream<TcpStream>> {
        let mut tls = accept_client_hello(core, listener).await?;
        let mut hello = core.computer_hello();
        hello.version = version;
        hello.addresses = addresses;
        send(&mut tls, &Message::Hello(hello)).await?;
        Ok(tls)
    }
    type Candidate = futures_util::future::BoxFuture<
        'static,
        (TcpStream, SocketAddr, Vec<u8>, std::io::Result<()>),
    >;
    struct MockListener {
        tcp: TcpListener,
        candidates: tokio::sync::Mutex<futures_util::stream::FuturesUnordered<Candidate>>,
    }
    impl MockListener {
        fn new(tcp: TcpListener) -> Self {
            Self {
                tcp,
                candidates: tokio::sync::Mutex::new(futures_util::stream::FuturesUnordered::new()),
            }
        }
        fn local_addr(&self) -> std::io::Result<SocketAddr> {
            self.tcp.local_addr()
        }
        fn into_inner(self) -> TcpListener {
            self.tcp
        }
    }
    // Other host processes can reach these shared loopback listeners. Select
    // the first handshake record carrying the dialers' encoded pneu.local SNI
    // extension; rustls and the protocol assertions still validate that socket
    // strictly, with no retry after selection. Concurrent reads let silent or
    // partial candidates wait without polling or delaying another dialer.
    // The listener owns candidate futures, preserving their sockets and read
    // progress across selection or cancellation until the listener is dropped.
    async fn accept_tls_tcp(listener: &MockListener) -> Result<(TcpStream, Vec<u8>)> {
        use futures_util::{FutureExt, StreamExt};
        use tokio::io::AsyncReadExt;
        // server_name extension: type, extension length, list length, name type,
        // name length, name. Match its encoding, not an arbitrary name substring.
        const SNI: &[u8] = b"\x00\x00\x00\x0f\x00\x0d\x00\x00\x0apneu.local";
        let local = listener.local_addr()?;
        let mut candidates = listener.candidates.lock().await;
        loop {
            tokio::select! {
                accepted = listener.tcp.accept() => {
                    let (mut tcp, peer) = accepted?;
                    candidates.push(async move {
                        let mut prefix = vec![0; 1];
                        let read = async {
                            tcp.read_exact(&mut prefix).await?;
                            if prefix[0] == 0x16 {
                                prefix.resize(5, 0);
                                tcp.read_exact(&mut prefix[1..]).await?;
                                let len = usize::from(u16::from_be_bytes([prefix[3], prefix[4]]));
                                prefix.resize(5 + len, 0);
                                tcp.read_exact(&mut prefix[5..]).await?;
                            }
                            Ok::<_, std::io::Error>(())
                        }.await;
                        (tcp, peer, prefix, read)
                    }.boxed());
                }
                Some((tcp, peer, prefix, read)) = candidates.next(), if !candidates.is_empty() => {
                    match read {
                        Ok(()) if prefix.windows(SNI.len()).any(|w| w == SNI) => {
                            return Ok((tcp, prefix));
                        }
                        Ok(()) => eprintln!(
                            "mock listener {local} dropped foreign connection from {peer}: \"{}\"",
                            prefix[..prefix.len().min(32)].escape_ascii()
                        ),
                        Err(e) => eprintln!("mock listener {local} skipped connection from {peer}: {e}"),
                    }
                }
            }
        }
    }
    // A must-not-dial window: strays are filtered as above, but a TLS dialer
    // that connects within it fails the caller's assertion.
    async fn no_tls_dial(listener: &MockListener, window: Duration) -> Result<()> {
        match tokio::time::timeout(window, accept_tls_tcp(listener)).await {
            Err(_) => Ok(()),
            Ok(Ok((tcp, _))) => bail!("unexpected TLS dial from {:?}", tcp.peer_addr()),
            Ok(Err(e)) => Err(e),
        }
    }
    // Await a mock peer with its own failure, so an unexpected dial outcome
    // reports the cause rather than only a derived classification mismatch.
    async fn mock_result(mock: Task) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), mock)
            .await
            .context("mock peer did not finish")??
    }
    async fn accept_client_hello(
        core: &Core,
        listener: &MockListener,
    ) -> Result<TlsStream<TcpStream>> {
        let verifier = Arc::new(ClientVerifier::new(
            core.admission.clone(),
            core.provisional.clone(),
        ));
        let config = crate::tls::server_config(&core.identity, verifier.clone())?;
        let (tcp, prefix) = accept_tls_tcp(listener).await?;
        // Feed exactly the inspected bytes to rustls, then continue its same
        // handshake on the selected socket. Any TLS failure is returned.
        let mut acceptor = rustls::server::Acceptor::default();
        let mut prefix = std::io::Cursor::new(prefix);
        let handshake = loop {
            if prefix.position() == prefix.get_ref().len() as u64 {
                break tokio_rustls::LazyConfigAcceptor::new(acceptor, tcp).await?;
            }
            acceptor.read_tls(&mut prefix)?;
            if let Some(accepted) = acceptor.accept().map_err(|(error, _alert)| error)? {
                break tokio_rustls::StartHandshake::from_parts(accepted, tcp);
            }
        };
        let mut tls: TlsStream<TcpStream> = handshake.into_stream(Arc::new(config)).await?.into();
        assert!(matches!(
            read_frame(&mut tls).await?,
            Some(Message::Hello(_))
        ));
        verifier.release_provisional();
        Ok(tls)
    }
    async fn accept_ready(
        core: &Core,
        listener: &MockListener,
        addresses: Option<Vec<String>>,
    ) -> Result<TlsStream<TcpStream>> {
        let mut tls = accept_hello(core, listener, addresses).await?;
        let Some(Message::Ping(ping)) = read_frame(&mut tls).await? else {
            bail!("READY confirmation PING");
        };
        send(&mut tls, &Message::Pong(ping)).await?;
        Ok(tls)
    }
    #[tokio::test]
    async fn mock_accept_skips_foreign_loopback_connections_queued_before_the_dialer() -> Result<()>
    {
        let (_da, a, _la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        bind(&a, &b).await?;
        let address = lb.local_addr()?.to_string();
        // Queue foreign connections first, including partial records that must
        // neither spin nor delay selection, and a real ClientHello for localhost.
        let mut plaintext = TcpStream::connect(&address).await?;
        tokio::io::AsyncWriteExt::write_all(&mut plaintext, b"GET / HTTP/1.1\r\n").await?;
        let mut probe = TcpStream::connect(&address).await?;
        let config = crate::tls::client_config(&a.identity, b.identity.fingerprint.clone())?;
        let mut client = rustls::ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::try_from("localhost")?,
        )?;
        let mut hello = Vec::new();
        client.write_tls(&mut hello)?;
        tokio::io::AsyncWriteExt::write_all(&mut probe, &hello).await?;
        let mut partial_header = TcpStream::connect(&address).await?;
        tokio::io::AsyncWriteExt::write_all(&mut partial_header, &[0x16, 0x03]).await?;
        let mut partial_body = TcpStream::connect(&address).await?;
        tokio::io::AsyncWriteExt::write_all(&mut partial_body, &[0x16, 0x03, 0x01, 0x00, 0x20])
            .await?;
        let mut closed_header = TcpStream::connect(&address).await?;
        tokio::io::AsyncWriteExt::write_all(&mut closed_header, &[0x16]).await?;
        tokio::io::AsyncWriteExt::shutdown(&mut closed_header).await?;
        // The name outside an encoded SNI extension is not a dialer marker.
        let mut name_only = TcpStream::connect(&address).await?;
        tokio::io::AsyncWriteExt::write_all(&mut name_only, b"\x16\x03\x01\x00\x0apneu.local")
            .await?;
        let mut silent = TcpStream::connect(&address).await?;
        drop(TcpStream::connect(&address).await?);
        let mock_core = b.clone();
        let mock = Task::spawn(async move { accept_hello(&mock_core, &lb, None).await.map(drop) });
        let link = tokio::time::timeout(
            Duration::from_secs(5),
            a.connect_computer(
                &[address],
                b.identity.fingerprint.clone(),
                Some(&b.identity.device_id),
            ),
        )
        .await??;
        assert_eq!(link.hello.device_id, b.identity.device_id);
        mock_result(mock).await?;
        for stray in [
            &mut plaintext,
            &mut probe,
            &mut partial_header,
            &mut partial_body,
            &mut closed_header,
            &mut name_only,
            &mut silent,
        ] {
            let mut bytes = Vec::new();
            // A reset (unread data) or EOF both mean the mock closed it.
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                tokio::io::AsyncReadExt::read_to_end(stray, &mut bytes),
            )
            .await
            .context("stray connection left open")?;
            assert!(bytes.is_empty());
        }
        drop(link);
        // The must-not-dial helper retains pending candidates on timeout;
        // dropping the listener closes them without background tasks.
        let listener = MockListener::new(TcpListener::bind("127.0.0.1:0").await?);
        let mut stalled = TcpStream::connect(listener.local_addr()?).await?;
        tokio::io::AsyncWriteExt::write_all(&mut stalled, &[0x16, 0x03]).await?;
        no_tls_dial(&listener, Duration::from_millis(50)).await?;
        let mut byte = [0];
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                tokio::io::AsyncReadExt::read(&mut stalled, &mut byte),
            )
            .await
            .is_err(),
            "must-not-dial timeout dropped a pending candidate"
        );
        drop(listener);
        let mut bytes = Vec::new();
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stalled, &mut bytes),
        )
        .await
        .context("dropped listener left a candidate open")?;
        assert!(bytes.is_empty());
        let listener = MockListener::new(TcpListener::bind("127.0.0.1:0").await?);
        let config = crate::tls::client_config(&a.identity, b.identity.fingerprint.clone())?;
        let mut client = rustls::ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::try_from("pneu.local")?,
        )?;
        let mut hello = Vec::new();
        client.write_tls(&mut hello)?;
        let mut dialer = TcpStream::connect(listener.local_addr()?).await?;
        tokio::io::AsyncWriteExt::write_all(&mut dialer, &hello).await?;
        let error = no_tls_dial(&listener, Duration::from_millis(50))
            .await
            .expect_err("must-not-dial window accepted a dialer");
        assert!(error.to_string().contains("unexpected TLS dial"));
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn mock_accept_preserves_queued_legitimate_dialers_across_accepts() -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (_da, a, _la) = core().await?;
        let (_db, b, listener) = core().await?;
        let listener = MockListener::new(listener);
        let config = crate::tls::client_config(&a.identity, b.identity.fingerprint.clone())?;
        let mut client = rustls::ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::try_from("pneu.local")?,
        )?;
        let mut hello = Vec::new();
        client.write_tls(&mut hello)?;
        let address = listener.local_addr()?;
        let mut stray = TcpStream::connect(address).await?;
        stray.write_all(b"GET / HTTP/1.1\r\n").await?;
        let mut partial = TcpStream::connect(address).await?;
        partial.write_all(&hello[..2]).await?;
        let mut accepting = Box::pin(accept_tls_tcp(&listener));
        // Keep an already-accepted legitimate candidate pending while other
        // complete ClientHellos queue up behind it.
        assert!(futures_util::poll!(accepting.as_mut()).is_pending());
        let mut dialers = vec![partial];
        for _ in 0..3 {
            let mut dialer = TcpStream::connect(address).await?;
            dialer.write_all(&hello).await?;
            dialers.push(dialer);
        }
        let first = tokio::time::timeout(Duration::from_secs(2), accepting).await??;
        dialers[0].write_all(&hello[2..]).await?;
        for (index, dialer) in dialers.iter_mut().enumerate() {
            dialer.write_all(&[index as u8]).await?;
        }
        let mut accepted = vec![first];
        for _ in 1..dialers.len() {
            accepted.push(
                tokio::time::timeout(Duration::from_secs(2), accept_tls_tcp(&listener))
                    .await
                    .context("queued legitimate dialer was dropped")??,
            );
        }
        let mut seen = std::collections::HashSet::new();
        for (mut tcp, prefix) in accepted {
            assert_eq!(prefix, hello);
            let mut marker = [0];
            tokio::time::timeout(Duration::from_secs(2), tcp.read_exact(&mut marker)).await??;
            let index = usize::from(marker[0]);
            assert_eq!(tcp.peer_addr()?, dialers[index].local_addr()?);
            assert!(seen.insert(index), "dialer accepted twice");
        }
        assert_eq!(seen.len(), dialers.len());
        let mut bytes = Vec::new();
        // A foreign connection may close with either EOF or a reset.
        let _ = tokio::time::timeout(Duration::from_secs(2), stray.read_to_end(&mut bytes))
            .await
            .context("foreign stray left open")?;
        assert!(bytes.is_empty());
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn mock_accept_fails_on_the_selected_tls_connection_without_skipping() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        let address = lb.local_addr()?.to_string();
        // Skip plaintext first, then select a record with the encoded SNI but
        // a truncated ClientHello body. Its TLS error must surface, not retry.
        let mut plaintext = TcpStream::connect(&address).await?;
        tokio::io::AsyncWriteExt::write_all(&mut plaintext, b"GET / HTTP/1.1\r\n").await?;
        let body = [
            &[0x01, 0x00, 0x00, 0x15, 0x03, 0x03][..],
            b"\x00\x00\x00\x0f\x00\x0d\x00\x00\x0apneu.local",
        ]
        .concat();
        let record = [&[0x16, 0x03, 0x01, 0x00, body.len() as u8][..], &body].concat();
        let mut corrupt = TcpStream::connect(&address).await?;
        tokio::io::AsyncWriteExt::write_all(&mut corrupt, &record[..2]).await?;
        let mut accepted = Box::pin(accept_client_hello(&b, &lb));
        assert!(futures_util::poll!(accepted.as_mut()).is_pending());
        tokio::io::AsyncWriteExt::write_all(&mut corrupt, &record[2..]).await?;
        let error = tokio::time::timeout(Duration::from_secs(2), accepted)
            .await
            .context("corrupt TLS connection was skipped")?
            .err()
            .context("corrupt TLS connection was accepted")?;
        assert!(
            matches!(
                error.downcast_ref::<rustls::Error>(),
                Some(rustls::Error::InvalidMessage(
                    rustls::InvalidMessage::MissingData("Random")
                ))
            ),
            "unexpected selected TLS error: {error:?}"
        );
        // A real dialer pinning the wrong fingerprint fails the handshake.
        let pin = Hash::new("a".repeat(64))?;
        let addresses = [address];
        let (accepted, dialed) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(2), accept_client_hello(&b, &lb)),
            a.connect_computer(&addresses, pin, None)
        );
        assert!(
            accepted
                .context("wrong-pin TLS connection was skipped")?
                .is_err()
        );
        assert!(dialed.is_err());
        drop(corrupt);
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    async fn computer_route(a: &Core, b: &Core, address: String) -> Result<()> {
        bind(a, b).await?;
        a.store
            .remember_route(b.identity.device_id.clone(), address)
            .await?;
        a.store
            .call(|c| {
                c.execute("UPDATE peers SET role='computer'", [])?;
                Ok(())
            })
            .await
    }
    async fn resume_test_link(core: Arc<Core>, address: String, pin: Hash) -> Result<()> {
        let captured = core.store.with_current_pairing();
        let ComputerLink {
            mut tls,
            hello,
            remote,
            permit,
            transport: _transport,
            setup_sent,
            ..
        } = core.connect_computer(&[address], pin.clone(), None).await?;
        let buffered = core.confirm_peer_ready(&mut tls).await?;
        core.register_buffered_setup(
            tls, hello, pin, remote, permit, captured, true, false, buffered, None, setup_sent,
        )
        .await
    }
    #[tokio::test]
    async fn resume_bulk_pending_cancels_reconnect_without_blocking_reports() -> Result<()> {
        let (dir, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(la);
        let address = lb.local_addr()?.to_string();
        computer_route(&a, &b, address.clone()).await?;
        bind(&b, &a).await?;
        let device = b.identity.device_id.clone();
        let generation = a.store.pairing_generation(&device).unwrap();
        let peer = device.clone();
        a.store
            .call(move |c| {
                let tx = c.transaction()?;
                for n in 0..64u8 {
                    tx.execute(
                        "INSERT INTO file_pending_cancels(device,id,generation) VALUES(?,?,?)",
                        rusqlite::params![
                            peer.as_str(),
                            Id::from_bytes([n; 16]).as_str(),
                            generation as i64
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        let source = dir.path().join("after-cancels.txt");
        std::fs::write(&source, b"connection is usable")?;
        let server = Task::spawn(b.clone().serve(lb, false));
        let link = Task::spawn(resume_test_link(
            a.clone(),
            address,
            b.identity.fingerprint.clone(),
        ));
        let completed: Result<()> = async {
            let drained = tokio::time::timeout(Duration::from_secs(10), async {
                while !a
                    .outbox
                    .pending_cancels(device.clone(), generation)
                    .await?
                    .is_empty()
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await;
            ensure!(
                drained.is_ok(),
                "64 pending cancels must drain reports: {} intents remain",
                a.outbox
                    .pending_cancels(device.clone(), generation)
                    .await?
                    .len()
            );
            drained??;
            // HELLO has now established file support. The connection must
            // remain usable after draining all 64 cancellation reports.
            let reply = a
                .send_files(vec![source], crate::outbox::Targets::Devices(vec![device]))
                .await?;
            let operation = Id::new(reply["operation"].as_str().unwrap().to_owned())?;
            until(|| {
                a.ui_snapshot()
                    .transfers
                    .iter()
                    .any(|t| t.operation == operation && t.state == "sent")
            })
            .await?;
            Ok(())
        }
        .await;
        a.request_shutdown();
        b.request_shutdown();
        server.await??;
        let _ = tokio::time::timeout(Duration::from_secs(5), link).await?;
        a.shutdown_actions().await?;
        b.shutdown_actions().await?;
        completed?;
        assert_eq!(
            std::fs::read(b.config().save_path.join("after-cancels.txt"))?,
            b"connection is usable"
        );
        Ok(())
    }
    #[tokio::test]
    async fn desktop_resume_after_connection_drop_preserves_exact_file() -> Result<()> {
        let (da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(la);
        let address = lb.local_addr()?.to_string();
        computer_route(&a, &b, address.clone()).await?;
        bind(&b, &a).await?;
        a.set_setting("queue_offline".into(), false.into()).await?;
        *b.file_write_delay.lock().unwrap() = Duration::from_millis(15);
        let server = Task::spawn(b.clone().serve(lb, false));
        let dialing = a.clone();
        let route = address.clone();
        let pin = b.identity.fingerprint.clone();
        let first = Task::spawn(resume_test_link(dialing, route, pin));
        until(|| a.connected_devices().contains(&b.identity.device_id)).await?;
        let bytes: Vec<u8> = (0..12 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let source = da.path().join("resume.bin");
        std::fs::write(&source, &bytes)?;
        let mut events = b.subscribe();
        let reply = a
            .send_files(
                vec![source],
                crate::outbox::Targets::Devices(vec![b.identity.device_id.clone()]),
            )
            .await?;
        let operation = Id::new(reply["operation"].as_str().unwrap().to_owned())?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::TransferProgress { received, .. } = events.recv().await?
                    && received >= 5 * 1024 * 1024
                {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        b.sessions.lock().unwrap().sessions[&a.identity.device_id]
            .close
            .send_replace(Some(CloseReason::Error(ErrorCode::Closing)));
        let _ = first.await?;
        until(|| {
            a.ui_snapshot()
                .transfers
                .iter()
                .any(|t| t.operation == operation && t.state == "failed")
        })
        .await?;
        until(|| b.connected_devices().is_empty()).await?;
        tokio::time::timeout(Duration::from_secs(10), b.store.drain()).await?;
        let sender = a.identity.device_id.clone();
        let durable = b
            .store
            .call(move |c| {
                let record: String = c.query_row(
                    "SELECT record FROM file_partials WHERE device=?",
                    [sender.as_str()],
                    |r| r.get(0),
                )?;
                Ok(serde_json::from_str::<Value>(&record)?["offset"]
                    .as_u64()
                    .unwrap())
            })
            .await?;
        assert!(
            durable > 0 && durable < bytes.len() as u64,
            "durable prefix {durable}"
        );
        // A sender can be ahead of the receiver's fsynced prefix. Deliberately
        // persist a misleading UI offset; READY must still select the suffix.
        let advisory_offset = bytes.len() as i64;
        a.store
            .call(move |c| {
                c.execute(
                    "UPDATE outbox_deliveries SET sent=? WHERE state='failed'",
                    [advisory_offset],
                )?;
                Ok(())
            })
            .await?;
        let mut events = b.subscribe();
        a.retry_transfer(operation.clone(), b.identity.device_id.clone())
            .await?;
        let dialing = a.clone();
        let pin = b.identity.fingerprint.clone();
        let second = Task::spawn(resume_test_link(dialing, address, pin));
        let resumed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::TransferProgress { received, .. } = events.recv().await? {
                    return Ok::<_, anyhow::Error>(received);
                }
            }
        })
        .await??;
        assert_eq!(
            resumed, durable,
            "first receiver progress is the READY offset"
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            while !a
                .ui_snapshot()
                .transfers
                .iter()
                .any(|t| t.operation == operation && t.state == "sent")
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(
            std::fs::read(b.config().save_path.join("resume.bin"))?,
            bytes
        );
        a.request_shutdown();
        b.request_shutdown();
        server.await??;
        let _ = tokio::time::timeout(Duration::from_secs(5), second).await?;
        a.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn folder_resume_after_connection_drop_preserves_exact_tree() -> Result<()> {
        let (da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(la);
        let address = lb.local_addr()?.to_string();
        computer_route(&a, &b, address.clone()).await?;
        bind(&b, &a).await?;
        a.set_setting("queue_offline".into(), false.into()).await?;
        *b.file_write_delay.lock().unwrap() = Duration::from_millis(15);
        let server = Task::spawn(b.clone().serve(lb, false));
        let dialing = a.clone();
        let route = address.clone();
        let pin = b.identity.fingerprint.clone();
        let first = Task::spawn(resume_test_link(dialing, route, pin));
        until(|| a.connected_devices().contains(&b.identity.device_id)).await?;
        let bytes: Vec<u8> = (0..12 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let source = da.path().join("Trip");
        std::fs::create_dir_all(source.join("empty/nested"))?;
        std::fs::create_dir_all(source.join("docs"))?;
        std::fs::write(source.join("first.txt"), b"first")?;
        std::fs::write(source.join("docs/resume.bin"), &bytes)?;
        std::fs::write(source.join("zero"), b"")?;
        let mut events = b.subscribe();
        let reply = a
            .send_files(
                vec![source],
                crate::outbox::Targets::Devices(vec![b.identity.device_id.clone()]),
            )
            .await?;
        let operation = Id::new(reply["operation"].as_str().unwrap().to_owned())?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::TransferProgress { received, .. } = events.recv().await?
                    && received >= 5 * 1024 * 1024
                {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        b.sessions.lock().unwrap().sessions[&a.identity.device_id]
            .close
            .send_replace(Some(CloseReason::Error(ErrorCode::Closing)));
        let _ = first.await?;
        until(|| {
            a.ui_snapshot()
                .transfers
                .iter()
                .any(|t| t.operation == operation && t.state == "failed")
        })
        .await?;
        until(|| b.connected_devices().is_empty()).await?;
        tokio::time::timeout(Duration::from_secs(10), b.store.drain()).await?;
        let sender = a.identity.device_id.clone();
        let durable = b
            .store
            .call(move |c| {
                let record: String = c.query_row(
                    "SELECT record FROM file_partials WHERE device=?",
                    [sender.as_str()],
                    |r| r.get(0),
                )?;
                Ok(serde_json::from_str::<Value>(&record)?["offset"]
                    .as_u64()
                    .unwrap())
            })
            .await?;
        assert!(
            durable > 0 && durable < bytes.len() as u64,
            "durable prefix {durable}"
        );
        // A sender can be ahead of the receiver's fsynced prefix. Deliberately
        // persist a misleading UI offset; READY must still select the suffix.
        let advisory_offset = bytes.len() as i64;
        a.store
            .call(move |c| {
                c.execute(
                    "UPDATE outbox_deliveries SET sent=? WHERE state='failed'",
                    [advisory_offset],
                )?;
                Ok(())
            })
            .await?;
        let mut events = b.subscribe();
        a.retry_transfer(operation.clone(), b.identity.device_id.clone())
            .await?;
        let dialing = a.clone();
        let pin = b.identity.fingerprint.clone();
        let second = Task::spawn(resume_test_link(dialing, address, pin));
        let resumed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::TransferProgress { received, .. } = events.recv().await? {
                    return Ok::<_, anyhow::Error>(received);
                }
            }
        })
        .await??;
        assert_eq!(
            resumed, durable,
            "first receiver progress is the READY offset"
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            while !a
                .ui_snapshot()
                .transfers
                .iter()
                .any(|t| t.operation == operation && t.state == "sent")
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(
            std::fs::read(b.config().save_path.join("Trip/docs/resume.bin"))?,
            bytes
        );
        assert_eq!(
            std::fs::read(b.config().save_path.join("Trip/first.txt"))?,
            b"first"
        );
        assert!(b.config().save_path.join("Trip/empty/nested").is_dir());
        assert_eq!(
            std::fs::metadata(b.config().save_path.join("Trip/zero"))?.len(),
            0
        );
        a.request_shutdown();
        b.request_shutdown();
        server.await??;
        let _ = tokio::time::timeout(Duration::from_secs(5), second).await?;
        a.shutdown_actions().await?;
        b.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn sixteen_stalled_computers_leave_inbound_phone_admission_available() -> Result<()> {
        let (dir, a, la) = core().await?;
        let address = la.local_addr()?.to_string();
        let phone = crate::client::Phone::new(dir.path().join("phone"))?;
        let server = Task::spawn(a.clone().serve(la, false));
        let invitation = a.start_pairing_with_addresses(vec![address]).await?;
        let mut initial = phone.pair(&invitation).await?;
        let ping = Ping {
            nonce: Nonce::new(crate::random_hex(8))?,
        };
        initial.write(&Message::Ping(ping.clone())).await?;
        assert_eq!(
            initial.read(Duration::from_secs(2)).await?,
            Message::Pong(ping)
        );
        drop(initial);
        until(|| a.connected_devices().is_empty()).await?;

        let stalled = MockListener::new(TcpListener::bind("127.0.0.1:0").await?);
        let mut peers = Vec::new();
        for _ in 0..16 {
            let device = Id::new(crate::random_hex(16))?;
            let fingerprint = Hash::new(crate::random_hex(32))?;
            let guard = a.pairing_gate.clone().lock_owned().await;
            a.persist_pairing(&device, &fingerprint, guard, false)
                .await?;
            a.store
                .remember_route(device.clone(), stalled.local_addr()?.to_string())
                .await?;
            peers.push(device);
        }
        a.store
            .call(move |c| {
                for device in peers {
                    c.execute(
                        "UPDATE peers SET role='computer' WHERE id=?",
                        [device.as_str()],
                    )?;
                }
                Ok(())
            })
            .await?;
        // Every worker is reconnecting; only the separate outbound allowance
        // may hold sockets. Keep these TCP routes open without answering TLS.
        until(|| {
            a.mesh.connecting.lock().unwrap().len() == 16
                && a.outbound_ingress.available_permits() == 0
        })
        .await?;
        let mut stalled_sockets = Vec::new();
        for _ in 0..OUTBOUND_ADMISSION_LIMIT {
            stalled_sockets.push(
                tokio::time::timeout(Duration::from_secs(2), accept_tls_tcp(&stalled))
                    .await??
                    .0,
            );
        }
        assert_eq!(a.ingress.available_permits(), ADMISSION_LIMIT);
        assert_eq!(
            a.transports.available_permits(),
            TRANSPORT_LIMIT - OUTBOUND_ADMISSION_LIMIT
        );
        // Require a complete authenticated reconnect and READY heartbeat before
        // even one stalled ten-second candidate timeout can release capacity.
        let mut reconnected = tokio::time::timeout(Duration::from_secs(2), async {
            let mut link = phone.reconnect().await?;
            let ping = Ping {
                nonce: Nonce::new(crate::random_hex(8))?,
            };
            link.write(&Message::Ping(ping.clone())).await?;
            assert_eq!(
                link.read(Duration::from_secs(1)).await?,
                Message::Pong(ping)
            );
            Ok::<_, anyhow::Error>(link)
        })
        .await??;
        assert_eq!(
            a.connected_devices(),
            std::slice::from_ref(&phone.identity.device_id)
        );
        reconnected.close().await?;
        drop(reconnected);
        a.request_shutdown();
        server.await??;
        drop(stalled_sockets);
        assert_eq!(a.ingress.available_permits(), ADMISSION_LIMIT);
        assert_eq!(
            a.outbound_ingress.available_permits(),
            OUTBOUND_ADMISSION_LIMIT
        );
        assert_eq!(a.transports.available_permits(), TRANSPORT_LIMIT);
        Ok(())
    }

    #[tokio::test]
    async fn outbound_legacy_hello_ignores_role_and_address_hints_before_pairing() -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        drop(la);
        let address = lb.local_addr()?.to_string();
        let responder = b.clone();
        let server = Task::spawn(async move {
            let mut tls = accept_client_hello(&responder, &lb).await?;
            let mut hello = responder.computer_hello();
            hello.capabilities.retain(|c| c != "mesh");
            hello.role = Some(DeviceRole::Phone);
            hello.addresses = Some(vec!["192.0.2.99:47821".into()]);
            send(&mut tls, &Message::Hello(hello)).await?;
            Ok(())
        });
        let link = a
            .connect_computer(
                std::slice::from_ref(&address),
                b.identity.fingerprint.clone(),
                None,
            )
            .await?;
        assert_eq!(link.hello.role, Some(DeviceRole::Computer));
        assert!(
            link.hello.addresses.is_none(),
            "unnegotiated hints reached join route planning"
        );
        // Keep an invitation-provided route for a legacy computer, while a
        // subsequent HELLO cannot replace it with unnegotiated addresses.
        let mut initial = link.hello.clone();
        initial.addresses = Some(vec![address.clone()]);
        let guard = a.pairing_gate.clone().lock_owned().await;
        a.persist_pairing_hello(
            &b.identity.device_id,
            &b.identity.fingerprint,
            guard,
            false,
            Some(initial),
            None,
        )
        .await?;
        let mut hello = link.hello.clone();
        hello.role = Some(DeviceRole::Phone);
        hello.addresses = Some(vec!["192.0.2.98:47821".into()]);
        a.store
            .remember_peer_hello(hello, Arc::new(AtomicU64::new(1)), 1, true)
            .await?;
        let peer = a.store.peer_info().await?.remove(0);
        assert_eq!(peer.role, "computer");
        assert_eq!(peer.addresses, vec![address]);
        drop(link);
        server.await??;
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }

    #[tokio::test]
    async fn address_sweeps_release_admission_between_candidates() -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop((la, lb));
        let first = MockListener::new(TcpListener::bind("127.0.0.1:0").await?);
        let second = MockListener::new(TcpListener::bind("127.0.0.1:0").await?);
        let other = MockListener::new(TcpListener::bind("127.0.0.1:0").await?);
        let reserved = a
            .outbound_ingress
            .clone()
            .acquire_many_owned((OUTBOUND_ADMISSION_LIMIT - 1) as u32)
            .await?;
        let addresses = vec![
            first.local_addr()?.to_string(),
            second.local_addr()?.to_string(),
        ];
        let dialer = a.clone();
        let pin = b.identity.fingerprint.clone();
        let sweep = Task::spawn(async move {
            dialer
                .connect_computer(&addresses, pin, None)
                .await
                .map(|_| ())
        });
        let (first_socket, _) =
            tokio::time::timeout(Duration::from_secs(2), accept_tls_tcp(&first)).await??;
        let addresses = [other.local_addr()?.to_string()];
        let mut queued =
            Box::pin(a.connect_computer(&addresses, b.identity.fingerprint.clone(), None));
        // Poll to queue this candidate behind the sweep's occupied permit.
        assert!(futures_util::poll!(queued.as_mut()).is_pending());
        drop(first_socket);
        let (other_socket, _) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut queued => bail!("queued handshake unexpectedly finished: {}", result.is_ok()),
                accepted = accept_tls_tcp(&other) => accepted,
            }
        }).await??;
        // The sweep must yield to the earlier waiter, rather than retain a
        // permit while opening its second route.
        no_tls_dial(&second, Duration::from_millis(50)).await?;
        drop((queued, other_socket));
        let (second_socket, _) =
            tokio::time::timeout(Duration::from_secs(2), accept_tls_tcp(&second)).await??;
        sweep.abort();
        let _ = sweep.await;
        drop((reserved, second_socket));
        assert_eq!(
            a.outbound_ingress.available_permits(),
            OUTBOUND_ADMISSION_LIMIT
        );
        assert_eq!(a.transports.available_permits(), TRANSPORT_LIMIT);
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }

    #[tokio::test]
    async fn authenticated_inbound_recovery_cancels_error_backoff_and_drop_redials() -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        computer_route(&a, &b, lb.local_addr()?.to_string()).await?;
        bind(&b, &a).await?;
        let generation = a.store.pairing_generation(&b.identity.device_id).unwrap();
        let server = Task::spawn(a.clone().serve(la, false));
        let rejected = accept_hello_version(&b, &lb, None, 2).await?;
        until(|| a.link_problem(&b.identity.device_id) == Some(LinkProblem::UpdateRequired))
            .await?;
        drop(rejected);
        // Wake ordinary reconnect sleep, then let the worker enter the issue
        // wait. Discovery must not bypass its remaining five-minute deadline.
        a.mesh.changed.notify_waiters();
        tokio::task::yield_now().await;
        for _ in 0..3 {
            a.mesh.changed.notify_waiters();
            no_tls_dial(&lb, Duration::from_millis(50)).await?;
        }
        let ComputerLink {
            mut tls,
            permit,
            transport,
            ..
        } = b
            .connect_computer(
                &[format!("127.0.0.1:{}", a.bound_port())],
                a.identity.fingerprint.clone(),
                Some(&a.identity.device_id),
            )
            .await?;
        b.confirm_peer_ready(&mut tls).await?;
        assert!(a.link_problem(&b.identity.device_id).is_none());
        assert_eq!(
            a.connected_devices(),
            std::slice::from_ref(&b.identity.device_id)
        );
        drop((tls, permit, transport));
        // Real loopback redial without advancing virtual time or sending a
        // metadata notification after recovery. The old deadline is minutes away.
        let mut redial =
            tokio::time::timeout(Duration::from_secs(2), accept_ready(&b, &lb, None)).await??;
        until(|| a.connected_devices().contains(&b.identity.device_id)).await?;
        assert!(
            a.mesh
                .links
                .lock()
                .unwrap()
                .get(&b.identity.device_id)
                .and_then(|link| link.issue.as_ref())
                .is_none_or(|issue| issue.generation != generation)
        );
        let ping = Ping {
            nonce: Nonce::new(crate::random_hex(8))?,
        };
        send(&mut redial, &Message::Ping(ping.clone())).await?;
        assert_eq!(read_frame(&mut redial).await?, Some(Message::Pong(ping)));
        drop(redial);
        a.request_shutdown();
        server.await??;
        b.shutdown_actions().await
    }

    #[tokio::test]
    async fn dialer_keepalive_answers_then_times_out_without_pong() -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        drop(la);
        computer_route(&a, &b, lb.local_addr()?.to_string()).await?;
        bind(&b, &a).await?;
        let peer = a.store.peer_info().await?.remove(0);
        let (cancel, rx) = watch::channel(false);
        let worker = Task::spawn(a.clone().dial_worker(peer, rx));
        let mut tls = accept_ready(&b, &lb, None).await?;
        until(|| !a.connected_devices().is_empty()).await?;
        let ready_nonce = Nonce::new(crate::random_hex(8))?;
        send(
            &mut tls,
            &Message::Ping(Ping {
                nonce: ready_nonce.clone(),
            }),
        )
        .await?;
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(2),read_frame(&mut tls)).await.context("READY barrier")??,Some(Message::Pong(p)) if p.nonce==ready_nonce)
        );
        // Real loopback TLS, virtual idle periods; resume before polling I/O so
        // Tokio cannot auto-advance past a PONG while the kernel is answering.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::time::resume();
        let ping = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut tls))
            .await
            .context("first idle PING")??;
        let Some(Message::Ping(ping)) = ping else {
            panic!("idle dialer must PING")
        };
        send(&mut tls, &Message::Pong(ping)).await?;
        // A matching reply to a subsequent frame proves the reader consumed
        // the preceding PONG before we cross its deadline. A sleep cannot.
        let barrier = Ping {
            nonce: Nonce::new(crate::random_hex(8))?,
        };
        send(&mut tls, &Message::Ping(barrier.clone())).await?;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), read_frame(&mut tls)).await??,
            Some(Message::Pong(barrier))
        );
        assert_eq!(a.connected_devices(), vec![b.identity.device_id.clone()]);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::time::resume();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), read_frame(&mut tls))
                .await
                .context("second idle PING")??,
            Some(Message::Ping(_))
        ));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::time::resume();
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(2),read_frame(&mut tls)).await.context("missing PONG close")??,Some(Message::Error(e)) if e.code==ErrorCode::Timeout)
        );
        until(|| a.connected_devices().is_empty()).await?;
        assert_eq!(a.paired_devices(), vec![b.identity.device_id.clone()]);
        cancel.send_replace(true);
        worker.await??;
        assert!(a.mesh.connecting.lock().unwrap().is_empty());
        assert_eq!(a.ingress.available_permits(), ADMISSION_LIMIT);
        assert_eq!(a.transports.available_permits(), TRANSPORT_LIMIT);
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn network_loss_redials_using_changed_hello_address() -> Result<()> {
        let (_da, a, la) = core().await?;
        let (mut db, b, old) = core().await?;
        let old = MockListener::new(old);
        drop(la);
        let moved = db.listen()?;
        let address = moved.local_addr()?.to_string();
        computer_route(&a, &b, old.local_addr()?.to_string()).await?;
        bind(&b, &a).await?;
        let supervisor = Task::spawn(a.clone().mesh_supervise());
        let tls = accept_ready(&b, &old, Some(vec![address.clone()])).await?;
        until(|| a.connected_devices().contains(&b.identity.device_id)).await?;
        let previous = a.sessions.lock().unwrap().sessions[&b.identity.device_id].id;
        // HELLO metadata is durable and picked up by the supervisor.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if a.store.peer_info().await.unwrap()[0]
                    .addresses
                    .contains(&address)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        drop(tls);
        // Keep the stale route owned, but reject its dials immediately so the
        // cached last-good address cannot stall recovery to the moved server.
        let _old_server = Task::spawn(async move {
            loop {
                let (socket, _) = accept_tls_tcp(&old).await?;
                drop(socket);
            }
        });
        let server = Task::spawn(b.clone().serve(moved, false));
        until(|| {
            a.sessions
                .lock()
                .unwrap()
                .sessions
                .get(&b.identity.device_id)
                .is_some_and(|s| s.id != previous)
        })
        .await?;
        until(|| b.connected_devices().contains(&a.identity.device_id)).await?;
        assert!(a.store.peer_info().await?[0].addresses.contains(&address));
        a.request_shutdown();
        b.request_shutdown();
        supervisor.await??;
        server.await??;
        assert!(a.connected_devices().is_empty());
        assert!(a.mesh.connecting.lock().unwrap().is_empty());
        a.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn cancelling_or_panicking_supervisor_cleans_connecting_workers_and_ready_links()
    -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        drop(la);
        computer_route(&a, &b, lb.local_addr()?.to_string()).await?;
        bind(&b, &a).await?;
        let supervisor = Task::spawn(a.clone().mesh_supervise());
        // Stall TLS after TCP admission. The snapshot exposes an active dial.
        let (tcp, _) = accept_tls_tcp(&lb).await?;
        until(|| {
            a.ui_snapshot()
                .devices
                .iter()
                .any(|d| d.state == "connecting")
        })
        .await?;
        supervisor.abort();
        assert!(supervisor.await.unwrap_err().is_cancelled());
        until(|| {
            a.mesh.connecting.lock().unwrap().is_empty()
                && a.ingress.available_permits() == ADMISSION_LIMIT
                && a.transports.available_permits() == TRANSPORT_LIMIT
        })
        .await?;
        drop(tcp);
        // Restart the supervisor itself, then abort it with a published READY.
        let supervisor = Task::spawn(a.clone().mesh_supervise());
        let tls = accept_ready(&b, &lb, None).await?;
        until(|| !a.connected_devices().is_empty()).await?;
        a.mesh.panic_supervisor.store(true, Ordering::SeqCst);
        assert!(supervisor.await.unwrap_err().is_panic());
        until(|| {
            a.connected_devices().is_empty()
                && a.mesh.connecting.lock().unwrap().is_empty()
                && a.ingress.available_permits() == ADMISSION_LIMIT
                && a.transports.available_permits() == TRANSPORT_LIMIT
        })
        .await?;
        let mut tls = tls;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), read_frame(&mut tls)).await?,
            Err(_) | Ok(None)
        ));
        a.store.drain().await;
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn pending_join_recovers_lost_result_and_buffers_ready_before_binding() -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, lb) = core().await?;
        drop(lb);
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        let join = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let worker = Task::spawn(b.clone().join_worker(join));
        let mut tls = accept_hello(&a, &la, None).await?;
        let token = match read_frame(&mut tls).await? {
            Some(Message::PairRequest(p)) => p.token,
            _ => panic!("PAIR_REQUEST"),
        };
        a.finish_pairing_captured(&b.identity.device_id, &b.identity.fingerprint, &token)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        drop(tls);
        assert!(worker.await?.is_err());
        assert!(b.paired_devices().is_empty());
        assert!(!b.paths.data.join("join-pending.json").exists());
        // Local expiry applies only before the first request. Retry in-process
        // with the same token and pin, even after the invitation expires.
        b.mesh.join.lock().unwrap().as_mut().unwrap().uri.expires_at = 0;
        let join = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let worker = Task::spawn(b.clone().join_worker(join));
        let mut tls = accept_hello(&a, &la, None).await?;
        assert!(matches!(
            read_frame(&mut tls).await?,
            Some(Message::PairRequest(p)) if p.token == uri.token
        ));
        let clip = Clipboard {
            sensitive: false,
            transfer_id: Id::new(crate::random_hex(16))?,
            text: "buffer me".into(),
            created_at: crate::now_ms(),
            origin: ClipboardOrigin::Clipboard,
            hlc: Some(crate::now_ms()),
            source_device: Some(a.identity.device_id.clone()),
        };
        send(&mut tls, &Message::Clipboard(clip.clone())).await?;
        send(&mut tls, &Message::Status(Status { paused: false })).await?;
        let nonce = Nonce::new(crate::random_hex(8))?;
        send(
            &mut tls,
            &Message::Ping(Ping {
                nonce: nonce.clone(),
            }),
        )
        .await?;
        assert!(matches!(read_frame(&mut tls).await?,Some(Message::Pong(p)) if p.nonce==nonce));
        assert!(
            b.paired_devices().is_empty(),
            "buffer has no effects before binding persistence"
        );
        send(
            &mut tls,
            &Message::PairResult(PairResult {
                accepted: true,
                code: None,
                message: None,
            }),
        )
        .await?;
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(2),read_frame(&mut tls)).await??,Some(Message::Ack(a)) if a.transfer_id==clip.transfer_id)
        );
        assert_eq!(b.join_status(attempt)?["state"], "paired");
        assert!(!b.paths.data.join("join-pending.json").exists());
        assert_eq!(a.paired_devices(), vec![b.identity.device_id.clone()]);
        assert_eq!(b.paired_devices(), vec![a.identity.device_id.clone()]);
        worker.abort();
        let _ = worker.await;
        until(|| {
            b.connected_devices().is_empty() && b.transports.available_permits() == TRANSPORT_LIMIT
        })
        .await?;
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }

    #[tokio::test]
    async fn supervisor_recovers_join_and_dial_panics_without_leaking_children() -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, lb) = core().await?;
        drop(lb);
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        b.mesh.panic_join.store(true, Ordering::SeqCst);
        let supervisor = Task::spawn(b.clone().mesh_supervise());
        let mut tls = accept_hello(&a, &la, None).await?;
        let token = match read_frame(&mut tls).await? {
            Some(Message::PairRequest(p)) => p.token,
            _ => panic!("PAIR_REQUEST"),
        };
        a.finish_pairing_captured(&b.identity.device_id, &b.identity.fingerprint, &token)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        drop(tls);
        let server = Task::spawn(a.clone().serve(la.into_inner(), false));
        until(|| a.connected_devices().len() == 1 && b.connected_devices().len() == 1).await?;
        assert_eq!(b.join_status(attempt)?["state"], "paired");
        // An actual worker panic is collected, then the supervisor replaces it.
        b.mesh.panic_dial.store(true, Ordering::SeqCst);
        let previous = b.sessions.lock().unwrap().sessions[&a.identity.device_id].id;
        b.sessions.lock().unwrap().sessions[&a.identity.device_id]
            .close
            .send_replace(Some(CloseReason::Error(ErrorCode::ConnectionReplaced)));
        until(|| {
            b.sessions
                .lock()
                .unwrap()
                .sessions
                .get(&a.identity.device_id)
                .is_some_and(|s| s.id != previous)
        })
        .await?;
        a.request_shutdown();
        b.request_shutdown();
        server.await??;
        supervisor.await??;
        until(|| {
            b.connected_devices().is_empty()
                && b.mesh.connecting.lock().unwrap().is_empty()
                && b.transports.available_permits() == TRANSPORT_LIMIT
                && b.ingress.available_permits() == ADMISSION_LIMIT
        })
        .await?;
        assert!(!b.paths.data.join("join-pending.json").exists());
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn window_focus_lease_and_pair_notification_are_bounded() -> Result<()> {
        let (_dir, core, listener) = core().await?;
        drop(listener);
        assert!(!core.window_focused());
        core.set_window_focus(0, true);
        assert!(core.window_focused());
        core.set_window_focus(1, false);
        assert!(
            core.window_focused(),
            "stale child cannot change current focus"
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(!core.window_focused(), "crashed child lease expires");
        tokio::time::resume();
        core.set_window_focus(0, true);
        core.set_window_focus(0, false);
        assert!(!core.window_focused());
        let notice = crate::ui::notifications::Notice::paired("Other PC");
        assert_eq!(notice.summary, "Paired with Other PC");
        assert!(notice.transient);
        assert_eq!(notice.expires, 4000);
        core.shutdown_actions().await
    }
    #[tokio::test]
    async fn local_offline_or_disabled_copies_still_advance_persistent_applied_register()
    -> Result<()> {
        let (_dir, core, listener) = core().await?;
        drop(listener);
        let mut previous = 0;
        for disabled in [false, true] {
            if disabled {
                let mut config = core.config();
                config.clipboard_sync_to_phone = false;
                core.update_config(config).await?;
            }
            assert!(core.observe_local_clipboard_offer().await?.is_none());
            let (hlc, source): (String, String) = core
                .store
                .call(|c| {
                    Ok(
                        c.query_row("SELECT hlc,source_device FROM clipboard_clock", [], |r| {
                            Ok((r.get(0)?, r.get(1)?))
                        })?,
                    )
                })
                .await?;
            let hlc = hlc.parse::<u64>()?;
            assert!(hlc > previous);
            previous = hlc;
            assert_eq!(source, core.identity.device_id.as_str());
        }
        core.shutdown_actions().await
    }
    #[tokio::test]
    async fn unrelated_peer_decisions_do_not_suppress_diagnostics_or_revocation() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let (_db, b, _lb) = core().await?;
        let (_dc, c, _lc) = core().await?;
        bind(&a, &b).await?;
        bind(&a, &c).await?;
        let device = &b.identity.device_id;
        let other = &c.identity.device_id;
        let generation = a.store.pairing_generation(device).unwrap();
        let other_generation = a.store.pairing_generation(other).unwrap();
        for problem in [
            LinkProblem::UpdateRequired,
            LinkProblem::FingerprintMismatch,
            LinkProblem::BindingMismatch,
            LinkProblem::Unauthorized,
        ] {
            let pending = a.link_epoch(device);
            let inbound = a.link_epochs();
            a.record_link_registration(other, a.next_connection.fetch_add(1, Ordering::SeqCst));
            a.set_link_problem(other, other_generation, Some(LinkProblem::UpdateRequired));
            a.set_link_problem(other, other_generation, None);
            assert_eq!(a.link_epoch(device), pending);
            assert_eq!(inbound.get(device).copied().unwrap_or_default(), pending);
            a.set_link_problem_if(device, generation, pending, Some(problem));
            assert_eq!(a.link_problem(device), Some(problem));
            a.set_link_problem(device, generation, None);
        }
        let pending = a.link_epoch(device);
        a.record_link_registration(other, a.next_connection.fetch_add(1, Ordering::SeqCst));
        a.set_link_problem(
            other,
            other_generation,
            Some(LinkProblem::FingerprintMismatch),
        );
        assert!(
            a.forget_pairing_generation_checked(
                device.clone(),
                CloseReason::Error(ErrorCode::Closing),
                None,
                None,
                Some(generation),
                Some(pending)
            )
            .await?
        );
        assert_eq!(a.paired_devices(), std::slice::from_ref(other));
        assert_eq!(
            a.link_problem(other),
            Some(LinkProblem::FingerprintMismatch)
        );
        a.shutdown_actions().await?;
        b.shutdown_actions().await?;
        c.shutdown_actions().await
    }
    #[tokio::test]
    async fn invitation_versions_are_distinct_from_malformed_links_without_dialing() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        let uri = b
            .start_pairing_with_addresses(vec![lb.local_addr()?.to_string()])
            .await?
            .build()?;
        for (version, expected) in [
            ("2", "update_required"),
            ("4294967295", "update_required"),
            ("0", "garbled"),
            ("abc", "garbled"),
            ("4294967296", "garbled"),
        ] {
            assert_eq!(
                a.join(&uri.replacen("v=1", &format!("v={version}"), 1))
                    .await?["state"],
                expected
            );
            assert!(a.mesh.join.lock().unwrap().is_none());
        }
        assert_eq!(a.join("unrelated clipboard").await?["state"], "no_link");
        no_tls_dial(&lb, Duration::from_millis(50)).await?;
        assert!(a.paired_devices().is_empty());
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn dial_order_uses_last_good_then_lan_hints_then_tailscale() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let (_db, b, _lb) = core().await?;
        bind(&a, &b).await?;
        let mut peer = a.store.peer_info().await?.remove(0);
        peer.route_proven = true;
        peer.addresses = vec![
            "192.0.2.1:47821".into(), // last-good
            "192.0.2.2:47821".into(), // HELLO hint
            "100.67.144.28:47821".into(),
            "192.0.2.3:47821".into(), // invitation hint
            "100.100.1.1:47821".into(),
            "192.0.2.2:47821".into(), // duplicate
        ];
        assert_eq!(
            a.dial_candidates(&peer),
            [
                DialCandidate::current("192.0.2.1:47821".into()),
                DialCandidate::cached("192.0.2.2:47821".into()),
                DialCandidate::cached("192.0.2.3:47821".into()),
                DialCandidate::current("100.67.144.28:47821".into()),
                DialCandidate::current("100.100.1.1:47821".into()),
            ]
        );
        // A last-good Tailscale route still follows secondary LAN hints.
        peer.addresses.swap(0, 2);
        let routes = a.dial_candidates(&peer);
        assert_eq!(routes[0], DialCandidate::cached("192.0.2.2:47821".into()));
        assert_eq!(routes[1], DialCandidate::cached("192.0.2.1:47821".into()));
        assert_eq!(
            routes[3],
            DialCandidate::current("100.67.144.28:47821".into())
        );
        peer.addresses = (1..=16).map(|n| format!("192.0.2.{n}:47821")).collect();
        let routes = a.dial_candidates(&peer);
        assert_eq!(routes.len(), 16);
        assert!(
            routes[1..]
                .iter()
                .all(|route| route.connect_timeout == Duration::from_secs(1))
        );
        peer.route_proven = false;
        assert!(
            a.dial_candidates(&peer)
                .iter()
                .all(|route| route.connect_timeout == Duration::from_secs(1)),
            "forwarded hints have no proven last-good route"
        );
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn persisted_hello_hints_precede_invitation_and_tailscale_fallbacks() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let (_db, b, _lb) = core().await?;
        computer_route(&a, &b, "100.67.144.28:47821".into()).await?;
        a.store
            .remember_route(b.identity.device_id.clone(), "192.0.2.3:47821".into())
            .await?;
        a.store
            .remember_route(b.identity.device_id.clone(), "192.0.2.1:47821".into())
            .await?;
        let stale = a.store.peer_info().await?.remove(0);
        let mut hello = b.computer_hello();
        hello.addresses = Some(vec!["192.0.2.2:47821".into(), "100.67.144.28:47821".into()]);
        a.store
            .remember_peer_hello(hello, Arc::new(AtomicU64::new(1)), 1, false)
            .await?;
        let latest = a.store.peer_info().await?;
        assert_eq!(
            latest[0].addresses,
            [
                "192.0.2.1:47821",
                "192.0.2.2:47821",
                "100.67.144.28:47821",
                "192.0.2.3:47821",
            ]
        );
        *a.mesh.peers.lock().unwrap() = latest;
        assert_eq!(
            a.dial_candidates(&stale),
            [
                DialCandidate::current("192.0.2.1:47821".into()),
                DialCandidate::cached("192.0.2.2:47821".into()),
                DialCandidate::cached("192.0.2.3:47821".into()),
                DialCandidate::current("100.67.144.28:47821".into()),
            ]
        );
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn dial_sweep_deadline_includes_admission_and_releases_permits() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let reserved = a
            .outbound_ingress
            .clone()
            .acquire_many_owned(OUTBOUND_ADMISSION_LIMIT as u32)
            .await?;
        let pin = Hash::new("a".repeat(64))?;
        let addresses = [_la.local_addr()?.to_string()];
        tokio::time::pause();
        let started = Instant::now();
        let mut dial = Box::pin(a.connect_computer(&addresses, pin, None));
        assert!(futures_util::poll!(dial.as_mut()).is_pending());
        tokio::time::advance(DIAL_SWEEP_TIMEOUT - Duration::from_secs(1)).await;
        assert!(futures_util::poll!(dial.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(dial.await.err().unwrap().to_string(), "unreachable");
        assert!(started.elapsed() >= DIAL_SWEEP_TIMEOUT);
        assert!(started.elapsed() <= DIAL_SWEEP_TIMEOUT + Duration::from_millis(1));
        assert_eq!(a.transports.available_permits(), TRANSPORT_LIMIT);
        drop(reserved);
        assert_eq!(
            a.outbound_ingress.available_permits(),
            OUTBOUND_ADMISSION_LIMIT
        );
        tokio::time::resume();
        a.shutdown_actions().await
    }
    #[tokio::test]
    async fn dial_sweep_deadline_cancels_an_active_handshake_after_admission_wait() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let listener = MockListener::new(TcpListener::bind("127.0.0.1:0").await?);
        let reserved = a
            .outbound_ingress
            .clone()
            .acquire_many_owned(OUTBOUND_ADMISSION_LIMIT as u32)
            .await?;
        let addresses = [listener.local_addr()?.to_string()];
        let pin = Hash::new("a".repeat(64))?;
        let mut dial = Box::pin(a.connect_computer(&addresses, pin, None));
        tokio::time::pause();
        assert!(futures_util::poll!(dial.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(25)).await;
        tokio::time::resume();
        drop(reserved);
        let (socket, _) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                _ = &mut dial => bail!("dial ended before TCP accept"),
                accepted = accept_tls_tcp(&listener) => accepted,
            }
        })
        .await??;
        assert!(futures_util::poll!(dial.as_mut()).is_pending());
        assert_eq!(
            a.outbound_ingress.available_permits(),
            OUTBOUND_ADMISSION_LIMIT - 1
        );
        assert_eq!(a.transports.available_permits(), TRANSPORT_LIMIT - 1);
        tokio::time::pause();
        // The TLS timer alone has another four seconds. The original sweep
        // deadline must still win, without restarting after admission.
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(matches!(
            futures_util::poll!(dial.as_mut()),
            std::task::Poll::Ready(Err(_))
        ));
        assert_eq!(
            a.outbound_ingress.available_permits(),
            OUTBOUND_ADMISSION_LIMIT
        );
        assert_eq!(a.transports.available_permits(), TRANSPORT_LIMIT);
        tokio::time::resume();
        drop((dial, socket));
        a.shutdown_actions().await
    }
    #[tokio::test]
    async fn inbound_version_errors_survive_unrelated_peer_registration_and_recovery() -> Result<()>
    {
        for error_frame in [false, true] {
            let (_da, a, la) = core().await?;
            let (_db, b, _lb) = core().await?;
            let (_dc, c, _lc) = core().await?;
            bind(&a, &b).await?;
            bind(&a, &c).await?;
            let address = la.local_addr()?.to_string();
            let server = Task::spawn(a.clone().serve(la, false));
            let config = crate::tls::client_config(&b.identity, a.identity.fingerprint.clone())?;
            let mut old: TlsStream<TcpStream> = TlsConnector::from(Arc::new(config))
                .connect(
                    rustls::pki_types::ServerName::try_from("pneu.local")?,
                    TcpStream::connect(&address).await?,
                )
                .await?
                .into();
            let ComputerLink { mut tls, .. } = c
                .connect_computer(
                    &[address],
                    a.identity.fingerprint.clone(),
                    Some(&a.identity.device_id),
                )
                .await?;
            c.confirm_peer_ready(&mut tls).await?;
            let generation = a.store.pairing_generation(&c.identity.device_id).unwrap();
            a.set_link_problem(
                &c.identity.device_id,
                generation,
                Some(LinkProblem::FingerprintMismatch),
            );
            a.set_link_problem(&c.identity.device_id, generation, None);
            if error_frame {
                send_error(&mut old, ErrorCode::VersionMismatch).await?;
            } else {
                let mut hello = b.computer_hello();
                hello.version = 2;
                send(&mut old, &Message::Hello(hello)).await?;
                assert!(
                    matches!(read_frame(&mut old).await?, Some(Message::Error(e)) if e.code == ErrorCode::VersionMismatch)
                );
            }
            until(|| a.link_problem(&b.identity.device_id) == Some(LinkProblem::UpdateRequired))
                .await?;
            assert!(a.connected_devices().contains(&c.identity.device_id));
            assert_eq!(a.link_problem(&c.identity.device_id), None);
            a.request_shutdown();
            server.await??;
            b.shutdown_actions().await?;
            c.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn outbound_hello_role_and_capability_errors_retain_protocol_classification() -> Result<()>
    {
        for variant in [
            "missing_role",
            "phone_role",
            "invalid_capability",
            "unknown_capability",
            "legacy",
        ] {
            let (_da, a, _la) = core().await?;
            let (_db, b, lb) = core().await?;
            let lb = MockListener::new(lb);
            bind(&a, &b).await?;
            let generation = a.store.pairing_generation(&b.identity.device_id);
            let fallback = MockListener::new(TcpListener::bind("127.0.0.1:0").await?);
            let addresses = [
                lb.local_addr()?.to_string(),
                fallback.local_addr()?.to_string(),
            ];
            let mock_core = b.clone();
            let mock = Task::spawn(async move {
                let mut tls = accept_client_hello(&mock_core, &lb).await?;
                let mut hello = mock_core.computer_hello();
                match variant {
                    "missing_role" => hello.role = None,
                    "phone_role" => hello.role = Some(DeviceRole::Phone),
                    "invalid_capability" => hello.capabilities.push("mesh".into()),
                    "unknown_capability" => hello.capabilities.push("future_feature".into()),
                    "legacy" => {
                        hello.capabilities.retain(|c| c != "mesh");
                        hello.role = Some(DeviceRole::Phone);
                    }
                    _ => unreachable!(),
                }
                if variant == "invalid_capability" {
                    // Bypass the sender codec to exercise the receiving parser.
                    let payload = serde_json::to_vec(&hello)?;
                    let mut wire = ((payload.len() + 1) as u32).to_be_bytes().to_vec();
                    wire.push(1);
                    wire.extend(payload);
                    tokio::io::AsyncWriteExt::write_all(&mut tls, &wire).await?;
                } else {
                    send(&mut tls, &Message::Hello(hello)).await?;
                }
                if matches!(
                    variant,
                    "missing_role" | "phone_role" | "invalid_capability"
                ) {
                    assert!(
                        matches!(read_frame(&mut tls).await?, Some(Message::Error(e)) if e.code == ErrorCode::MalformedMessage)
                    );
                }
                Ok(())
            });
            let result = a
                .connect_computer(
                    &addresses,
                    b.identity.fingerprint.clone(),
                    Some(&b.identity.device_id),
                )
                .await;
            let mock = mock_result(mock).await;
            let outcome = format!(
                "{variant}: dial: {:?}; mock: {mock:?}",
                result.as_ref().err()
            );
            if variant == "invalid_capability" {
                assert!(
                    result
                        .err()
                        .context(outcome.clone())?
                        .downcast_ref::<ProtocolError>()
                        .is_some(),
                    "{outcome}"
                );
            } else if matches!(variant, "missing_role" | "phone_role") {
                assert!(
                    result
                        .err()
                        .context(outcome.clone())?
                        .downcast_ref::<crate::client::PcRejection>()
                        .is_some_and(|e| e.0.code == ErrorCode::MalformedMessage),
                    "{outcome}"
                );
            } else {
                let link = result.context(outcome.clone())?;
                assert_eq!(link.hello.role, Some(DeviceRole::Computer), "{outcome}");
                if variant == "legacy" {
                    assert!(link.hello.addresses.is_none());
                }
            }
            mock.context(outcome)?;
            no_tls_dial(&fallback, Duration::from_millis(30)).await?;
            assert_eq!(
                a.store.pairing_generation(&b.identity.device_id),
                generation
            );
            assert_eq!(
                a.paired_devices(),
                std::slice::from_ref(&b.identity.device_id)
            );
            a.shutdown_actions().await?;
            b.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn mesh_only_settings_on_legacy_peer_report_update_required_without_mutation()
    -> Result<()> {
        let (_da, a, _la) = core().await?;
        let (_db, b, _lb) = core().await?;
        bind(&a, &b).await?;
        let device = &b.identity.device_id;
        let generation = a.store.pairing_generation(device);
        a.store
            .enqueue_clipboard(device.clone(), "legacy queue".into())
            .await?;
        assert_eq!(a.peer_settings(device.clone()).await?["can_sync"], false);
        let error = a
            .set_peer_setting(
                device.clone(),
                format!("{}/{device}/clipboard", a.identity.device_id),
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "update_required");
        assert_eq!(a.store.pairing_generation(device), generation);
        assert_eq!(a.paired_devices(), std::slice::from_ref(device));
        assert!(a.store.first_outgoing(device.clone()).await?.is_some());
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn obsolete_results_cannot_replace_newer_issue_or_recovery_or_revoke() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let (_db, b, _lb) = core().await?;
        bind(&a, &b).await?;
        let device = &b.identity.device_id;
        let generation = a.store.pairing_generation(device).unwrap();
        let old = a.link_epoch(device);
        a.set_link_problem(device, generation, Some(LinkProblem::FingerprintMismatch));
        a.set_link_problem_if(device, generation, old, Some(LinkProblem::UpdateRequired));
        assert_eq!(
            a.link_problem(device),
            Some(LinkProblem::FingerprintMismatch)
        );
        assert!(
            !a.forget_pairing_generation_checked(
                device.clone(),
                CloseReason::Error(ErrorCode::Closing),
                None,
                None,
                Some(generation),
                Some(old)
            )
            .await?
        );
        let old = a.link_epoch(device);
        a.set_link_problem(device, generation, None);
        a.set_link_problem_if(device, generation, old, Some(LinkProblem::UpdateRequired));
        assert_eq!(a.link_problem(device), None);
        // Registration history fences a result even if the admitted connection
        // has already disconnected when that result arrives.
        let old = a.link_epoch(device);
        a.record_link_registration(device, a.next_connection.fetch_add(1, Ordering::SeqCst));
        a.set_link_problem_if(device, generation, old, Some(LinkProblem::Unauthorized));
        assert_eq!(a.link_problem(device), None);
        assert!(
            !a.forget_pairing_generation_checked(
                device.clone(),
                CloseReason::Error(ErrorCode::Closing),
                None,
                None,
                Some(generation),
                Some(old)
            )
            .await?
        );
        assert_eq!(a.paired_devices(), vec![device.clone()]);
        a.shutdown_actions().await?;
        b.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn attempt_own_committed_install_is_reused_without_replacing_generation() -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, _lb) = core().await?;
        let address = la.local_addr()?.to_string();
        let uri = a
            .start_pairing_with_addresses(vec![address.clone()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        let pending = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let installed = pending.installed_generation.clone();
        let worker = Task::spawn(b.clone().join_worker(pending));
        let mut tls = accept_join_request(&a, &b, &la).await?;
        // Model an install whose waiter was interrupted after the commit
        // callback. Only this attempt's shared marker recognises ownership.
        let mut hello = a.computer_hello();
        hello.addresses = Some(vec![address]);
        let guard = b.pairing_gate.clone().lock_owned().await;
        b.persist_pairing_hello(
            &a.identity.device_id,
            &a.identity.fingerprint,
            guard,
            false,
            Some(hello),
            Some(installed.clone()),
        )
        .await?;
        let generation = b.store.pairing_generation(&a.identity.device_id);
        assert_eq!(Some(installed.load(Ordering::SeqCst)), generation);
        accepted(&mut tls).await?;
        until(|| b.connected_devices().contains(&a.identity.device_id)).await?;
        assert_eq!(b.join_status(attempt)?["state"], "paired");
        assert_eq!(
            b.store.pairing_generation(&a.identity.device_id),
            generation
        );
        assert_eq!(b.store.peer_info().await?[0].role, "computer");
        worker.abort();
        let _ = worker.await;
        drop(tls);
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn delayed_inbound_handshake_errors_cannot_overwrite_newer_registration() -> Result<()> {
        for variant in ["hello_version", "error", "compatible"] {
            let (_da, a, la) = core().await?;
            let (_db, b, _lb) = core().await?;
            bind(&a, &b).await?;
            let address = la.local_addr()?.to_string();
            let server = Task::spawn(a.clone().serve(la, false));
            let config = crate::tls::client_config(&b.identity, a.identity.fingerprint.clone())?;
            let tcp = TcpStream::connect(&address).await?;
            let mut old: TlsStream<TcpStream> = TlsConnector::from(Arc::new(config))
                .connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)
                .await?
                .into();
            let ComputerLink { mut tls, .. } = b
                .connect_computer(
                    &[address],
                    a.identity.fingerprint.clone(),
                    Some(&a.identity.device_id),
                )
                .await?;
            b.confirm_peer_ready(&mut tls).await?;
            if variant == "error" {
                send_error(&mut old, ErrorCode::VersionMismatch).await?;
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(2), read_frame(&mut old)).await?,
                    Ok(None) | Err(_)
                ));
            } else {
                let mut hello = b.computer_hello();
                hello.version = if variant == "compatible" { 1 } else { 2 };
                send(&mut old, &Message::Hello(hello)).await?;
                if variant == "compatible" {
                    assert!(matches!(
                        read_frame(&mut old).await?,
                        Some(Message::Hello(_))
                    ));
                    assert!(
                        matches!(read_frame(&mut old).await?, Some(Message::Error(e)) if e.code == ErrorCode::ConnectionReplaced)
                    );
                } else {
                    assert!(
                        matches!(read_frame(&mut old).await?, Some(Message::Error(e)) if e.code == ErrorCode::VersionMismatch)
                    );
                }
            }
            assert_eq!(a.ui_snapshot().devices[0].state, "connected");
            b.confirm_peer_ready(&mut tls).await?;
            a.request_shutdown();
            server.await??;
            b.shutdown_actions().await?;
        }
        Ok(())
    }
    include!("storage_failure_tests.rs");
    async fn scoped_binding(a: &Core, b: &Core, remote_enabled: bool) -> Result<()> {
        a.setup_test_capability.store(true, Ordering::SeqCst);
        b.setup_test_capability.store(true, Ordering::SeqCst);
        let mut hello = b.computer_hello();
        hello.setup = Some(SetupHello {
            scope: if remote_enabled {
                SetupScope::Enabled
            } else {
                SetupScope::Disabled
            },
            setup_id: None,
        });
        let guard = a.pairing_gate.clone().lock_owned().await;
        a.persist_pairing_hello_scoped(
            &b.identity.device_id,
            &b.identity.fingerprint,
            guard,
            false,
            Some(hello),
            None,
            Some(true),
        )
        .await?;
        Ok(())
    }
    async fn assign_from(a: &Core, b: &Core) -> Result<SetupFence> {
        let mut fence = SetupFence {
            generation: a.store.setup_state().generation,
            binding_generation: a.store.pairing_generation(&b.identity.device_id).unwrap(),
            device: b.identity.device_id.clone(),
            fingerprint: b.identity.fingerprint.clone(),
            connection: 100,
            current: Arc::new(AtomicU64::new(100)),
        };
        a.store
            .setup_activate(
                fence.clone(),
                a.self_member(),
                b.self_member(),
                SetupHello {
                    scope: SetupScope::Enabled,
                    setup_id: Some(
                        a.store
                            .setup_state()
                            .setup_id
                            .unwrap_or_else(|| Id::from_bytes([42; 16])),
                    ),
                },
                |_| {},
            )
            .await?;
        fence.generation = a.store.setup_state().generation;
        Ok(fence)
    }
    #[tokio::test]
    async fn setup_advisory_one_sided_scope_keeps_ready_and_prehello_loss_handling() -> Result<()> {
        let mut outcomes = Vec::new();
        for ready in [true, false] {
            let (_da, a, la) = core().await?;
            let (_db, b, lb) = core().await?;
            let lb = MockListener::new(lb);
            drop(la);
            scoped_binding(&a, &b, false).await?;
            bind(&b, &a).await?;
            b.store
                .setup_link(a.identity.device_id.clone(), SetupScope::Disabled)
                .await?;
            a.store
                .remember_route(b.identity.device_id.clone(), lb.local_addr()?.to_string())
                .await?;
            a.store
                .mark_peer_established(b.identity.device_id.clone())
                .await?;
            let peer = a.store.peer_info().await?.remove(0);
            let (cancel, rx) = watch::channel(false);
            let worker = Task::spawn(a.clone().dial_worker(peer, rx));
            let mut tls = accept_client_hello(&b, &lb).await?;
            if ready {
                let mut hello = b.computer_hello();
                hello.setup = Some(SetupHello {
                    scope: SetupScope::Disabled,
                    setup_id: None,
                });
                send(&mut tls, &Message::Hello(hello)).await?;
                let Some(Message::Ping(p)) = read_frame(&mut tls).await? else {
                    bail!("expected READY ping");
                };
                send(&mut tls, &Message::Pong(p)).await?;
                until(|| a.connected_devices().contains(&b.identity.device_id)).await?;
            }
            send_error(&mut tls, ErrorCode::Unauthorized).await?;
            outcomes.push((ready, until(|| a.paired_devices().is_empty()).await.is_ok()));
            cancel.send_replace(true);
            worker.await??;
            a.shutdown_actions().await?;
            b.shutdown_actions().await?;
        }
        assert_eq!(
            outcomes,
            vec![(true, true), (false, true)],
            "READY and pre-HELLO refusal must both forget this direct link"
        );
        Ok(())
    }
    #[tokio::test]
    async fn setup_advisory_disabled_remote_unpair_keeps_unrelated_setup_and_rechecks_gate()
    -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        let (_dc, c, _lc) = core().await?;
        drop(la);
        scoped_binding(&a, &c, true).await?;
        let fence = assign_from(&a, &c).await?;
        scoped_binding(&a, &b, false).await?;
        bind(&b, &a).await?;
        let mut member = b.self_member();
        member.hlc = crate::now_ms();
        let mut roster = a.store.setup_state().roster;
        roster.push(member);
        let setup_id = a.store.setup_state().setup_id.unwrap();
        a.store
            .members_merge(
                fence,
                a.self_member(),
                Members {
                    transfer_id: Id::from_bytes(rand::random()),
                    setup_id: setup_id.clone(),
                    members: roster,
                },
                Vec::new(),
                |_| {},
            )
            .await?;
        a.store
            .remember_route(b.identity.device_id.clone(), lb.local_addr()?.to_string())
            .await?;
        let peer = a
            .store
            .peer_info()
            .await?
            .into_iter()
            .find(|p| p.id == b.identity.device_id)
            .unwrap();
        let (cancel, rx) = watch::channel(false);
        let worker = Task::spawn(a.clone().dial_worker(peer, rx));
        let mut tls = accept_client_hello(&b, &lb).await?;
        let mut hello = b.computer_hello();
        hello.setup = Some(SetupHello {
            scope: SetupScope::Disabled,
            setup_id: None,
        });
        send(&mut tls, &Message::Hello(hello.clone())).await?;
        let Some(Message::Ping(p)) = read_frame(&mut tls).await? else {
            bail!("READY ping");
        };
        send(&mut tls, &Message::Pong(p)).await?;
        until(|| a.connected_devices().contains(&b.identity.device_id)).await?;
        // Recheck under the gate even if a caller observed participation before
        // waiting: a newer authenticated disabled HELLO has won the lifecycle.
        let mut captured = a.store.with_current_pairing();
        captured.connection_id =
            Some(a.sessions.lock().unwrap().sessions[&b.identity.device_id].id);
        captured.setup_generation = Some(a.store.setup_state().generation);
        let guard = a.pairing_gate.clone().lock_owned().await;
        let mut withdraw = Box::pin(a.withdraw_setup(&captured, &hello));
        assert!(futures_util::poll!(withdraw.as_mut()).is_pending());
        drop(guard);
        assert!(!withdraw.await?);
        send(&mut tls, &Message::Unpair(Unpair {})).await?;
        until(|| !a.paired_devices().contains(&b.identity.device_id)).await?;
        assert_eq!(a.store.setup_state().setup_id, Some(setup_id));
        assert!(!a.store.setup_state().withdrawn);
        assert!(a.paired_devices().contains(&c.identity.device_id));
        cancel.send_replace(true);
        worker.await??;
        a.shutdown_actions().await?;
        b.shutdown_actions().await?;
        c.shutdown_actions().await
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_review_retry_cannot_resurrect_withdrawn_join() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        let uri = b
            .start_pairing_with_addresses(vec![lb.local_addr()?.to_string()])
            .await?;
        let preview = a.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        a.confirm_join(attempt).await?;
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocking) = std::sync::mpsc::channel();
        *a.mesh.retry_hook.lock().unwrap() = Some((entered, blocking));
        a.mesh.panic_join.store(true, Ordering::SeqCst);
        let supervisor = Task::spawn(a.clone().mesh_supervise());
        let mut tls = accept_hello(&b, &lb, None).await?;
        assert!(matches!(
            read_frame(&mut tls).await?,
            Some(Message::PairRequest(_))
        ));
        drop(tls);
        tokio::time::timeout(Duration::from_secs(5), wait).await??;
        let held = a.mesh.join.try_lock().is_err();
        let cancellation = if held {
            let cancel = a.clone();
            Some(tokio::task::spawn_blocking(move || {
                cancel.mesh.cancel_withdrawn_join();
            }))
        } else {
            // Complete withdrawal in the old clone-and-replace gap.
            a.mesh.cancel_withdrawn_join();
            None
        };
        release.send(())?;
        if let Some(cancellation) = cancellation {
            cancellation.await?;
        }
        a.request_shutdown();
        supervisor.await??;
        assert_eq!(a.join_status(attempt)?["state"], "cancelled");
        assert!(a.mesh.join.lock().unwrap().as_ref().unwrap().phase == JoinPhase::Finished);
        assert!(held, "retry exposed a stale clone to withdrawal");
        assert!(a.paired_devices().is_empty());
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn setup_review_stale_periodic_refresh_preserves_rename_on_restart() -> Result<()> {
        let (_da, a, la) = core().await?;
        let (_db, b, _lb) = core().await?;
        scoped_binding(&a, &b, true).await?;
        assign_from(&a, &b).await?;
        let (captured, wait) = tokio::sync::oneshot::channel();
        let (release, blocked) = tokio::sync::oneshot::channel();
        let refresher = a.clone();
        let refresh = tokio::spawn(async move {
            let mut stale = refresher.self_member();
            stale.reach.as_mut().unwrap().addresses = vec!["127.0.0.1:45678".into()];
            let _ = captured.send(());
            blocked.await?;
            refresher.store.setup_refresh_self(stale).await
        });
        wait.await?;
        a.rename("Renamed computer".into()).await?;
        let own = || {
            a.store
                .setup_state()
                .roster
                .into_iter()
                .find(|m| m.device_id == a.identity.device_id)
                .unwrap()
        };
        let renamed = own();
        release.send(()).unwrap();
        refresh.await??;
        let refreshed = own();
        assert_eq!(refreshed.name, "Renamed computer");
        assert_eq!(refreshed.hlc, renamed.hlc);
        assert_eq!(refreshed.writer_device, renamed.writer_device);
        let reach = refreshed.reach.unwrap();
        assert_eq!(reach.addresses, vec!["127.0.0.1:45678"]);
        assert!(reach.hlc > renamed.hlc);
        let paths = a.paths.clone();
        let config = Config::load(&paths.config)?;
        a.shutdown_actions().await?;
        a.store.drain().await;
        drop(la);
        drop(a);
        let restarted = Core::new(paths, config, Arc::new(Mock)).await?;
        assert_eq!(restarted.config().name, "Renamed computer");
        assert_eq!(
            Config::load(&restarted.paths.config)?.name,
            "Renamed computer"
        );
        restarted.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_advisory_confirmation_holds_join_mutex_through_mutation() -> Result<()> {
        for withdrawal in [false, true] {
            let (_da, a, _la) = core().await?;
            let (_db, b, lb) = core().await?;
            let uri = b
                .start_pairing_with_addresses(vec![lb.local_addr()?.to_string()])
                .await?;
            let preview = a.join(&uri.build()?).await?;
            let attempt = preview["attempt"].as_str().unwrap().to_string();
            let (entered, wait) = tokio::sync::oneshot::channel();
            let (release, blocking) = std::sync::mpsc::channel();
            *a.mesh.confirm_hook.lock().unwrap() = Some((entered, blocking));
            let confirmer = a.clone();
            let id = attempt.clone();
            let confirm =
                tokio::spawn(async move { confirmer.confirm_join_scoped(&id, true).await });
            wait.await?;
            let held = a.mesh.join.try_lock().is_err();
            let cancel = a.clone();
            let key = (b.identity.device_id.clone(), b.identity.fingerprint.clone());
            let cancellation = if held {
                Some(tokio::task::spawn_blocking(move || {
                    if withdrawal {
                        cancel.mesh.cancel_withdrawn_join();
                    } else {
                        cancel.mesh.cancel_removed(&[key]);
                    }
                }))
            } else {
                // With the old clone-and-replace path, complete cancellation
                // inside that gap before confirmation can publish its clone.
                if withdrawal {
                    cancel.mesh.cancel_withdrawn_join();
                } else {
                    cancel.mesh.cancel_removed(&[key]);
                }
                None
            };
            release.send(())?;
            confirm.await??;
            if let Some(cancellation) = cancellation {
                cancellation.await?;
            }
            assert_eq!(a.join_status(&attempt)?["state"], "cancelled");
            assert!(held, "confirmation exposed a stale clone to cancellation");
            a.shutdown_actions().await?;
            b.shutdown_actions().await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn setup_advisory_cancelled_worker_rechecks_after_pairing_gate_before_request()
    -> Result<()> {
        let (_da, a, la) = core().await?;
        let la = MockListener::new(la);
        let (_db, b, _lb) = core().await?;
        let uri = a
            .start_pairing_with_addresses(vec![la.local_addr()?.to_string()])
            .await?;
        let preview = b.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.confirm_join(attempt).await?;
        let pending = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocked) = tokio::sync::oneshot::channel();
        *b.mesh.request_hook.lock().unwrap() = Some((entered, blocked));
        let worker = Task::spawn(b.clone().join_worker(pending));
        let mut tls = accept_hello(&a, &la, None).await?;
        wait.await?;
        let guard = b.pairing_gate.clone().lock_owned().await;
        b.mesh
            .cancel_removed(&[(a.identity.device_id.clone(), a.identity.fingerprint.clone())]);
        release.send(()).unwrap();
        drop(guard);
        closed_without_unpair(&mut tls).await?;
        assert_eq!(worker.await?.unwrap_err().to_string(), "cancelled");
        let join = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        assert!(!join.requested);
        assert!(join.authority.is_none());
        assert_eq!(b.join_status(attempt)?["state"], "cancelled");
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn setup_advisory_plain_unpair_rechecks_member_after_waiting_for_gate() -> Result<()> {
        let (_da, a, _la) = core().await?;
        let (_db, b, _lb) = core().await?;
        scoped_binding(&a, &b, true).await?;
        let guard = a.pairing_gate.clone().lock_owned().await;
        let mut unpair = Box::pin(a.unpair_device(b.identity.device_id.clone()));
        assert!(futures_util::poll!(unpair.as_mut()).is_pending());
        assign_from(&a, &b).await?;
        let before = a.store.setup_state();
        drop(guard);
        assert!(
            unpair
                .await
                .unwrap_err()
                .to_string()
                .contains("confirm Remove from all devices")
        );
        assert!(a.paired_devices().contains(&b.identity.device_id));
        assert_eq!(a.store.setup_state(), before);
        a.shutdown_actions().await?;
        b.shutdown_actions().await
    }
    #[tokio::test]
    async fn setup_introduced_authenticated_refusal_keeps_trust_before_and_after_ready()
    -> Result<()> {
        for ready in [false, true] {
            let (_da, a, _la) = core().await?;
            let (_db, b, lb) = core().await?;
            let lb = MockListener::new(lb);
            computer_route(&a, &b, lb.local_addr()?.to_string()).await?;
            bind(&b, &a).await?;
            let device = b.identity.device_id.clone();
            let d = device.clone();
            a.store.call(move |c| { c.execute("UPDATE setup_links SET introduced_by=?,introducer_name='Introducer',negotiated=1,participating=1 WHERE device=?",rusqlite::params![Id::from_bytes([9;16]).as_str(),d.as_str()])?;Ok(()) }).await?;
            a.store
                .setup_link(device.clone(), SetupScope::Enabled)
                .await?;
            let generation = a.store.pairing_generation(&device).unwrap();
            assert!(a.store.setup_protected(&device));
            assert_eq!(a.setup_status().await?["links"][0]["hello_seen"], false);
            assert_eq!(a.setup_status().await?["links"][0]["ready"], false);
            let peer = a.store.peer_info().await?.remove(0);
            let (cancel, rx) = watch::channel(false);
            let worker = Task::spawn(a.clone().dial_worker(peer, rx));
            let mut tls = if ready {
                accept_ready(&b, &lb, None).await?
            } else {
                accept_client_hello(&b, &lb).await?
            };
            if ready {
                until(|| a.connected_devices().contains(&device)).await?;
                wait_established(&a, &device).await?;
            }
            send_error(&mut tls, ErrorCode::Unauthorized).await?;
            until(|| a.ui_snapshot().devices[0].state == "unauthorized").await?;
            assert_eq!(a.paired_devices(), vec![device.clone()]);
            assert_eq!(a.store.pairing_generation(&device), Some(generation));
            assert!(a.store.setup_protected(&device));
            assert!(
                a.store
                    .setup_state()
                    .roster
                    .iter()
                    .all(|m| m.status != MemberStatus::Removed)
            );
            cancel.send_replace(true);
            worker.await??;
            drop(tls);
            // A direct successful READY in this generation clears the hint.
            let peer = a.store.peer_info().await?.remove(0);
            a.mesh
                .links
                .lock()
                .unwrap()
                .get_mut(&device)
                .unwrap()
                .issue
                .as_mut()
                .unwrap()
                .retry_at = Instant::now();
            let (cancel, rx) = watch::channel(false);
            let worker = Task::spawn(a.clone().dial_worker(peer, rx));
            let tls = accept_ready(&b, &lb, None).await?;
            until(|| a.connected_devices().contains(&device)).await?;
            assert!(a.link_problem(&device).is_none());
            cancel.send_replace(true);
            worker.await??;
            drop(tls);
            a.store
                .pair(device.clone(), b.identity.fingerprint.clone())
                .await?;
            a.set_link_problem(&device, generation, Some(LinkProblem::Unauthorized));
            assert!(
                a.link_problem(&device).is_none(),
                "old generation refusal reached replacement"
            );
            assert!(link_problem(&anyhow::anyhow!("peer idle timeout")).is_none());
            assert!(link_problem(&anyhow::anyhow!("TLS handshake failed")).is_none());
            a.shutdown_actions().await?;
            b.shutdown_actions().await?;
        }
        Ok(())
    }
}
