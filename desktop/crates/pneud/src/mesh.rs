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
}
#[derive(Default)]
pub(super) struct Mesh {
    peers: Mutex<Vec<PeerInfo>>,
    join: Mutex<Option<Join>>,
    gate: tokio::sync::Mutex<()>,
    pub(super) changed: Notify,
    links: Mutex<HashMap<Id, LinkState>>,
    connecting: Mutex<HashSet<Id>>,
    #[cfg(test)]
    panic_join: AtomicBool,
    #[cfg(test)]
    panic_dial: AtomicBool,
    #[cfg(test)]
    panic_supervisor: AtomicBool,
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
}
impl Mesh {
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
    pub(super) fn computer_hello(&self) -> Hello {
        let addresses = crate::discovery::pairing_candidates()
            .unwrap_or_default()
            .into_iter()
            .map(|i| SocketAddr::new(i.ip, self.bound_port()).to_string())
            .collect();
        Hello {
            version: 1,
            device_id: self.identity.device_id.clone(),
            name: self.config().name,
            role: Some(DeviceRole::Computer),
            addresses: Some(addresses),
            paused: None,
            capabilities: vec![
                "files".into(),
                "clipboard_text".into(),
                "clipboard_image".into(),
                "mesh".into(),
            ],
        }
    }
    pub(super) fn device_snapshots(
        &self,
        sessions: &Connections,
        admission: &Admission,
        ui: &crate::ui::State,
    ) -> Vec<DeviceSnapshot> {
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
                    name: phone
                        .map(|p| p.name.clone())
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
                    last_seen: phone.map(|p| p.last_seen).unwrap_or(0),
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
    fn link_problem(&self, device: &Id) -> Option<LinkProblem> {
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
            link.issue = Some(LinkIssue {
                generation,
                problem,
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
        Ok(join_value(join))
    }
    pub async fn confirm_join(&self, attempt: &str) -> Result<Value> {
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
        ensure!(
            join.phase != JoinPhase::Finished,
            "join cannot be confirmed"
        );
        if join.phase == JoinPhase::Preview && join.uri.expires_at <= crate::now_ms() {
            join.state = "expired".into();
            join.phase = JoinPhase::Finished;
        } else {
            join.phase = JoinPhase::Pending;
            join.state = "pairing".into();
            join.retry_at = None;
        }
        let value = join_value(&join);
        *self.mesh.join.lock().expect("join") = Some(join);
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
        let connector =
            TlsConnector::from(Arc::new(crate::tls::client_config(&self.identity, pin)?));
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
                let sent = send(&mut tls, &Message::Hello(self.computer_hello())).await;
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
                Ok(ComputerLink { tls, hello, remote, address: address.clone(), permit, transport })
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
            let _pairing = self.pairing_gate.lock().await;
            let mut state = self.mesh.join.lock().expect("join");
            let current = state.as_mut().context("join disappeared")?;
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
                    | Message::PeerSettings(_)) => {
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
            drop(guard);
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
        self.register_buffered(
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
                    | Message::PeerSettings(_)) => {
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
                let dial = async {
                    let addresses = self.dial_candidates(&peer);
                    let ComputerLink {
                        mut tls,
                        hello,
                        remote,
                        address,
                        permit,
                        transport: _transport,
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
                        .register_buffered(
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
                        )
                        .await
                };
                let _connecting = Connecting::new(self.clone(), peer.id.clone());
                #[cfg(test)]
                if self.mesh.panic_dial.swap(false, Ordering::SeqCst) {
                    panic!("injected dial failure");
                }
                let result = tokio::select! { biased; _ = cancelled.changed() => break, _ = revoked.changed() => break, _ = shutdown.changed() => break, result = dial => result };
                if let Err(error) = &result
                    && error
                        .downcast_ref::<crate::client::PcRejection>()
                        .is_some_and(|e| e.0.code == ErrorCode::Unauthorized)
                    && may_revoke
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
            .filter(|a| !tailscale_endpoint(a))
            .cloned()
            .map(DialCandidate::current)
            .into_iter()
            .chain(
                latest
                    .addresses
                    .iter()
                    .skip(1)
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
        loop {
            tokio::select! { biased; _ = stop.changed() => break, _ = tick.tick() => {} }
            if *stop.borrow() {
                break;
            }
            #[cfg(test)]
            if self.mesh.panic_supervisor.swap(false, Ordering::SeqCst) {
                panic!("injected supervisor failure");
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
                        if !self.joining(&attempt) {
                            continue;
                        }
                        let mut pending = self
                            .mesh
                            .join
                            .lock()
                            .expect("join")
                            .as_ref()
                            .expect("join")
                            .clone();
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
                        *self.mesh.join.lock().expect("join") = Some(pending);
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
    json!({"attempt":join.attempt,"state":join.state,"name":join.uri.name,"fingerprint":join.uri.fingerprint,"role":"computer"})
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

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn core() -> Result<(tempfile::TempDir, Arc<Core>, TcpListener)> {
        let dir = crate::test_support::scratch()?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
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
        eprintln!(
            "mock Core PID={} port={} directory={} (no mDNS)",
            std::process::id(),
            core.bound_port(),
            dir.path().display()
        );
        Ok((dir, core, listener))
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
            assert!(
                result
                    .unwrap_err()
                    .downcast_ref::<crate::client::PcRejection>()
                    .is_some_and(|e| e.0.code == ErrorCode::Unauthorized)
            );
            assert!(a.paired_devices().is_empty());
            assert!(a.store.peer_info().await?.is_empty());
            assert_eq!(a.store.pairing_generation(&a.identity.device_id), None);
            mock.await??;
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
        let (_da, mut a, mut la) = core().await?;
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
            la = TcpListener::bind("127.0.0.1:0").await?;
            config.port = la.local_addr()?.port();
            a = Core::new(paths, config, Arc::new(Mock)).await?;
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
        la: &TcpListener,
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
        let (mut tcp, _) = la.accept().await?;
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
        assert!(b.join_status(attempt).is_err());
        assert!(!b.paths.data.join("join-pending.json").exists());
        let fresh = a.start_pairing_with_addresses(vec![address]).await?;
        assert_ne!(uri.token, fresh.token);
        let server = Task::spawn(a.clone().serve(la, false));
        let preview = b.join(&fresh.build()?).await?;
        let next = preview["attempt"].as_str().unwrap();
        b.confirm_join(next).await?;
        let pending = b.mesh.join.lock().unwrap().as_ref().unwrap().clone();
        let worker = Task::spawn(b.clone().join_worker(pending));
        until(|| b.join_status(next).unwrap()["state"] == "paired").await?;
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
        let a_server = Task::spawn(a.clone().serve(la, false));
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
        let lower = if a.identity.device_id.as_str() < b.identity.device_id.as_str() {
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
        a.request_shutdown();
        b.request_shutdown();
        sa.await??;
        sb.await??;
        wa.await??;
        wb.await??;
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
        // Only B listens: its HELLO-advertised physical addresses cannot reach
        // A's absent listener. This deliberately exercises a lone higher dial.
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
            assert!(
                tokio::time::timeout(Duration::from_millis(50), lb.accept())
                    .await
                    .is_err(),
                "{failure} retried too soon"
            );
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
        let server = Task::spawn(b.clone().serve(lb, false));
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
        drop(la);
        computer_route(&a, &b, lb.local_addr()?.to_string()).await?;
        let paths = a.paths.clone();
        let config = a.config();
        let generation = a.store.pairing_generation(&b.identity.device_id);
        a.shutdown_actions().await?;
        a.store.drain().await;
        drop(a);
        let a = Core::new(paths, config, Arc::new(Mock)).await?;
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
        listener: &TcpListener,
        addresses: Option<Vec<String>>,
    ) -> Result<TlsStream<TcpStream>> {
        accept_hello_version(core, listener, addresses, 1).await
    }
    async fn accept_hello_version(
        core: &Core,
        listener: &TcpListener,
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
    async fn accept_client_hello(
        core: &Core,
        listener: &TcpListener,
    ) -> Result<TlsStream<TcpStream>> {
        let verifier = Arc::new(ClientVerifier::new(
            core.admission.clone(),
            core.provisional.clone(),
        ));
        let config = crate::tls::server_config(&core.identity, verifier.clone())?;
        let (tcp, _) = listener.accept().await?;
        let mut tls: TlsStream<TcpStream> = TlsAcceptor::from(Arc::new(config))
            .accept(tcp)
            .await?
            .into();
        assert!(matches!(
            read_frame(&mut tls).await?,
            Some(Message::Hello(_))
        ));
        verifier.release_provisional();
        Ok(tls)
    }
    async fn accept_ready(
        core: &Core,
        listener: &TcpListener,
        addresses: Option<Vec<String>>,
    ) -> Result<TlsStream<TcpStream>> {
        let mut tls = accept_hello(core, listener, addresses).await?;
        let Some(Message::Ping(ping)) = read_frame(&mut tls).await? else {
            bail!("READY confirmation PING");
        };
        send(&mut tls, &Message::Pong(ping)).await?;
        Ok(tls)
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

        let stalled = TcpListener::bind("127.0.0.1:0").await?;
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
                tokio::time::timeout(Duration::from_secs(2), stalled.accept())
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
        let first = TcpListener::bind("127.0.0.1:0").await?;
        let second = TcpListener::bind("127.0.0.1:0").await?;
        let other = TcpListener::bind("127.0.0.1:0").await?;
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
            tokio::time::timeout(Duration::from_secs(2), first.accept()).await??;
        let addresses = [other.local_addr()?.to_string()];
        let mut queued =
            Box::pin(a.connect_computer(&addresses, b.identity.fingerprint.clone(), None));
        // Poll to queue this candidate behind the sweep's occupied permit.
        assert!(futures_util::poll!(queued.as_mut()).is_pending());
        drop(first_socket);
        let (other_socket, _) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut queued => bail!("queued handshake unexpectedly finished: {}", result.is_ok()),
                accepted = other.accept() => Ok(accepted?),
            }
        }).await??;
        // The sweep must yield to the earlier waiter, rather than retain a
        // permit while opening its second route.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second.accept())
                .await
                .is_err()
        );
        drop((queued, other_socket));
        let (second_socket, _) =
            tokio::time::timeout(Duration::from_secs(2), second.accept()).await??;
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
            assert!(
                tokio::time::timeout(Duration::from_millis(50), lb.accept())
                    .await
                    .is_err()
            );
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
        let (_db, b, old) = core().await?;
        drop(la);
        let moved = TcpListener::bind("127.0.0.1:0").await?;
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
        drop(old);
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
        drop(la);
        computer_route(&a, &b, lb.local_addr()?.to_string()).await?;
        bind(&b, &a).await?;
        let supervisor = Task::spawn(a.clone().mesh_supervise());
        // Stall TLS after TCP admission. The snapshot exposes an active dial.
        let (tcp, _) = lb.accept().await?;
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
        let server = Task::spawn(a.clone().serve(la, false));
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
        assert!(
            tokio::time::timeout(Duration::from_millis(50), lb.accept())
                .await
                .is_err()
        );
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
        let listener = TcpListener::bind("127.0.0.1:0").await?;
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
                accepted = listener.accept() => Ok(accepted?),
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
            bind(&a, &b).await?;
            let generation = a.store.pairing_generation(&b.identity.device_id);
            let fallback = TcpListener::bind("127.0.0.1:0").await?;
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
            if variant == "invalid_capability" {
                assert!(
                    result
                        .err()
                        .unwrap()
                        .downcast_ref::<ProtocolError>()
                        .is_some()
                );
            } else if matches!(variant, "missing_role" | "phone_role") {
                assert!(
                    result
                        .err()
                        .unwrap()
                        .downcast_ref::<crate::client::PcRejection>()
                        .is_some_and(|e| e.0.code == ErrorCode::MalformedMessage)
                );
            } else {
                let link = result?;
                assert_eq!(link.hello.role, Some(DeviceRole::Computer));
                if variant == "legacy" {
                    assert!(link.hello.addresses.is_none());
                }
            }
            mock.await??;
            assert!(
                tokio::time::timeout(Duration::from_millis(30), fallback.accept())
                    .await
                    .is_err()
            );
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
}
