//! IO driver for the clipboard state machine. No slow operation is awaited by
//! its event loop. Jobs return tagged events; the actor decides their relevance.
use super::*;
use crate::actions::ClipboardInput;
use crate::core::{ActionOrigin, ActionScope, Core};
use crate::store::{ClipboardReservation, Store};
use anyhow::{Result, anyhow};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{oneshot, watch};
#[path = "clipboard_mailbox.rs"]
mod mailbox;

#[derive(Debug)]
pub(in crate::core) struct Outcome {
    pub deduplicated: bool,
    pub applied: bool,
}
pub(in crate::core) struct Request {
    pub input: ClipboardInput,
    pub claim: Option<ClipboardReservation>,
    pub store: Store,
    pub authority: Option<(ActionScope, ActionOrigin)>,
    pub kind: Option<pneu_proto::FileKind>,
    pub policy: Vec<pneu_proto::PeerSetting>,
    pub reply: oneshot::Sender<Result<Outcome>>,
}
pub(in crate::core) enum Message {
    Event(Event),
    Share(
        Event,
        oneshot::Sender<Result<Outcome>>,
        tokio::sync::OwnedSemaphorePermit,
    ),
    InstallMarker(Ticket, oneshot::Sender<bool>),
    Submit(Operation, Box<Request>, tokio::sync::OwnedSemaphorePermit),
    ReadHandle(Token, Arc<std::sync::atomic::AtomicBool>),
    #[cfg(test)]
    Barrier(oneshot::Sender<()>),
    #[cfg(test)]
    Wait(Token, oneshot::Sender<Result<Outcome>>),
    Stop(oneshot::Sender<()>),
}
#[derive(Clone)]
pub(in crate::core) struct Handle {
    pub tx: mailbox::Sender,
    admission: Arc<tokio::sync::Semaphore>,
    sequence: Arc<AtomicU64>,
}
impl Handle {
    pub fn channel() -> (Self, mailbox::Receiver) {
        let (tx, rx) = mailbox::channel();
        (
            Self {
                tx,
                admission: Arc::new(tokio::sync::Semaphore::new(61)),
                sequence: Arc::new(AtomicU64::new(1)),
            },
            rx,
        )
    }
    pub fn token(&self) -> Token {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }
    pub fn event(&self, event: Event) {
        let _ = self.tx.send(Message::Event(event));
    }
    pub async fn submit(&self, operation: Operation, mut request: Request) -> Result<Outcome> {
        let permit = self
            .admission
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("clipboard actor stopped"))?;
        let token = operation.token;
        let (tx, rx) = oneshot::channel();
        request.reply = tx;
        self.tx
            .send(Message::Submit(operation, Box::new(request), permit))
            .map_err(|_| anyhow!("clipboard actor stopped"))?;
        struct Cancellation {
            handle: Handle,
            token: Token,
        }
        impl Drop for Cancellation {
            fn drop(&mut self) {
                self.handle.event(Event::Cancel(self.token));
            }
        }
        let _cancel = Cancellation {
            handle: self.clone(),
            token,
        };
        rx.await.map_err(|_| anyhow!("clipboard actor stopped"))?
    }
    #[cfg(test)]
    pub async fn barrier(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Message::Barrier(tx)).is_ok() {
            let _ = rx.await;
        }
    }
    #[cfg(test)]
    pub fn waiter(&self, token: Token) -> oneshot::Receiver<Result<Outcome>> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(Message::Wait(token, tx));
        rx
    }
    pub async fn share(&self, event: Event) -> Result<Outcome> {
        let permit = self.admission.clone().acquire_owned().await?;
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Message::Share(event, tx, permit))
            .map_err(|_| anyhow!("clipboard actor stopped"))?;
        rx.await.map_err(|_| anyhow!("clipboard actor stopped"))?
    }
    pub async fn stop(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Message::Stop(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}
struct Helper {
    stop: watch::Sender<bool>,
    authorize: Option<oneshot::Sender<()>>,
}
struct Driver {
    core: Weak<Core>,
    actions: Arc<dyn crate::actions::PcActions>,
    state: State,
    handle: Handle,
    requests: HashMap<Token, Request>,
    shares: HashMap<Token, oneshot::Sender<Result<Outcome>>>,
    admissions: HashMap<Token, tokio::sync::OwnedSemaphorePermit>,
    helpers: HashMap<Ticket, Helper>,
    reads: HashMap<Token, Arc<std::sync::atomic::AtomicBool>>,
    jobs: tokio::task::JoinSet<()>,
    job_failures: HashMap<tokio::task::Id, Vec<Event>>,
    stopping: Option<oneshot::Sender<()>>,
    barriers: Vec<oneshot::Sender<()>>,
    #[cfg(test)]
    waiters: HashMap<Token, oneshot::Sender<Result<Outcome>>>,
    timers: Vec<(tokio::time::Instant, Event)>,
    clock_jobs: HashMap<Token, tokio::task::AbortHandle>,
    share_jobs: HashMap<Token, tokio::task::AbortHandle>,
    history_revision: u64,
}
impl Driver {
    fn spawn(
        &mut self,
        failure: Vec<Event>,
        job: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> tokio::task::AbortHandle {
        let task = self.jobs.spawn(job);
        self.job_failures.insert(task.id(), failure);
        task
    }
    fn joined(&mut self, result: Result<(tokio::task::Id, ()), tokio::task::JoinError>) {
        match result {
            Ok((id, ())) => {
                self.job_failures.remove(&id);
            }
            Err(error) => {
                let events = self.job_failures.remove(&error.id()).unwrap_or_default();
                if events.is_empty() {
                    return;
                }
                // Dropped PreparedClipboard values kill and enqueue child reap.
                // Drain that work before declaring a panicked helper stopped.
                let helper = events.iter().any(|e| matches!(e, Event::Stopped(_)));
                let actions = self.actions.clone();
                let handle = self.handle.clone();
                self.spawn(Vec::new(), async move {
                    if helper {
                        use futures_util::FutureExt;
                        let _ = std::panic::AssertUnwindSafe(actions.drain_clipboard())
                            .catch_unwind()
                            .await;
                    }
                    for event in events {
                        handle.event(event);
                    }
                });
            }
        }
    }

    fn send(&self, event: Event) {
        self.handle.event(event);
    }
    fn authorized(&self, operation: &Operation) -> bool {
        let Some(core) = self.core.upgrade() else {
            return false;
        };
        self.requests.get(&operation.token).is_none_or(|request| {
            request.authority.as_ref().is_none_or(|(scope, origin)| {
                scope.allows(origin)
                    && (!origin.mesh || {
                        request.policy
                            == core.clipboard_receive_policy(&origin.device, request.kind)
                            && core.pair_allows(
                                &origin.device,
                                &origin.device,
                                &core.identity.device_id,
                                "clipboard",
                            )
                            && match &request.input {
                                ClipboardInput::Text(_) => true,
                                _ => core.incoming_kind_allowed(
                                    &request.store,
                                    &origin.device,
                                    request.kind.unwrap_or(pneu_proto::FileKind::ClipboardImage),
                                ),
                            }
                    })
            })
        })
    }
    fn effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            let Some(core) = self.core.upgrade() else {
                continue;
            };
            match effect {
                Effect::PairingLink(uri) => {
                    let mut ui = core.ui.lock().expect("UI state");
                    ui.snapshot.pairing_clipboard = uri;
                    ui.snapshot.pairing_clipboard_revision =
                        ui.snapshot.pairing_clipboard_revision.wrapping_add(1);
                }
                Effect::LoadShare { token, payload } => {
                    let handle = self.handle.clone();
                    let outbox = core.outbox.clone();
                    let job = self.spawn(
                        vec![Event::ShareReady {
                            token,
                            content: None,
                        }],
                        async move {
                            let content = outbox
                                .file_io(move || {
                                    Ok(match payload.as_ref() {
                                        Payload::Content(content) => content.clone(),
                                        #[cfg(target_os = "linux")]
                                        Payload::Input(ClipboardInput::Files(_)) => {
                                            anyhow::bail!(
                                                "file references are not clipboard sync content"
                                            )
                                        }
                                        Payload::Input(ClipboardInput::Text(text)) => {
                                            Content::Text(text.clone())
                                        }
                                        Payload::Input(ClipboardInput::Bytes { bytes, .. }) => {
                                            Content::Image(bytes.clone())
                                        }
                                        Payload::Input(ClipboardInput::Formats {
                                            text,
                                            png,
                                            ..
                                        }) => Content::Formats(text.clone(), png.clone()),
                                        Payload::Input(ClipboardInput::Image(file, mime)) => {
                                            let mut bytes =
                                                file.read_bounded(crate::ui::clipboard::MAX_IMAGE)?;
                                            if mime != "image/png" {
                                                let mut png = std::io::Cursor::new(Vec::new());
                                                crate::actions::decode_image(&bytes)?
                                                    .write_to(&mut png, image::ImageFormat::Png)?;
                                                bytes = png.into_inner();
                                            }
                                            anyhow::ensure!(
                                                bytes.len() <= crate::ui::clipboard::MAX_IMAGE,
                                                "clipboard image too large"
                                            );
                                            Content::Image(bytes)
                                        }
                                    })
                                })
                                .await
                                .ok();
                            handle.event(Event::ShareReady { token, content });
                        },
                    );
                    self.share_jobs.insert(token, job);
                }
                Effect::Clock { token, floor } => {
                    let operation = self
                        .state
                        .clocking
                        .as_ref()
                        .filter(|o| o.token == token)
                        .cloned();
                    let Some(operation) = operation else {
                        continue;
                    };
                    let source = if operation.received {
                        operation.source.clone()
                    } else {
                        core.identity.device_id.clone()
                    };
                    let store = core.store.clone();
                    let handle = self.handle.clone();
                    let job = self.spawn(
                        vec![Event::Clock {
                            token,
                            result: None,
                        }],
                        async move {
                            let report_core = core.clone();
                            let checked = operation.clone();
                            let result = store
                                .allocate_clipboard_clock(source, floor, move |hlc| {
                                    if checked.received
                                        || checked.observation.is_some()
                                        || checked
                                            .offer
                                            .as_ref()
                                            .is_none_or(|o| o.recipients.is_empty())
                                    {
                                        return Ok(());
                                    }
                                    core.clipboard_frames(&checked, hlc).map(|_| ())
                                })
                                .await;
                            if let Err(error) = &result {
                                report_core
                                    .error(None, format!("clipboard clock unavailable: {error}"));
                            }
                            handle.event(Event::Clock {
                                token,
                                result: result.ok(),
                            });
                        },
                    );
                    self.clock_jobs.insert(token, job);
                }
                Effect::RetryClock { token, delay_ms } => {
                    self.timers.retain(
                        |(_, event)| !matches!(event, Event::RetryClock(old) if *old == token),
                    );
                    self.timers.push((
                        tokio::time::Instant::now() + std::time::Duration::from_millis(delay_ms),
                        Event::RetryClock(token),
                    ));
                }
                Effect::ReadTimeout(token) => {
                    self.timers.push((
                        tokio::time::Instant::now() + std::time::Duration::from_secs(6),
                        Event::ReadTimeout(token),
                    ));
                }
                Effect::Debounce(token) => {
                    self.timers
                        .retain(|(_, event)| !matches!(event,Event::Debounce(old) if *old==token));
                    self.timers.push((
                        tokio::time::Instant::now() + std::time::Duration::from_millis(300),
                        Event::Debounce(token),
                    ));
                }
                Effect::CancelRead(token) => {
                    if let Some(cancel) = self.reads.remove(&token) {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    self.timers
                        .retain(|(_, event)| !matches!(event,Event::Debounce(old) if *old==token));
                }
                Effect::Prepare { ticket, operation } => {
                    if !self.authorized(&operation) {
                        self.send(Event::Cancel(operation.token));
                        continue;
                    }
                    let input = match operation.payload.as_deref() {
                        Some(Payload::Content(content)) => {
                            core.clipboard_input(Some(content.clone()))
                        }
                        Some(Payload::Input(input)) => Ok(input.clone()),
                        None => Err(anyhow!("missing register winner clipboard payload")),
                    };
                    let input = match input {
                        Ok(input) => input,
                        Err(_) => {
                            self.send(Event::Failed(ticket));
                            continue;
                        }
                    };
                    let actions = core.actions.clone();
                    let handle = self.handle.clone();
                    let authority = self.requests.get(&operation.token).and_then(|r| {
                        r.authority.clone().map(|(scope, origin)| Authority {
                            cancelled: scope.cancelled.subscribe(),
                            generation: scope.generation_changed.subscribe(),
                            settings: r.store.settings_changed.subscribe(),
                            config: core.config.subscribe(),
                            scope,
                            origin,
                            policy: r.policy.clone(),
                            kind: r.kind,
                            store: r.store.clone(),
                            input: r.input.clone(),
                            core: self.core.clone(),
                        })
                    });
                    let fingerprint_key = self.state.fingerprint_key.clone();
                    let token = operation.token;
                    let (stop, mut stopped) = watch::channel(false);
                    let (authorize, authorized) = oneshot::channel();
                    self.helpers.insert(
                        ticket,
                        Helper {
                            stop,
                            authorize: Some(authorize),
                        },
                    );
                    self.spawn(
                        vec![Event::Failed(ticket), Event::Stopped(ticket)],
                        async move {
                            #[cfg(windows)]
                            let install_authority = authority.clone();
                            let revoked = async move {
                                if let Some(mut authority) = authority {
                                    authority.revoked().await;
                                } else {
                                    std::future::pending::<()>().await;
                                }
                            };
                            tokio::pin!(revoked);
                            let result = tokio::select! {biased;
                                _=stopped.changed()=>None,
                                _=&mut revoked=>{ handle.event(Event::Revoked(token)); None },
                                result=actions.prepare_clipboard(input,ticket)=>Some(result),
                            };
                            let Some(result) = result else {
                                actions.drain_clipboard().await;
                                handle.event(Event::Stopped(ticket));
                                return;
                            };
                            let mut prepared = match result {
                                Ok(prepared) => prepared,
                                Err(error) => {
                                    eprintln!("pneud: clipboard preparation unavailable: {error}");
                                    handle.event(Event::Failed(ticket));
                                    handle.event(Event::Stopped(ticket));
                                    return;
                                }
                            };
                            let needs_marker = prepared.marker_confirmation;
                            handle.event(Event::Prepared {
                                ticket,
                                hashes: fingerprint_key.hashes(&prepared.hashes),
                            });
                            let commit = tokio::select! {biased;
                                _=stopped.changed()=>false,
                                _=&mut revoked=>{ handle.event(Event::Revoked(token)); false },
                                result=authorized=>result.is_ok(),
                            };
                            if commit {
                                #[cfg(windows)]
                                {
                                    let stopped = stopped.clone();
                                    prepared.set_install_authority(move || {
                                        !*stopped.borrow()
                                            && install_authority
                                                .as_ref()
                                                .is_none_or(Authority::valid)
                                    });
                                }
                                let result = tokio::select! {biased;
                                    _=stopped.changed()=>None,
                                    _=&mut revoked=>{ handle.event(Event::Revoked(token)); None },
                                    result=actions.install_clipboard(&mut prepared)=>Some(result),
                                };
                                match result {
                                    Some(Ok(())) => {
                                        if !needs_marker {
                                            handle.event(Event::Installed(ticket));
                                        }
                                        tokio::select! {biased;
                                            _=stopped.changed()=>{},
                                            _=&mut revoked=>{
                                                // The marker may already have completed the op.
                                                // Let the actor decide; confirmed receipts and the
                                                // serving selection survive policy changes.
                                                handle.event(Event::Revoked(token));
                                                tokio::select! {biased;
                                                    _=stopped.changed()=>{},
                                                    _=prepared.wait_exit()=>{},
                                                }
                                            },
                                            _=prepared.wait_exit()=>{},
                                        }
                                    }
                                    Some(Err(error)) => {
                                        eprintln!(
                                            "pneud: clipboard installation unavailable: {error}"
                                        );
                                        handle.event(Event::Failed(ticket));
                                    }
                                    None => {}
                                }
                            }
                            prepared.stop().await;
                            actions.drain_clipboard().await;
                            handle.event(Event::Stopped(ticket));
                        },
                    );
                    self.timers.push((
                        tokio::time::Instant::now() + std::time::Duration::from_secs(10),
                        Event::Timeout(ticket),
                    ));
                }
                Effect::SaveHistory { ticket, history } => {
                    let store = core.store.clone();
                    let handle = self.handle.clone();
                    self.history_revision += 1;
                    let job = store.persist_clipboard_history(self.history_revision, history);
                    self.spawn(
                        vec![Event::HistorySaved {
                            ticket,
                            success: false,
                        }],
                        async move {
                            let result = job.await;
                            handle.event(Event::HistorySaved {
                                ticket,
                                success: result.is_ok(),
                            });
                        },
                    );
                }
                Effect::Authorize(ticket) => {
                    let authorized = self
                        .state
                        .installing
                        .as_ref()
                        .filter(|i| i.ticket == ticket)
                        .is_some_and(|i| self.authorized(&i.operation));
                    if !authorized {
                        self.send(Event::Failed(ticket));
                        continue;
                    }
                    if let Some(authorize) = self
                        .helpers
                        .get_mut(&ticket)
                        .and_then(|h| h.authorize.take())
                    {
                        let _ = authorize.send(());
                    }
                }
                Effect::Stop(ticket) => {
                    if let Some(helper) = self.helpers.remove(&ticket) {
                        helper.stop.send_replace(true);
                    } else {
                        self.send(Event::Stopped(ticket));
                    }
                }
                Effect::Persist { operation, applied } => {
                    let store = self
                        .requests
                        .get(&operation.token)
                        .map(|r| r.store.clone())
                        .unwrap_or_else(|| core.store.clone());
                    let claim = self
                        .requests
                        .get_mut(&operation.token)
                        .and_then(|r| r.claim.take());
                    let handle = self.handle.clone();
                    self.history_revision += 1;
                    let history_job = store.persist_clipboard_history(
                        self.history_revision,
                        self.state.history.iter().cloned().collect(),
                    );
                    let register = self.state.register.clone();
                    self.spawn(
                        vec![Event::Persisted {
                            token: operation.token,
                            success: false,
                        }],
                        async move {
                            let result = async {
                                history_job.await?;
                                // Repair any earlier successful OS effect whose DB
                                // commit failed before accepting a later receipt.
                                if let Some(register) = register {
                                    store
                                        .persist_clipboard_register(register, operation.created_at)
                                        .await?;
                                }
                                if let Some(claim) = claim {
                                    claim
                                        .complete(
                                            operation.key.as_ref().map(|key| key.hlc),
                                            applied,
                                        )
                                        .await
                                } else {
                                    Ok(())
                                }
                            }
                            .await;
                            if let Err(error) = &result {
                                eprintln!("pneud: clipboard completion storage failed: {error}");
                            }
                            handle.event(Event::Persisted {
                                token: operation.token,
                                success: result.is_ok(),
                            });
                        },
                    );
                }
                Effect::Publish(operation) => {
                    let result = operation
                        .offer
                        .as_ref()
                        .ok_or_else(|| anyhow!("clipboard recipients changed"))
                        .and_then(|offer| {
                            let hlc = operation.key.as_ref().expect("stamp").hlc;
                            let (text, image) = core.clipboard_frames(&operation, hlc)?;
                            core.publish_automatic(offer.clone(), text, image)
                        });
                    if let Err(error) = &result {
                        core.error(None, error.to_string());
                    }
                    self.send(Event::Published {
                        token: operation.token,
                        accepted: result.is_ok(),
                    });
                }
                Effect::Reply {
                    token,
                    applied,
                    deduplicated,
                    failure,
                } => {
                    #[cfg(test)]
                    if let Some(reply) = self.waiters.remove(&token) {
                        let _ = reply.send(match failure {
                            None => Ok(Outcome {
                                deduplicated,
                                applied,
                            }),
                            Some(_) => Err(crate::store::ClipboardStorageFailure.into()),
                        });
                    }
                    if let Some(job) = self.share_jobs.remove(&token) {
                        job.abort();
                    }
                    if let Some(reply) = self.shares.remove(&token) {
                        let _ = reply.send(if failure.is_none() && applied {
                            Ok(Outcome {
                                applied,
                                deduplicated,
                            })
                        } else {
                            Err(anyhow!("clipboard cannot be shared"))
                        });
                    }
                    self.admissions.remove(&token);
                    self.timers.retain(
                        |(_, event)| !matches!(event, Event::Deadline(old) if *old == token),
                    );
                    if let Some(request) = self.requests.remove(&token) {
                        if let Some(claim) = request.claim {
                            self.spawn(Vec::new(), async move {
                                let _ = claim.discard().await;
                            });
                        }
                        let result = match failure {
                            None => Ok(Outcome {
                                deduplicated,
                                applied,
                            }),
                            Some(Failure::Storage) => {
                                Err(crate::store::ClipboardStorageFailure.into())
                            }
                            Some(Failure::Cancelled) => {
                                Err(anyhow!("clipboard operation cancelled"))
                            }
                            Some(Failure::Clipboard) => Err(anyhow!("clipboard unavailable")),
                        };
                        let _ = request.reply.send(result);
                    }
                }
                Effect::Invalidate(peer) => {
                    let mut delivery = core.automatic.lock().expect("clipboard delivery");
                    if let Some(peer) = peer {
                        delivery.invalidate_peer(&peer);
                    } else {
                        delivery.invalidate();
                    }
                }
            }
        }
    }
}
pub(in crate::core) async fn run(
    core: Weak<Core>,
    handle: Handle,
    mut rx: mailbox::Receiver,
    state: State,
) {
    let Some(owner) = core.upgrade() else {
        return;
    };
    let actions = owner.actions.clone();
    drop(owner);
    let mut driver = Driver {
        core,
        actions,
        state,
        handle,
        requests: HashMap::new(),
        shares: HashMap::new(),
        admissions: HashMap::new(),
        helpers: HashMap::new(),
        reads: HashMap::new(),
        jobs: tokio::task::JoinSet::new(),
        job_failures: HashMap::new(),
        stopping: None,
        barriers: Vec::new(),
        #[cfg(test)]
        waiters: HashMap::new(),
        timers: Vec::new(),
        clock_jobs: HashMap::new(),
        share_jobs: HashMap::new(),
        history_revision: 0,
    };
    loop {
        let due = driver
            .timers
            .iter()
            .map(|(at, _)| *at)
            .min()
            .unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(3600));
        tokio::select! {
            message=rx.recv()=>{
                let Some(message)=message else {break;};
                let event=match message {
                    Message::Event(event)=>event,
                    Message::Share(event,reply,permit)=>{
                        let Event::Share{token,..}=&event else {continue;};
                        driver.shares.insert(*token,reply);driver.admissions.insert(*token,permit);
                        driver.timers.push((tokio::time::Instant::now()+std::time::Duration::from_secs(10),Event::Deadline(*token)));event
                    },
                    Message::InstallMarker(ticket,reply)=>{
                        let current=driver.state.confirms_install(ticket);
                        let _=reply.send(current);
                        if current {Event::Installed(ticket)} else {continue;}
                    }
                    Message::ReadHandle(token,cancel)=>{driver.reads.insert(token,cancel);continue;}
                    Message::Submit(mut operation,request,permit)=>{
                        operation.payload=Some(Arc::new(Payload::Input(request.input.clone())));
                        driver.admissions.insert(operation.token,permit);
                        driver.timers.push((tokio::time::Instant::now() + std::time::Duration::from_secs(10), Event::Deadline(operation.token)));
                        driver.requests.insert(operation.token,*request);
                        Event::Submit(operation)
                    }
                    #[cfg(test)]
                    Message::Wait(token,reply)=>{driver.waiters.insert(token,reply);continue;}
                    #[cfg(test)]
                    Message::Barrier(reply)=>{driver.barriers.push(reply);if driver.state.idle() {for reply in driver.barriers.drain(..){let _=reply.send(());}}continue;}
                    Message::Stop(reply)=>{driver.handle.admission.close();driver.stopping=Some(reply);Event::Shutdown}
                };
                if let Event::Read{token,..}=&event {driver.reads.remove(token);}
                if let Event::Shutdown=&event {for (_,cancel) in driver.reads.drain(){cancel.store(true,Ordering::SeqCst);}}
                if let Event::Stopped(ticket)=&event {driver.helpers.remove(ticket);}
                driver.timers.retain(|(_,timer)|!matches!((&event,timer),(Event::Installed(ticket)|Event::Failed(ticket)|Event::Stopped(ticket),Event::Timeout(pending)) if ticket==pending));
                if let Some(core)=driver.core.upgrade() {
                    let config=core.config();let mode=if config.clipboard_sync_to_phone {config.clipboard_mode} else {ClipboardMode::Off};
                    let effects=driver.state.step(Event::Mode(mode));driver.effects(effects);
                    let allowed=core.automatic_target().map(|o|o.recipients).unwrap_or_default();
                    let effects=driver.state.step(Event::Policy {allowed});driver.effects(effects);
                }
                let effects=driver.state.step(event);driver.effects(effects);

            }
            result=driver.jobs.join_next_with_id(),if !driver.jobs.is_empty()=>{ if let Some(result) = result { driver.joined(result); } }
            _=tokio::time::sleep_until(due)=>{
                let now=tokio::time::Instant::now();
                let mut events=Vec::new();
                driver.timers.retain(|(at,event)| {if *at<=now {events.push(event.clone());false}else{true}});
                for event in events {let effects=driver.state.step(event);driver.effects(effects);}
            }
        }
        let current = driver.state.clocking.as_ref().map(|o| o.token);
        driver.clock_jobs.retain(|token, job| {
            let keep = Some(*token) == current;
            if !keep {
                job.abort();
            }
            keep
        });
        // Retired reads/retries must not leave timers accumulating across a
        // watcher flood. In-flight clocks are tracked separately above.
        driver.timers.retain(|(_, event)| match event {
            Event::RetryClock(token) => driver
                .state
                .clock_retry
                .as_ref()
                .is_some_and(|retry| retry.token == *token && retry.waiting),
            Event::Debounce(token) => driver
                .state
                .observation
                .iter()
                .chain(driver.state.held_observations.iter())
                .any(|o| o.token == *token),
            Event::ReadTimeout(token) => driver
                .state
                .observation
                .iter()
                .chain(driver.state.held_observations.iter())
                .any(|o| o.token == *token && o.read.is_none()),
            _ => true,
        });
        if driver.core.strong_count() == 0 {
            let store = driver.requests.values().next().map(|r| r.store.clone());
            driver.requests.clear();
            for (_, helper) in driver.helpers.drain() {
                helper.stop.send_replace(true);
            }
            for (_, cancel) in driver.reads.drain() {
                cancel.store(true, Ordering::SeqCst);
            }
            while driver.jobs.join_next().await.is_some() {}
            driver.actions.drain_clipboard().await;
            if let Some(store) = store {
                store.drain().await;
            }
            break;
        }
        if driver.state.idle() {
            for reply in driver.barriers.drain(..) {
                let _ = reply.send(());
            }
        }
        if driver.stopping.is_some()
            && driver.jobs.is_empty()
            && driver.state.persistence.is_empty()
            && driver.state.publishing.is_empty()
            && driver.state.stopping.is_none()
        {
            for (_, request) in driver.requests.drain() {
                let _ = request.reply.send(Err(anyhow!("clipboard actor stopped")));
            }
            if let Some(reply) = driver.stopping.take() {
                let _ = reply.send(());
            }
            break;
        }
    }
}

#[cfg(all(test, unix))]
#[path = "clipboard_driver_tests.rs"]
mod tests;

/// Authority is watched independently from the event loop. A blocked install
/// future is dropped on revocation before the helper is stopped and reaped.
#[cfg_attr(windows, derive(Clone))]
struct Authority {
    core: Weak<Core>,
    scope: ActionScope,
    origin: ActionOrigin,
    policy: Vec<pneu_proto::PeerSetting>,
    kind: Option<pneu_proto::FileKind>,
    input: ClipboardInput,
    store: Store,
    cancelled: watch::Receiver<bool>,
    generation: watch::Receiver<u64>,
    settings: watch::Receiver<u64>,
    config: watch::Receiver<crate::Config>,
}
impl Authority {
    fn valid(&self) -> bool {
        self.core.upgrade().is_some_and(|core| {
            self.scope.allows(&self.origin)
                && (!self.origin.mesh || {
                    self.policy == core.clipboard_receive_policy(&self.origin.device, self.kind)
                        && core.pair_allows(
                            &self.origin.device,
                            &self.origin.device,
                            &core.identity.device_id,
                            "clipboard",
                        )
                        && (matches!(self.input, ClipboardInput::Text(_))
                            || core.incoming_kind_allowed(
                                &self.store,
                                &self.origin.device,
                                self.kind.unwrap_or(pneu_proto::FileKind::ClipboardImage),
                            ))
                })
        })
    }
    async fn revoked(&mut self) {
        loop {
            if !self.valid() {
                return;
            }
            tokio::select! { biased;
                _=self.cancelled.changed()=>{},
                _=self.generation.changed()=>{},
                _=self.settings.changed()=>{},
                _=self.config.changed()=>{},
            }
        }
    }
}
