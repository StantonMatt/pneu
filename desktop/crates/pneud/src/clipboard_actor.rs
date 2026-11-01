//! Clipboard coordination. `State::step` is deliberately sans IO: slow jobs and
//! helper lifetimes belong to the driver, and every completion carries a token.
use super::{ClipboardOffer, ClipboardRecipient};
use crate::clipboard_fingerprint::FingerprintKey;
use crate::config::ClipboardMode;
use crate::store::history::Capture as HistoryCapture;
use crate::ui::clipboard::Content;
use pneu_proto::Id;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

pub(super) type Token = u64;
pub(super) type Ticket = u128;
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Key {
    pub hlc: u64,
    pub source: Id,
}
impl Ord for Key {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.hlc, self.source.as_str()).cmp(&(other.hlc, other.source.as_str()))
    }
}
impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Fingerprint {
    pub id: Id,
    pub key: Key,
    pub hashes: Vec<[u8; 32]>,
}
/// Exact payload of a register candidate. File-backed submissions retain their
/// anchored input; local observations retain the complete physical selection.
#[derive(Clone, Debug)]
pub(super) enum Payload {
    Content(Content),
    Input(crate::actions::ClipboardInput),
}
#[derive(Clone, Debug)]
pub(super) struct Operation {
    pub token: Token,
    pub sensitive: bool,
    pub history: Option<Arc<HistoryCapture>>,
    pub history_id: Option<String>,
    pub id: Id,
    pub source: Id,
    pub key: Option<Key>,
    pub created_at: u64,
    pub content: Option<Content>,
    // Full physical payload, before outbound provenance filtering. Repairs use
    // only this payload, never the potentially narrower outbound `content`.
    pub payload: Option<Arc<Payload>>,
    pub hashes: Vec<[u8; 32]>,
    pub offer: Option<ClipboardOffer>,
    pub received: bool,
    pub legacy: bool,
    pub restore: bool,
    pub repair: bool,
    pub observation: Option<Token>,
}
impl Operation {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            id: self.id.clone(),
            key: self.key.clone().expect("stamped operation"),
            hashes: self.hashes.clone(),
        }
    }
}
pub(super) fn hashes(content: &Content) -> Vec<[u8; 32]> {
    match content {
        Content::Text(text) => vec![Sha256::digest(text.as_bytes()).into()],
        Content::Image(bytes) => vec![Sha256::digest(bytes).into()],
        Content::Formats(text, bytes) => vec![
            Sha256::digest(text.as_bytes()).into(),
            Sha256::digest(bytes).into(),
        ],
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    Storage,
    Clipboard,
    Cancelled,
}
#[derive(Clone, Debug)]
pub(super) enum Event {
    Mode(ClipboardMode),
    Share {
        token: Token,
        id: Id,
        source: Id,
        offer: ClipboardOffer,
    },
    ShareReady {
        token: Token,
        content: Option<Content>,
    },
    Observe {
        token: Token,
        id: Id,
        offer: Option<ClipboardOffer>,
        origin: bool,
        clear: bool,
        history: Option<Arc<HistoryCapture>>,
    },
    Read {
        token: Token,
        content: Option<Content>,
        fingerprints: Vec<[u8; 32]>,
        debounce: bool,
    },
    Debounce(Token),
    ReadTimeout(Token),
    RetryClock(Token),
    Submit(Operation),
    Clock {
        token: Token,
        result: Option<Key>,
    },
    Prepared {
        ticket: Ticket,
        hashes: Vec<[u8; 32]>,
    },
    HistorySaved {
        ticket: Ticket,
        success: bool,
    },
    Installed(Ticket),
    Failed(Ticket),
    Timeout(Ticket),
    Stopped(Ticket),
    Persisted {
        token: Token,
        success: bool,
    },
    Published {
        token: Token,
        accepted: bool,
    },
    Cancel(Token),
    Deadline(Token),
    Revoked(Token),
    AbandonRead(Token),
    Revoke(Id),
    BindingReset(Id),
    Narrow(Id),
    Policy {
        allowed: Vec<ClipboardRecipient>,
    },
    #[cfg(all(test, target_os = "linux"))]
    TestEcho(Token),
    Shutdown,
}
#[derive(Clone, Debug)]
pub(super) enum Effect {
    RecordHistory {
        content: Content,
        capture: Arc<HistoryCapture>,
    },
    /// Local UI hint, independent from the ordinary sharing classification.
    PairingLink(Option<String>),
    LoadShare {
        token: Token,
        payload: Arc<Payload>,
    },
    Clock {
        token: Token,
        floor: Option<Key>,
    },
    Debounce(Token),
    ReadTimeout(Token),
    RetryClock {
        token: Token,
        delay_ms: u64,
    },
    CancelRead(Token),
    Prepare {
        ticket: Ticket,
        operation: Operation,
    },
    SaveHistory {
        ticket: Ticket,
        history: Vec<Fingerprint>,
    },
    Authorize(Ticket),
    Stop(Ticket),
    Persist {
        operation: Operation,
        applied: bool,
    },
    Publish(Operation),
    Reply {
        token: Token,
        applied: bool,
        deduplicated: bool,
        failure: Option<Failure>,
    },
    Invalidate(Option<Id>),
}
#[derive(Clone, Debug)]
struct Observation {
    history: Option<Arc<HistoryCapture>>,
    token: Token,
    id: Id,
    offer: Option<ClipboardOffer>,
    read: Option<(Option<Content>, Vec<[u8; 32]>)>,
    restore: bool,
    due: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminal {
    Installed,
    Superseded,
    Failed(Failure),
    Reaped,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Preparing,
    Clocking,
    Saving,
    Ready,
    Authorized,
}
#[derive(Clone, Debug)]
struct Installation {
    ticket: Ticket,
    operation: Operation,
    phase: Phase,
}
#[derive(Clone, Debug)]
struct Representation {
    hash: [u8; 32],
    received: Option<(Id, Id)>,
}
#[derive(Clone, Debug)]
struct Persistence {
    operation: Operation,
    deduplicated: bool,
    applied: bool,
}
#[derive(Clone, Debug)]
struct ClockRetry {
    token: Token,
    delay_ms: u64,
    waiting: bool,
}
/// There are no shared locks or local clock here. The Store is the only clock
/// allocator (also used by PEER_SETTINGS). `register` remains authoritative if
/// an OS effect succeeds and its later durable write fails.
#[derive(Default)]
pub(super) struct State {
    pub register: Option<Key>,
    mode: ClipboardMode,
    sharing: HashMap<Token, (Token, Option<Key>, Operation)>,
    // A manual request retains the exact OS selection and register it captured
    // until queued/clocked admission and publication have finished.
    manual: HashMap<Token, (Token, Option<Key>, u64)>,
    manual_epoch: u64,
    publishing: HashMap<Token, Persistence>,
    fingerprint_key: FingerprintKey,
    pub history: VecDeque<Fingerprint>,
    current_hashes: Vec<[u8; 32]>,
    physical: Vec<Representation>,
    os_current: bool,
    current: Option<Operation>,
    pinned: Option<(Id, Id)>,
    legacy: HashMap<Id, u64>,
    observation: Option<Observation>,
    held_observations: VecDeque<Observation>,
    os_token: Token,
    last_marker: Option<Ticket>,
    stopping_markers: VecDeque<Ticket>,
    queue: VecDeque<Operation>,
    clocking: Option<Operation>,
    installing: Option<Installation>,
    owner: Option<Ticket>,
    stopping: Option<Ticket>,
    persistence: HashMap<Token, Persistence>,
    local_completion: Option<Persistence>,
    next_ticket: Ticket,
    next_repair: Token,
    // Failed watcher stamps retain `clocking` until a timer retries them. This
    // replaces the old unknown latch, which had no autonomous recovery path.
    clock_retry: Option<ClockRetry>,
    shutdown: bool,
}
impl State {
    pub fn seed_tickets(&mut self, seed: Ticket) {
        self.next_ticket = seed;
    }
    pub fn load(
        register: Option<Key>,
        legacy: HashMap<Id, u64>,
        history: Vec<Fingerprint>,
        fingerprint_key: FingerprintKey,
    ) -> Self {
        let current = history.iter().find(|f| Some(&f.key) == register.as_ref());
        let current_hashes = current.map(|f| f.hashes.clone()).unwrap_or_default();
        let pinned = current.map(|f| (f.key.source.clone(), f.id.clone()));
        Self {
            register,
            fingerprint_key,
            legacy,
            physical: current_hashes
                .iter()
                .map(|hash| Representation {
                    hash: *hash,
                    received: pinned.clone(),
                })
                .collect(),
            current_hashes,
            pinned,
            history: history
                .into_iter()
                .rev()
                .take(64)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect(),
            ..Default::default()
        }
    }
    fn known_hash(&self, hash: &[u8; 32]) -> bool {
        self.physical
            .iter()
            .any(|r| r.hash == *hash && r.received.is_some())
            || (self
                .current
                .as_ref()
                .is_some_and(|o| o.received || o.restore)
                && self.current_hashes.contains(hash))
            || self.history.iter().any(|f| f.hashes.contains(hash))
    }
    fn hashes(&self, content: &Content) -> Vec<[u8; 32]> {
        self.fingerprint_key.hashes(&hashes(content))
    }
    fn payload_hashes(&self, operation: &Operation) -> Vec<[u8; 32]> {
        // Preparation can add aliases (e.g. JPEG plus converted PNG). Local
        // observations can also contain received formats omitted from outbound
        // content. Keep both, without manufacturing a second repair payload.
        let mut hashes = operation.hashes.clone();
        if let Some(Payload::Content(content)) = operation.payload.as_deref() {
            for hash in self.hashes(content) {
                if !hashes.contains(&hash) {
                    hashes.push(hash);
                }
            }
        }
        hashes
    }
    fn echo(&self, content: &Content) -> bool {
        self.hashes(content)
            .iter()
            .all(|hash| self.known_hash(hash))
    }
    fn local_formats(&self, content: Option<Content>) -> Option<Content> {
        let known = |bytes: &[u8]| {
            self.known_hash(&self.fingerprint_key.digest(&Sha256::digest(bytes).into()))
        };
        match content {
            Some(Content::Text(text)) => (!known(text.as_bytes())
                && text.len() <= pneu_proto::MAX_CLIPBOARD_BYTES)
                .then_some(Content::Text(text)),
            Some(Content::Image(image)) => (!known(&image)).then_some(Content::Image(image)),
            Some(Content::Formats(text, image)) => {
                match (
                    !known(text.as_bytes()) && text.len() <= pneu_proto::MAX_CLIPBOARD_BYTES,
                    !known(&image),
                ) {
                    (true, true) => Some(Content::Formats(text, image)),
                    (true, false) => Some(Content::Text(text)),
                    (false, true) => Some(Content::Image(image)),
                    _ => None,
                }
            }
            None => None,
        }
    }
    fn remember(&mut self, fingerprint: Fingerprint) {
        self.history
            .retain(|f| f.id != fingerprint.id || f.key.source != fingerprint.key.source);
        self.history.push_back(fingerprint);
        while self.history.len() > 64 {
            let index = self
                .history
                .iter()
                .position(|f| {
                    self.pinned
                        .as_ref()
                        .is_none_or(|(source, id)| &f.key.source != source || &f.id != id)
                        && !self.physical.iter().any(|r| {
                            r.received
                                .as_ref()
                                .is_some_and(|(source, id)| &f.key.source == source && &f.id == id)
                        })
                })
                .unwrap_or(0);
            self.history.remove(index);
        }
    }
    fn pending_count(&self) -> usize {
        self.queue.len()
            + self.sharing.len()
            + self.persistence.len()
            + self.publishing.len()
            + usize::from(self.local_completion.is_some())
            + usize::from(self.installing.is_some())
            + usize::from(self.clocking.as_ref().is_some_and(|o| {
                self.installing
                    .as_ref()
                    .is_none_or(|i| i.operation.token != o.token)
            }))
    }
    fn persist(&mut self, pending: Persistence, effects: &mut Vec<Effect>) {
        effects.push(Effect::Persist {
            operation: pending.operation.clone(),
            applied: pending.applied,
        });
        self.persistence.insert(pending.operation.token, pending);
    }
    fn finish(
        &mut self,
        operation: Operation,
        deduplicated: bool,
        applied: bool,
        effects: &mut Vec<Effect>,
    ) {
        let pending = Persistence {
            operation,
            deduplicated,
            applied,
        };
        if pending.operation.observation.is_some()
            && self
                .persistence
                .values()
                .any(|p| p.operation.observation.is_some())
        {
            // One active local write and one latest replacement. Never coalesce
            // admitted requests/receipts; their completion remains independent.
            if let Some(old) = self.local_completion.replace(pending) {
                effects.push(Effect::Reply {
                    token: old.operation.token,
                    applied: false,
                    deduplicated: false,
                    failure: Some(Failure::Cancelled),
                });
            }
        } else {
            self.persist(pending, effects);
        }
    }
    /// The single exit for every installation phase. Retire its clock token and
    /// retain only physical marker evidence; no late result can revive the op.
    fn terminate_install(&mut self, ticket: Ticket, outcome: Terminal, effects: &mut Vec<Effect>) {
        if !self.installing.as_ref().is_some_and(|i| {
            i.ticket == ticket && (outcome != Terminal::Installed || i.phase == Phase::Authorized)
        }) {
            return;
        }
        let install = self.installing.take().unwrap();
        if self
            .clocking
            .as_ref()
            .is_some_and(|o| o.token == install.operation.token)
        {
            self.clocking.take();
        }
        if outcome == Terminal::Installed {
            if let Some(old) = self.owner.replace(ticket)
                && old != ticket
            {
                effects.push(Effect::Stop(old));
            }
            self.register = install.operation.key.clone();
            if install.operation.legacy
                && matches!(install.operation.content, Some(Content::Text(_)))
            {
                self.legacy.insert(
                    install.operation.source.clone(),
                    install.operation.created_at,
                );
            }
            self.current = Some(install.operation.clone());
            if !install.operation.repair {
                self.current_hashes = self.payload_hashes(&install.operation);
            }
            if install.operation.received {
                self.physical = self
                    .current_hashes
                    .iter()
                    .map(|hash| Representation {
                        hash: *hash,
                        received: Some((
                            install.operation.source.clone(),
                            install.operation.id.clone(),
                        )),
                    })
                    .collect();
            } else {
                self.observe_physical(&self.current_hashes.clone());
            }
            self.os_current = true;
            self.pinned = install.operation.received.then(|| {
                (
                    install.operation.source.clone(),
                    install.operation.id.clone(),
                )
            });
            if let Some(observation) = &mut self.observation {
                observation.restore = true;
            }
            if !install.operation.repair {
                self.finish(install.operation, false, true, effects);
            }
            return;
        }
        if install.phase == Phase::Authorized {
            self.stopping_markers.push_back(ticket);
            while self.stopping_markers.len() > 64 {
                self.stopping_markers.pop_front();
            }
        }
        if outcome != Terminal::Reaped {
            self.stopping = Some(ticket);
            effects.push(Effect::Stop(ticket));
        }
        if !install.operation.repair {
            match outcome {
                Terminal::Superseded => self.finish(install.operation, true, false, effects),
                _ => effects.push(Effect::Reply {
                    token: install.operation.token,
                    applied: false,
                    deduplicated: false,
                    failure: Some(match outcome {
                        Terminal::Failed(failure) => failure,
                        _ => Failure::Clipboard,
                    }),
                }),
            }
        }
    }
    fn stop_install(&mut self, effects: &mut Vec<Effect>) {
        if let Some(ticket) = self.installing.as_ref().map(|i| i.ticket) {
            self.terminate_install(ticket, Terminal::Superseded, effects);
        }
    }
    fn observe_physical(&mut self, fingerprints: &[[u8; 32]]) {
        let physical = fingerprints
            .iter()
            .map(|hash| {
                let received = self
                    .physical
                    .iter()
                    .find(|r| &r.hash == hash)
                    .and_then(|r| r.received.clone())
                    .or_else(|| {
                        self.history
                            .iter()
                            .rev()
                            .find(|f| f.hashes.contains(hash))
                            .map(|f| (f.key.source.clone(), f.id.clone()))
                    });
                Representation {
                    hash: *hash,
                    received,
                }
            })
            .collect();
        self.physical = physical;
    }
    fn cancel_local_before(&mut self, token: Token, effects: &mut Vec<Effect>) {
        let mut kept = VecDeque::new();
        for operation in self.queue.drain(..) {
            if operation.repair {
                continue;
            }
            if !operation.received && operation.observation.unwrap_or(operation.token) < token {
                effects.push(Effect::Reply {
                    token: operation.token,
                    applied: false,
                    deduplicated: false,
                    failure: Some(Failure::Cancelled),
                });
            } else {
                kept.push_back(operation);
            }
        }
        self.queue = kept;
        if self
            .clocking
            .as_ref()
            .is_some_and(|o| !o.received && o.observation.unwrap_or(o.token) < token)
        {
            let old = self.clocking.take().unwrap();
            effects.push(Effect::Reply {
                token: old.token,
                applied: false,
                deduplicated: false,
                failure: Some(Failure::Cancelled),
            });
        }
    }
    fn invalidate_publications(&mut self, effects: &mut Vec<Effect>) {
        effects.push(Effect::Invalidate(None));
        for pending in self
            .persistence
            .values_mut()
            .chain(self.local_completion.iter_mut())
        {
            if let Some(offer) = &mut pending.operation.offer {
                offer.recipients.clear();
            }
        }
    }
    fn narrow_peer(&mut self, peer: &Id, effects: &mut Vec<Effect>) {
        for (_, _, op) in self.sharing.values_mut() {
            if let Some(offer) = &mut op.offer {
                offer.recipients.retain(|r| &r.target.device != peer);
            }
        }
        effects.push(Effect::Invalidate(Some(peer.clone())));
        for old in self
            .observation
            .iter_mut()
            .chain(self.held_observations.iter_mut())
        {
            if let Some(offer) = &mut old.offer {
                offer.recipients.retain(|r| &r.target.device != peer);
            }
        }
        for operation in self
            .queue
            .iter_mut()
            .chain(self.clocking.iter_mut())
            .chain(self.persistence.values_mut().map(|p| &mut p.operation))
            .chain(self.local_completion.iter_mut().map(|p| &mut p.operation))
        {
            if let Some(offer) = &mut operation.offer {
                offer.recipients.retain(|r| &r.target.device != peer);
            }
        }
    }
    fn revoke_peer(&mut self, peer: &Id, effects: &mut Vec<Effect>) {
        self.narrow_peer(peer, effects);
        if self
            .installing
            .as_ref()
            .is_some_and(|i| i.operation.received && &i.operation.source == peer)
        {
            self.stop_install(effects);
        }
        if self
            .clocking
            .as_ref()
            .is_some_and(|o| o.received && &o.source == peer)
        {
            let old = self.clocking.take().unwrap();
            effects.push(Effect::Reply {
                token: old.token,
                applied: false,
                deduplicated: false,
                failure: Some(Failure::Cancelled),
            });
        }
        let mut kept = VecDeque::new();
        for old in self.queue.drain(..) {
            if old.received && &old.source == peer {
                effects.push(Effect::Reply {
                    token: old.token,
                    applied: false,
                    deduplicated: false,
                    failure: Some(Failure::Cancelled),
                });
            } else {
                kept.push_back(old);
            }
        }
        self.queue = kept;
    }
    fn read_slot(&mut self, token: Token) -> Option<&mut Observation> {
        if self.observation.as_ref().is_some_and(|o| o.token == token) {
            return self.observation.as_mut();
        }
        self.held_observations.iter_mut().find(|o| o.token == token)
    }
    fn resolve_read(&mut self, token: Token, effects: &mut Vec<Effect>) {
        if self
            .read_slot(token)
            .is_none_or(|o| o.read.is_none() || !o.due)
        {
            return;
        }
        let observation = if self.observation.as_ref().is_some_and(|o| o.token == token) {
            self.observation.take().unwrap()
        } else {
            let index = self
                .held_observations
                .iter()
                .position(|o| o.token == token)
                .unwrap();
            self.held_observations.remove(index).unwrap()
        };
        let (original, fingerprints) = observation.read.unwrap();
        let mut fingerprints = self.fingerprint_key.hashes(&fingerprints);
        if fingerprints.is_empty()
            && let Some(content) = &original
        {
            fingerprints = self.hashes(content);
        }
        let content = self.local_formats(original.clone());
        self.observe_physical(&fingerprints);
        if content.is_none()
            && !fingerprints.is_empty()
            && fingerprints.iter().all(|hash| self.known_hash(hash))
        {
            if self
                .current
                .as_ref()
                .is_none_or(|o| o.payload.is_none() || o.key != self.register)
                && token == self.os_token
                && let Some(original) = original
                && let Some(fingerprint) = self
                    .history
                    .iter()
                    .rev()
                    .find(|f| fingerprints.iter().any(|hash| f.hashes.contains(hash)))
                    .cloned()
            {
                // Restart restores provenance, not payloads. Retain the actual
                // observed echo for manual sharing without a clock, receipt or
                // publication. Its historical key must stay intact: an older
                // echo is not a repair payload for the register winner.
                self.current = Some(Operation {
                    sensitive: false,
                    history: None,
                    history_id: None,
                    token,
                    id: fingerprint.id,
                    source: fingerprint.key.source.clone(),
                    created_at: fingerprint.key.hlc,
                    key: Some(fingerprint.key),
                    content: Some(original.clone()),
                    payload: Some(Arc::new(Payload::Content(original))),
                    hashes: fingerprints.clone(),
                    offer: None,
                    received: true,
                    legacy: false,
                    restore: false,
                    repair: false,
                    observation: None,
                });
                self.current_hashes = fingerprints.clone();
            }
            self.os_current = fingerprints
                .iter()
                .any(|hash| self.current_hashes.contains(hash));
            if !self.os_current
                && let Some(mut current) = self.current.clone()
                && current.payload.is_some()
            {
                self.next_repair += 1;
                current.token = (1u64 << 63) | self.next_repair;
                current.restore = true;
                current.repair = true;
                current.offer = None;
                current.observation = None;
                self.queue.push_front(current);
            }
            effects.push(Effect::Reply {
                token,
                applied: false,
                deduplicated: true,
                failure: None,
            });
            return;
        }
        // A genuinely newer local selection cancels older reads and pending
        // originations. An echo never cancels them, including after our marker.
        let mut kept = VecDeque::new();
        for old in self.held_observations.drain(..) {
            if old.token < token {
                effects.push(Effect::CancelRead(old.token));
            } else {
                kept.push_back(old);
            }
        }
        self.held_observations = kept;
        self.cancel_local_before(token, effects);
        self.invalidate_publications(effects);
        let hashes = content.as_ref().map(|c| self.hashes(c)).unwrap_or_default();
        let operation = Operation {
            sensitive: false,
            history: None,
            history_id: None,
            token,
            id: observation.id,
            source: self
                .register
                .as_ref()
                .map(|k| k.source.clone())
                .unwrap_or_else(|| Id::from_bytes([0; 16])),
            key: None,
            created_at: 0,
            hashes,
            restore: content.is_some() && (observation.restore || token != self.os_token),
            content,
            payload: original.map(Payload::Content).map(Arc::new),
            offer: if self.mode == ClipboardMode::Automatic {
                observation.offer.filter(|o| !o.recipients.is_empty())
            } else {
                None
            },
            received: false,
            legacy: false,
            repair: false,
            observation: Some(token),
        };
        self.stop_install(effects);
        self.queue.push_front(operation);
    }
    fn share_current(&self, operation: &Operation) -> bool {
        self.manual
            .get(&operation.token)
            .is_none_or(|(physical, key, epoch)| {
                self.mode == ClipboardMode::Manual
                    && *epoch == self.manual_epoch
                    && *physical == self.os_token
                    && self.os_current
                    && self.observation.is_none()
                    && self.held_observations.is_empty()
                    && (key == &self.register
                        || (operation.key.is_some() && operation.key == self.register))
                    && operation
                        .offer
                        .as_ref()
                        .is_some_and(|o| !o.recipients.is_empty())
            })
    }
    fn exclude_received_representations(&self, content: &Content, offer: &mut ClipboardOffer) {
        let (text, image) = match content {
            Content::Text(text) => (Some(self.hashes(&Content::Text(text.clone()))[0]), None),
            Content::Image(bytes) => (None, Some(self.hashes(&Content::Image(bytes.clone()))[0])),
            Content::Formats(text, bytes) => (
                Some(self.hashes(&Content::Text(text.clone()))[0]),
                Some(self.hashes(&Content::Image(bytes.clone()))[0]),
            ),
        };
        for recipient in &mut offer.recipients {
            let received = |hash: Option<[u8; 32]>| {
                hash.is_some_and(|h| {
                    self.physical.iter().any(|r| {
                        r.hash == h
                            && r.received
                                .as_ref()
                                .is_some_and(|(peer, _)| peer == &recipient.target.device)
                    })
                })
            };
            recipient.text &= text.is_some() && !received(text);
            recipient.image &= image.is_some() && !received(image);
        }
        offer.recipients.retain(|r| r.text || r.image);
    }
    fn pump(&mut self, effects: &mut Vec<Effect>) {
        if self.shutdown
            || self.observation.is_some()
            || !self.held_observations.is_empty()
            || self.clocking.is_some()
            || self.stopping.is_some()
        {
            return;
        }
        if let Some(install) = self.installing.as_ref() {
            if install.phase == Phase::Clocking {
                let operation = install.operation.clone();
                effects.push(Effect::Clock {
                    token: operation.token,
                    floor: self.register.clone(),
                });
                self.clocking = Some(operation);
            }
            if install.phase == Phase::Ready {
                let ticket = install.ticket;
                self.installing.as_mut().unwrap().phase = Phase::Authorized;
                effects.push(Effect::Authorize(ticket));
            }
            return;
        }
        while let Some(mut operation) = self.queue.pop_front() {
            if !self.share_current(&operation) {
                effects.push(Effect::Reply {
                    token: operation.token,
                    applied: false,
                    deduplicated: false,
                    failure: Some(Failure::Cancelled),
                });
                continue;
            }
            if operation.repair && (operation.key != self.register || operation.payload.is_none()) {
                continue;
            }
            if !operation.received
                && operation
                    .observation
                    .is_some_and(|token| token != self.os_token)
            {
                operation.restore = operation.content.is_some();
            }
            // A repair reasserts the register winner, rather than admitting a
            // new remote operation. Its own legacy timestamp must remain valid.
            if operation.legacy
                && !operation.repair
                && matches!(operation.content, Some(Content::Text(_)))
                && self
                    .legacy
                    .get(&operation.source)
                    .is_some_and(|old| operation.created_at <= *old)
            {
                self.finish(operation, true, false, effects);
                continue;
            }
            if operation.key.is_none() && !operation.legacy {
                effects.push(Effect::Clock {
                    token: operation.token,
                    floor: self.register.clone(),
                });
                self.clocking = Some(operation);
                return;
            }
            let key = operation.key.clone();
            if !operation.repair
                && key
                    .as_ref()
                    .zip(self.register.as_ref())
                    .is_some_and(|(key, current)| key <= current)
            {
                self.finish(operation, true, false, effects);
                continue;
            }
            let equal = self.os_current
                && !operation.hashes.is_empty()
                && operation
                    .hashes
                    .iter()
                    .any(|h| self.current_hashes.contains(h));
            if operation.legacy && operation.key.is_none() && equal {
                effects.push(Effect::Clock {
                    token: operation.token,
                    floor: self.register.clone(),
                });
                self.clocking = Some(operation);
                return;
            }
            if (equal || !operation.received)
                && !operation.sensitive
                && !operation.repair
                && operation.history_id.is_none()
            {
                // No OS write means the previous offer still serves every one
                // of its representations. Carry them into the new provenance,
                // including formats absent from the incoming operation.
                if equal {
                    for hash in &self.current_hashes {
                        if !operation.hashes.contains(hash) {
                            operation.hashes.push(*hash);
                        }
                    }
                }
                if equal && (operation.received || operation.observation.is_none()) {
                    operation.payload = self.current.as_ref().and_then(|o| o.payload.clone());
                }
                self.register = key;
                if operation.legacy && matches!(operation.content, Some(Content::Text(_))) {
                    self.legacy
                        .insert(operation.source.clone(), operation.created_at);
                }
                self.current = Some(operation.clone());
                if !equal || (!operation.received && operation.observation.is_some()) {
                    self.current_hashes = self.payload_hashes(&operation);
                }
                self.os_current = operation.received
                    || (!operation.restore && (operation.observation.is_some() || equal));
                if operation.received {
                    self.physical = self
                        .current_hashes
                        .iter()
                        .map(|hash| Representation {
                            hash: *hash,
                            received: Some((operation.source.clone(), operation.id.clone())),
                        })
                        .collect();
                    self.pinned = Some((operation.source.clone(), operation.id.clone()));
                    self.remember(operation.fingerprint());
                } else if !equal {
                    // An explicit identical share also leaves the received OS
                    // offer in place; keep its physical provenance pinned.
                    self.pinned = None;
                }
                let repair = (!operation.received
                    && operation.restore
                    && operation.payload.is_some())
                .then(|| {
                    let mut repair = operation.clone();
                    self.next_repair += 1;
                    repair.token = (1u64 << 63) | self.next_repair;
                    repair.repair = true;
                    repair.offer = None;
                    repair.observation = None;
                    repair
                });
                self.finish(operation, equal, true, effects);
                if let Some(repair) = repair {
                    self.queue.push_front(repair);
                }
                continue;
            }
            let Some(ticket) = self.next_ticket.checked_add(1) else {
                effects.push(Effect::Reply {
                    token: operation.token,
                    applied: false,
                    deduplicated: false,
                    failure: Some(Failure::Clipboard),
                });
                continue;
            };
            self.next_ticket = ticket;
            // Pending fingerprints are visible before helper authorization.
            if operation.received && operation.key.is_some() {
                self.remember(operation.fingerprint());
            }
            effects.push(Effect::Prepare {
                ticket,
                operation: operation.clone(),
            });
            self.installing = Some(Installation {
                ticket,
                operation,
                phase: Phase::Preparing,
            });
            return;
        }
    }
    /// Only the current authorized install can be confirmed by a watcher MIME.
    /// Retired/unknown tickets are clipboard observations, just like unmarked
    /// manager re-offers; their bytes still need provenance classification.
    pub fn confirms_install(&self, ticket: Ticket) -> bool {
        self.installing
            .as_ref()
            .is_some_and(|i| i.ticket == ticket && i.phase == Phase::Authorized)
    }
    pub fn idle(&self) -> bool {
        self.observation.is_none()
            && self.sharing.is_empty()
            && self.held_observations.is_empty()
            && self.clocking.is_none()
            && self.queue.is_empty()
            && self.installing.is_none()
            && self.stopping.is_none()
            && self.persistence.is_empty()
            && self.publishing.is_empty()
            && self.local_completion.is_none()
    }
    pub fn step(&mut self, event: Event) -> Vec<Effect> {
        let effects = self.step_inner(event);
        for effect in &effects {
            if let Effect::Reply { token, .. } = effect {
                self.manual.remove(token);
            }
        }
        effects
    }
    fn step_inner(&mut self, event: Event) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.shutdown
            && matches!(
                event,
                Event::Observe { .. }
                    | Event::Read { .. }
                    | Event::Debounce(_)
                    | Event::AbandonRead(_)
                    | Event::ReadTimeout(_)
                    | Event::RetryClock(_)
            )
        {
            if let Event::Observe { token, .. } = event {
                effects.push(Effect::CancelRead(token));
            }
            return effects;
        }
        match event {
            Event::Mode(mode) => {
                if self.mode != mode {
                    self.mode = mode;
                    self.invalidate_publications(&mut effects);
                    for old in self
                        .observation
                        .iter_mut()
                        .chain(self.held_observations.iter_mut())
                    {
                        if let Some(offer) = &mut old.offer {
                            offer.recipients.clear();
                        }
                    }
                    for old in self
                        .queue
                        .iter_mut()
                        .chain(self.clocking.iter_mut())
                        .chain(self.persistence.values_mut().map(|p| &mut p.operation))
                        .chain(self.local_completion.iter_mut().map(|p| &mut p.operation))
                    {
                        if !old.received {
                            old.offer = None;
                        }
                    }
                    for (token, _) in self.sharing.drain() {
                        effects.push(Effect::Reply {
                            token,
                            applied: false,
                            deduplicated: false,
                            failure: Some(Failure::Cancelled),
                        });
                    }
                }
            }
            Event::Share {
                token,
                id,
                source,
                offer,
            } => {
                let payload = self.current.as_ref().and_then(|o| o.payload.clone());
                if self.shutdown
                    || self.mode != ClipboardMode::Manual
                    || !self.os_current
                    || self.observation.is_some()
                    || !self.held_observations.is_empty()
                    || offer.recipients.is_empty()
                    || payload.is_none()
                    || self.current.as_ref().is_some_and(|o| o.sensitive)
                    || self.pending_count() >= 61
                {
                    effects.push(Effect::Reply {
                        token,
                        applied: false,
                        deduplicated: false,
                        failure: Some(Failure::Clipboard),
                    });
                } else if let Some(payload) = payload {
                    let operation = Operation {
                        sensitive: false,
                        history: None,
                        history_id: None,
                        token,
                        id,
                        source,
                        key: None,
                        created_at: 0,
                        content: None,
                        payload: Some(payload.clone()),
                        hashes: Vec::new(),
                        offer: Some(offer),
                        received: false,
                        legacy: false,
                        restore: false,
                        repair: false,
                        observation: None,
                    };
                    self.manual.insert(
                        token,
                        (self.os_token, self.register.clone(), self.manual_epoch),
                    );
                    self.sharing
                        .insert(token, (self.os_token, self.register.clone(), operation));
                    effects.push(Effect::LoadShare { token, payload });
                }
            }
            Event::ShareReady { token, content } => {
                if let Some((physical, key, mut operation)) = self.sharing.remove(&token) {
                    if self.mode == ClipboardMode::Manual
                        && physical == self.os_token
                        && key == self.register
                        && self.observation.is_none()
                        && self.held_observations.is_empty()
                        && self.os_current
                        && let Some(content) = content
                    {
                        let fingerprints = self.hashes(&content);
                        // File-backed payloads may have been edited since the
                        // OS installation. Only captured representation bytes
                        // (including normalized image aliases) can be shared.
                        if fingerprints
                            .iter()
                            .any(|h| !self.current_hashes.contains(h))
                        {
                            effects.push(Effect::Reply {
                                token,
                                applied: false,
                                deduplicated: false,
                                failure: Some(Failure::Clipboard),
                            });
                        } else {
                            if let Some(offer) = &mut operation.offer {
                                self.exclude_received_representations(&content, offer);
                            }
                            if operation
                                .offer
                                .as_ref()
                                .is_none_or(|o| o.recipients.is_empty())
                            {
                                effects.push(Effect::Reply {
                                    token,
                                    applied: false,
                                    deduplicated: false,
                                    failure: Some(Failure::Cancelled),
                                });
                            } else {
                                operation.hashes = hashes(&content);
                                operation.content = Some(content);
                                effects.extend(self.step(Event::Submit(operation)));
                            }
                        }
                    } else {
                        effects.push(Effect::Reply {
                            token,
                            applied: false,
                            deduplicated: false,
                            failure: Some(Failure::Clipboard),
                        });
                    }
                }
            }
            Event::Observe {
                token,
                id,
                offer,
                origin,
                clear,
                history,
            } => {
                effects.push(Effect::PairingLink(None));
                if clear
                    && self
                        .last_marker
                        .is_some_and(|ticket| self.stopping_markers.contains(&ticket))
                {
                    let ticket = self.last_marker.take().unwrap();
                    self.stopping_markers.retain(|old| *old != ticket);
                    self.os_current = false;
                } else if !origin {
                    // Stop unsent delivery immediately, including a frame whose
                    // first socket write is still pending. Classification may
                    // preserve an older human read across a manager echo, but
                    // must never keep already-prepared wire content authorized.
                    self.invalidate_publications(&mut effects);
                    self.last_marker = None;
                    if let Some(old) = self.observation.take() {
                        if !self.history.is_empty()
                            || self.current.as_ref().is_some_and(|o| o.restore)
                        {
                            // Eight outstanding offers bound descriptors/content.
                            // Keep earlier reads across unknown offers: the new
                            // owner may be a manager re-offering received bytes.
                            if self.held_observations.len() == 7
                                && let Some(old) = self.held_observations.pop_front()
                            {
                                effects.push(Effect::CancelRead(old.token));
                            }
                            self.held_observations.push_back(old);
                        } else {
                            effects.push(Effect::CancelRead(old.token));
                        }
                    }
                    self.os_current = false;
                    self.os_token = token;
                    let restore = self
                        .installing
                        .as_ref()
                        .is_some_and(|i| i.phase == Phase::Authorized)
                        || !self.stopping_markers.is_empty();
                    self.observation = Some(Observation {
                        history,
                        token,
                        id,
                        offer: if self.mode == ClipboardMode::Automatic {
                            offer
                        } else {
                            None
                        },
                        read: None,
                        restore,
                        due: false,
                    });
                    effects.push(Effect::Debounce(token));
                    effects.push(Effect::ReadTimeout(token));
                }
            }
            Event::Read {
                token,
                content,
                fingerprints,
                debounce,
            } => {
                if token == self.os_token
                    && self.observation.as_ref().is_some_and(|o| o.token == token)
                {
                    let text = match &content {
                        Some(Content::Text(text) | Content::Formats(text, _)) => Some(text.trim()),
                        _ => None,
                    };
                    effects.push(Effect::PairingLink(
                        text.filter(|text| text.starts_with("pneu://pair") && text.len() <= 16384)
                            .map(str::to_owned),
                    ));
                }
                // Apply the same received-fingerprint exclusion as resolve_read,
                // while keeping local history capture independent of debounce.
                let echo = if fingerprints.is_empty() {
                    content.as_ref().is_some_and(|content| self.echo(content))
                } else {
                    self.fingerprint_key
                        .hashes(&fingerprints)
                        .iter()
                        .all(|hash| self.known_hash(hash))
                };
                let history = self.read_slot(token).and_then(|o| o.history.take());
                if !echo && let (Some(capture), Some(content)) = (history, content.as_ref()) {
                    effects.push(Effect::RecordHistory {
                        capture,
                        content: content.clone(),
                    });
                }
                if let Some(observation) = self.read_slot(token) {
                    observation.read = Some((content, fingerprints));
                    if !debounce {
                        observation.due = true;
                    }
                    self.resolve_read(token, &mut effects);
                }
            }
            Event::Debounce(token) => {
                if let Some(observation) = self.read_slot(token) {
                    observation.due = true;
                }
                self.resolve_read(token, &mut effects);
            }
            Event::ReadTimeout(token) => {
                if let Some(observation) = self.read_slot(token)
                    && observation.read.is_none()
                {
                    // The reader normally completes within five seconds. A
                    // lost completion must still release the ordering barrier:
                    // stamp the unread selection as an opaque local change.
                    observation.read = Some((None, Vec::new()));
                    observation.due = true;
                    effects.push(Effect::CancelRead(token));
                    self.resolve_read(token, &mut effects);
                }
            }
            Event::RetryClock(token) => {
                if let Some(retry) = &mut self.clock_retry
                    && retry.token == token
                    && retry.waiting
                    && self.clocking.as_ref().is_some_and(|o| o.token == token)
                {
                    retry.waiting = false;
                    effects.push(Effect::Clock {
                        token,
                        floor: self.register.clone(),
                    });
                }
            }
            Event::Submit(mut operation) => {
                if !operation.received && !operation.repair && operation.history_id.is_none() {
                    if self.mode != ClipboardMode::Automatic
                        && (self.mode == ClipboardMode::Off || operation.observation.is_some())
                    {
                        effects.push(Effect::Reply {
                            token: operation.token,
                            applied: false,
                            deduplicated: false,
                            failure: Some(Failure::Cancelled),
                        });
                        return effects;
                    }
                    if operation.observation.is_some() && self.mode != ClipboardMode::Automatic {
                        operation.offer = None;
                    }
                }
                if operation.payload.is_none() {
                    operation.payload = operation
                        .content
                        .clone()
                        .map(Payload::Content)
                        .map(Arc::new);
                }
                operation.hashes = self.fingerprint_key.hashes(&operation.hashes);
                // Three reserved slots cover a local observation clock, the
                // active local persistence and its latest deferred replacement.
                if self.shutdown || self.pending_count() >= 61 {
                    effects.push(Effect::Reply {
                        token: operation.token,
                        applied: false,
                        deduplicated: false,
                        failure: Some(Failure::Storage),
                    });
                    return effects;
                }
                if let Some(token) = operation.observation {
                    if self.observation.as_ref().is_none_or(|o| o.token != token) {
                        effects.push(Effect::Reply {
                            applied: false,
                            token: operation.token,
                            deduplicated: false,
                            failure: Some(Failure::Cancelled),
                        });
                        return effects;
                    }
                    let observation = self.observation.take().unwrap();
                    if operation
                        .content
                        .as_ref()
                        .is_some_and(|content| self.echo(content))
                    {
                        effects.push(Effect::Reply {
                            applied: false,
                            token: operation.token,
                            deduplicated: true,
                            failure: None,
                        });
                        self.pump(&mut effects);
                        return effects;
                    }
                    operation.restore = observation.restore;
                    operation.offer = if self.mode == ClipboardMode::Automatic {
                        observation.offer
                    } else {
                        None
                    };
                    if operation
                        .offer
                        .as_ref()
                        .is_some_and(|o| o.recipients.is_empty())
                    {
                        effects.push(Effect::Reply {
                            applied: false,
                            token: operation.token,
                            deduplicated: false,
                            failure: Some(Failure::Cancelled),
                        });
                        self.pump(&mut effects);
                        return effects;
                    }
                    self.cancel_local_before(token, &mut effects);
                    self.invalidate_publications(&mut effects);
                    self.stop_install(&mut effects);
                }

                if operation.received
                    && !operation.repair
                    && operation
                        .key
                        .as_ref()
                        .zip(self.register.as_ref())
                        .is_none_or(|(new, current)| new > current)
                {
                    // A remote winner admitted after a manual click supersedes
                    // queued clicks before they can allocate a newer local HLC.
                    self.manual_epoch = self.manual_epoch.wrapping_add(1);
                }
                if !operation.received {
                    // Explicit shares narrow an unresolved physical offer. They
                    // never add later connections to its frozen recipient set.
                    if let Some(offer) = &operation.offer {
                        for recipient in &offer.recipients {
                            effects.push(Effect::Invalidate(Some(recipient.target.device.clone())));
                            for old in self
                                .queue
                                .iter_mut()
                                .chain(self.clocking.iter_mut())
                                .chain(self.persistence.values_mut().map(|p| &mut p.operation))
                                .chain(self.local_completion.iter_mut().map(|p| &mut p.operation))
                            {
                                if let Some(offer) = &mut old.offer {
                                    offer
                                        .recipients
                                        .retain(|r| r.target.device != recipient.target.device);
                                }
                            }
                            for old in self
                                .observation
                                .iter_mut()
                                .chain(self.held_observations.iter_mut())
                            {
                                if let Some(offer) = &mut old.offer {
                                    offer
                                        .recipients
                                        .retain(|r| r.target.device != recipient.target.device);
                                }
                            }
                        }
                    }
                    if self.observation.as_ref().is_some_and(|o| {
                        o.offer
                            .as_ref()
                            .is_none_or(|offer| offer.recipients.is_empty())
                    }) && let Some(old) = self.observation.take()
                    {
                        effects.push(Effect::CancelRead(old.token));
                    }
                    self.held_observations.retain(|old| {
                        let keep = old
                            .offer
                            .as_ref()
                            .is_some_and(|offer| !offer.recipients.is_empty());
                        if !keep {
                            effects.push(Effect::CancelRead(old.token));
                        }
                        keep
                    });
                    if self.installing.is_some() {
                        operation.restore = true;
                    }
                } else if let Some(install) = &self.installing
                    && operation
                        .key
                        .as_ref()
                        .zip(install.operation.key.as_ref())
                        .is_some_and(|(new, old)| new > old)
                {
                    self.stop_install(&mut effects);
                }
                if !operation.received
                    && operation.key.is_none()
                    && self.clocking.is_none()
                    && self.observation.is_none()
                    && self.held_observations.is_empty()
                {
                    effects.push(Effect::Clock {
                        token: operation.token,
                        floor: self.register.clone(),
                    });
                    self.clocking = Some(operation);
                } else {
                    self.queue.push_back(operation);
                }
            }
            Event::Clock { token, result } => {
                if self.clocking.as_ref().is_some_and(|o| o.token == token) {
                    let mut operation = self.clocking.take().unwrap();
                    if !self.share_current(&operation) {
                        effects.push(Effect::Reply {
                            token,
                            applied: false,
                            deduplicated: false,
                            failure: Some(Failure::Cancelled),
                        });
                    } else if let Some(key) = result {
                        operation.created_at = if operation.legacy {
                            operation.created_at
                        } else {
                            key.hlc
                        };
                        operation.source = key.source.clone();
                        operation.key = Some(key);
                        self.clock_retry = None;
                        if self.installing.as_ref().is_some_and(|i| {
                            i.operation.token == token && i.phase == Phase::Clocking
                        }) {
                            let install = self.installing.as_mut().unwrap();
                            install.operation = operation;
                            install.phase = Phase::Saving;
                            let ticket = install.ticket;
                            let fingerprint = install.operation.fingerprint();
                            self.remember(fingerprint);
                            effects.push(Effect::SaveHistory {
                                ticket,
                                history: self.history.iter().cloned().collect(),
                            });
                        } else {
                            if !operation.received
                                && operation.observation.is_none()
                                && self.installing.is_some()
                            {
                                operation.restore = true;
                                self.stop_install(&mut effects);
                            }
                            self.queue.push_front(operation);
                        }
                    } else if operation.observation.is_some() && !self.shutdown {
                        let delay_ms = self
                            .clock_retry
                            .as_ref()
                            .filter(|retry| retry.token == token)
                            .map_or(100, |retry| (retry.delay_ms * 2).min(2000));
                        self.clock_retry = Some(ClockRetry {
                            token,
                            delay_ms,
                            waiting: true,
                        });
                        self.clocking = Some(operation);
                        effects.push(Effect::RetryClock { token, delay_ms });
                    } else if let Some(ticket) = self
                        .installing
                        .as_ref()
                        .filter(|i| i.operation.token == token)
                        .map(|i| i.ticket)
                    {
                        self.terminate_install(
                            ticket,
                            Terminal::Failed(Failure::Storage),
                            &mut effects,
                        );
                    } else {
                        effects.push(Effect::Reply {
                            applied: false,
                            token,
                            deduplicated: false,
                            failure: Some(Failure::Storage),
                        });
                    }
                }
            }
            Event::Prepared { ticket, hashes } => {
                if self.installing.as_ref().is_some_and(|i| {
                    i.ticket == ticket
                        && i.phase == Phase::Preparing
                        && i.operation.repair
                        && (hashes.is_empty()
                            || hashes
                                .iter()
                                .any(|hash| !self.current_hashes.contains(hash)))
                }) {
                    self.terminate_install(
                        ticket,
                        Terminal::Failed(Failure::Clipboard),
                        &mut effects,
                    );
                }
                if let Some(install) = self
                    .installing
                    .as_mut()
                    .filter(|i| i.ticket == ticket && i.phase == Phase::Preparing)
                {
                    for hash in hashes {
                        if !install.operation.hashes.contains(&hash) {
                            install.operation.hashes.push(hash);
                        }
                    }
                    if install.operation.legacy && install.operation.key.is_none() {
                        install.phase = Phase::Clocking;
                        // `pump` submits this request once the one outstanding
                        // Store allocation has returned; never replace it.
                    } else {
                        install.phase = Phase::Saving;
                        if install.operation.received {
                            let fingerprint = install.operation.fingerprint();
                            self.remember(fingerprint);
                        }
                        effects.push(Effect::SaveHistory {
                            ticket,
                            history: self.history.iter().cloned().collect(),
                        });
                    }
                }
            }
            Event::HistorySaved { ticket, success } => {
                if let Some(install) = self
                    .installing
                    .as_mut()
                    .filter(|i| i.ticket == ticket && i.phase == Phase::Saving)
                {
                    if success {
                        install.phase = Phase::Ready;
                    } else {
                        self.terminate_install(
                            ticket,
                            Terminal::Failed(Failure::Storage),
                            &mut effects,
                        );
                    }
                }
            }
            Event::Installed(ticket) => {
                if self
                    .installing
                    .as_ref()
                    .is_some_and(|i| i.ticket == ticket && i.operation.received)
                {
                    effects.push(Effect::PairingLink(None));
                }
                if self.installing.as_ref().is_some_and(|i| i.ticket == ticket)
                    || self.owner == Some(ticket)
                    || self.stopping_markers.contains(&ticket)
                {
                    self.last_marker = Some(ticket);
                    self.stopping_markers.retain(|old| *old == ticket);
                }
                self.terminate_install(ticket, Terminal::Installed, &mut effects);
            }
            Event::Failed(ticket) | Event::Timeout(ticket) => {
                self.terminate_install(ticket, Terminal::Failed(Failure::Clipboard), &mut effects);
            }
            Event::Stopped(ticket) => {
                self.terminate_install(ticket, Terminal::Reaped, &mut effects);
                if self.stopping == Some(ticket) {
                    self.stopping = None;
                }
                if self.owner == Some(ticket) {
                    self.owner = None;
                }
            }
            Event::Persisted { token, success } => {
                if let Some(pending) = self.persistence.remove(&token) {
                    let explicit = !pending.operation.received
                        && pending.operation.history_id.is_none()
                        && !pending.operation.repair
                        && pending.operation.observation.is_none();
                    let publish = success
                        && pending.applied
                        && (self.mode == ClipboardMode::Automatic
                            || (self.mode == ClipboardMode::Manual
                                && pending.operation.observation.is_none()))
                        && !pending.operation.received
                        && !pending.operation.repair
                        && pending.operation.content.is_some()
                        && pending
                            .operation
                            .offer
                            .as_ref()
                            .is_some_and(|o| !o.recipients.is_empty())
                        && self.share_current(&pending.operation);
                    if publish {
                        effects.push(Effect::Publish(pending.operation.clone()));
                        if explicit {
                            self.publishing.insert(token, pending);
                        } else {
                            effects.push(Effect::Reply {
                                token,
                                applied: pending.applied,
                                deduplicated: pending.deduplicated,
                                failure: None,
                            });
                        }
                    } else {
                        effects.push(Effect::Reply {
                            token,
                            applied: pending.applied && !explicit,
                            deduplicated: pending.deduplicated,
                            failure: if !success {
                                Some(Failure::Storage)
                            } else if explicit {
                                Some(Failure::Cancelled)
                            } else {
                                None
                            },
                        });
                    }
                }
            }
            Event::Published { token, accepted } => {
                if let Some(pending) = self.publishing.remove(&token) {
                    effects.push(Effect::Reply {
                        token,
                        applied: accepted,
                        deduplicated: pending.deduplicated,
                        failure: (!accepted).then_some(Failure::Cancelled),
                    });
                }
            }
            Event::Revoked(token) => {
                if let Some(ticket) = self
                    .installing
                    .as_ref()
                    .filter(|i| i.operation.token == token)
                    .map(|i| i.ticket)
                {
                    self.terminate_install(
                        ticket,
                        Terminal::Failed(Failure::Cancelled),
                        &mut effects,
                    );
                }
            }
            Event::AbandonRead(token) => {
                if self.observation.as_ref().is_some_and(|o| o.token == token) {
                    self.observation.take();
                }
                self.held_observations.retain(|o| o.token != token);
                effects.push(Effect::CancelRead(token));
            }
            event @ (Event::Cancel(token) | Event::Deadline(token)) => {
                let deadline = matches!(event, Event::Deadline(_));
                if self.sharing.remove(&token).is_some() {
                    effects.push(Effect::Reply {
                        token,
                        applied: false,
                        deduplicated: false,
                        failure: Some(Failure::Cancelled),
                    });
                }
                let failure = if deadline {
                    Failure::Clipboard
                } else {
                    Failure::Cancelled
                };
                if self.persistence.contains_key(&token)
                    || self.publishing.contains_key(&token)
                    || (!deadline
                        && self.installing.as_ref().is_some_and(|i| {
                            i.operation.token == token && i.phase == Phase::Authorized
                        }))
                {
                    return effects;
                }
                if !self
                    .queue
                    .iter()
                    .chain(self.clocking.iter())
                    .chain(self.installing.iter().map(|i| &i.operation))
                    .any(|o| o.token == token)
                {
                    return effects;
                }
                // Once installed, cancellation of a caller never drops the
                // durable completion. A deadline also stops an authorized helper
                // whose OS effect has not been confirmed.
                if self
                    .installing
                    .as_ref()
                    .is_some_and(|i| i.operation.token == token)
                {
                    let ticket = self.installing.as_ref().unwrap().ticket;
                    self.terminate_install(ticket, Terminal::Failed(failure), &mut effects);
                    self.pump(&mut effects);
                    return effects;
                }
                self.queue.retain(|o| o.token != token);
                if self.clocking.as_ref().is_some_and(|o| o.token == token) {
                    self.clocking.take();
                }
                effects.push(Effect::Reply {
                    applied: false,
                    token,
                    deduplicated: false,
                    failure: Some(failure),
                });
            }
            Event::Narrow(peer) => self.narrow_peer(&peer, &mut effects),
            Event::BindingReset(peer) => {
                self.legacy.remove(&peer);
                self.revoke_peer(&peer, &mut effects);
            }
            Event::Revoke(peer) => self.revoke_peer(&peer, &mut effects),
            Event::Policy { allowed } => {
                for (_, _, op) in self.sharing.values_mut() {
                    if let Some(offer) = &mut op.offer {
                        offer
                            .recipients
                            .retain(|old| allowed.iter().any(|new| old.target == new.target));
                    }
                }
                let narrow = |offer: &mut ClipboardOffer| {
                    offer
                        .recipients
                        .retain(|old| allowed.iter().any(|new| old.target == new.target))
                };
                for old in self
                    .observation
                    .iter_mut()
                    .chain(self.held_observations.iter_mut())
                {
                    if let Some(offer) = &mut old.offer {
                        narrow(offer);
                    }
                }
                for operation in self
                    .queue
                    .iter_mut()
                    .chain(self.clocking.iter_mut())
                    .chain(self.persistence.values_mut().map(|p| &mut p.operation))
                    .chain(self.local_completion.iter_mut().map(|p| &mut p.operation))
                {
                    if let Some(offer) = &mut operation.offer {
                        narrow(offer);
                    }
                }
            }
            #[cfg(all(test, target_os = "linux"))]
            Event::TestEcho(token) => {
                if self.observation.as_ref().is_some_and(|o| o.token == token) {
                    self.observation.take();
                }
            }
            Event::Shutdown => {
                for (token, _) in self.sharing.drain() {
                    effects.push(Effect::Reply {
                        token,
                        applied: false,
                        deduplicated: false,
                        failure: Some(Failure::Cancelled),
                    });
                }
                self.shutdown = true;
                for old in self
                    .observation
                    .take()
                    .into_iter()
                    .chain(self.held_observations.drain(..))
                {
                    effects.push(Effect::CancelRead(old.token));
                }
                self.stop_install(&mut effects);
                for operation in self.queue.drain(..).chain(self.clocking.take()) {
                    effects.push(Effect::Reply {
                        applied: false,
                        token: operation.token,
                        deduplicated: false,
                        failure: Some(Failure::Cancelled),
                    });
                }
                self.stop_install(&mut effects);
                if let Some(owner) = self.owner.take() {
                    effects.push(Effect::Stop(owner));
                    self.stopping = Some(owner);
                }
            }
        }
        if self.clock_retry.as_ref().is_some_and(|retry| {
            self.clocking
                .as_ref()
                .is_none_or(|o| o.token != retry.token)
        }) {
            self.clock_retry = None;
        }
        if !self
            .persistence
            .values()
            .any(|p| p.operation.observation.is_some())
            && let Some(pending) = self.local_completion.take()
        {
            self.persist(pending, &mut effects);
        }
        self.pump(&mut effects);
        for effect in &effects {
            if let Effect::Reply { token, .. } = effect {
                self.manual.remove(token);
            }
        }
        effects
    }
}

#[path = "clipboard_driver.rs"]
pub(super) mod driver;
#[cfg(test)]
#[path = "clipboard_actor_tests.rs"]
mod tests;
