use super::*;
use crate::core::{ClipboardOffer, ClipboardTarget};
use std::collections::{BTreeSet, HashSet, VecDeque};
fn id(n: u8) -> Id {
    Id::from_bytes([n; 16])
}
fn content(text: &str) -> Content {
    Content::Text(text.into())
}
fn offer(token: u64, peers: &[u8]) -> ClipboardOffer {
    ClipboardOffer {
        revision: token,
        observed: true,
        recipients: peers
            .iter()
            .map(|n| ClipboardRecipient {
                target: ClipboardTarget {
                    device: id(*n),
                    connection: 1,
                    binding: 1,
                    policy: 0,
                    config_epoch: 0,
                },
                text: true,
                image: true,
            })
            .collect(),
    }
}
fn remote(token: u64, source: u8, hlc: u64, value: &str) -> Operation {
    let content = content(value);
    Operation {
        sensitive: false,
        history: None,
        history_id: None,
        token,
        id: id(token as u8),
        source: id(source),
        key: Some(Key {
            hlc,
            source: id(source),
        }),
        created_at: hlc,
        payload: None,
        hashes: hashes(&content),
        content: Some(content),
        offer: None,
        received: true,
        legacy: false,
        restore: false,
        repair: false,
        observation: None,
    }
}
#[derive(Default)]
struct Compositor {
    state: State,
    os: Option<Content>,
    installed: HashMap<u128, Operation>,
    pending: VecDeque<Event>,
    published: Vec<Operation>,
    replies: HashMap<u64, (bool, Option<Failure>)>,
    winners: HashMap<Key, Option<Content>>,
    clock: u64,
    live: HashSet<u128>,
    fail_persist: bool,
    fail_clock: bool,
    retained_claims: HashSet<Token>,
    read_timeouts: BTreeSet<Token>,
    stopped: HashSet<u128>,
}
impl Compositor {
    fn event(&mut self, event: Event) {
        if let Event::Read { token, .. } | Event::ReadTimeout(token) = &event {
            self.read_timeouts.remove(token);
        }
        if let Event::Submit(operation) = &event
            && let Some(key) = &operation.key
        {
            self.clock = self.clock.max(key.hlc);
            self.winners.insert(key.clone(), operation.content.clone());
        }
        let effects = self.state.step(event);
        self.effects(effects);
    }
    fn effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::RecordHistory { .. } => {}
                Effect::PairingLink(_) => {}
                Effect::LoadShare { token, payload } => self.pending.push_back(Event::ShareReady {
                    token,
                    content: match payload.as_ref() {
                        Payload::Content(content) => Some(content.clone()),
                        _ => None,
                    },
                }),
                Effect::Clock { token, floor } => {
                    if std::mem::take(&mut self.fail_clock) {
                        self.pending.push_back(Event::Clock {
                            token,
                            result: None,
                        });
                        continue;
                    }
                    // Clock completions use independent capacity, even with all
                    // sixteen general Store admissions retained by receipts.
                    assert!(self.retained_claims.len() <= crate::work::BLOCKING_LIMIT);
                    self.clock = self.clock.max(floor.map_or(0, |f| f.hlc)) + 1;
                    let source = self
                        .state
                        .clocking
                        .as_ref()
                        .map(|o| if o.received { o.source.clone() } else { id(1) })
                        .unwrap();
                    let key = Key {
                        hlc: self.clock,
                        source,
                    };
                    let operation = self.state.clocking.as_ref().unwrap();
                    let value = match operation.payload.as_deref() {
                        Some(Payload::Content(content)) => Some(content.clone()),
                        _ => operation.content.clone(),
                    };
                    self.winners.insert(key.clone(), value);
                    self.pending.push_back(Event::Clock {
                        token,
                        result: Some(key),
                    });
                }
                Effect::Prepare { ticket, operation } => {
                    assert!(
                        self.live
                            .iter()
                            .all(|ticket| Some(*ticket) == self.state.owner),
                        "overlapping unresolved installations"
                    );
                    assert!(!self.stopped.contains(&ticket));
                    self.live.insert(ticket);
                    self.installed.insert(ticket, operation.clone());
                    self.pending.push_back(Event::Prepared {
                        ticket,
                        hashes: match operation.payload.as_deref() {
                            Some(Payload::Content(content)) => self.state.hashes(content),
                            _ => operation.hashes,
                        },
                    });
                }
                Effect::SaveHistory { ticket, history } => {
                    assert!(history.len() <= 64);
                    self.pending.push_back(Event::HistorySaved {
                        ticket,
                        success: true,
                    });
                }
                Effect::Authorize(ticket) => {
                    assert!(self.live.contains(&ticket));
                    self.pending.push_back(Event::Installed(ticket));
                }
                Effect::Stop(ticket) => {
                    self.live.remove(&ticket);
                    self.stopped.insert(ticket);
                    self.pending
                        .retain(|e| !matches!(e,Event::Installed(t) if *t==ticket));
                    self.pending.push_back(Event::Stopped(ticket));
                }
                Effect::Persist { operation, .. } => {
                    self.pending.push_back(Event::Persisted {
                        token: operation.token,
                        success: !self.fail_persist,
                    });
                }
                Effect::Publish(operation) => {
                    assert!(!operation.received, "forwarding received operation");
                    assert!(
                        self.state.mode == ClipboardMode::Automatic
                            || (self.state.mode == ClipboardMode::Manual
                                && operation.observation.is_none()),
                        "mode allowed a prohibited publication"
                    );
                    self.pending.push_back(Event::Published {
                        token: operation.token,
                        accepted: true,
                    });
                    self.published.push(operation);
                }
                Effect::Reply {
                    token,
                    deduplicated,
                    failure,
                    ..
                } => {
                    self.retained_claims.remove(&token);
                    assert!(
                        self.replies
                            .insert(token, (deduplicated, failure))
                            .is_none(),
                        "double reply for {token}"
                    );
                }
                Effect::Debounce(token) => self.pending.push_back(Event::Debounce(token)),
                Effect::ReadTimeout(token) => {
                    self.read_timeouts.insert(token);
                }
                Effect::RetryClock { token, .. } => {
                    self.pending.push_back(Event::RetryClock(token))
                }
                Effect::CancelRead(token) => {
                    self.read_timeouts.remove(&token);
                }
                Effect::Invalidate(_) => {}
            }
        }
    }
    fn tick(&mut self) -> bool {
        let Some(event) = self.pending.pop_front() else {
            return false;
        };
        if let Event::Installed(ticket) = event {
            if self.live.contains(&ticket) {
                self.os = match self.installed[&ticket].payload.as_deref() {
                    Some(Payload::Content(content)) => Some(content.clone()),
                    Some(Payload::Input(_)) => panic!("model requires an in-memory payload"),
                    None => None,
                };
                self.event(Event::Installed(ticket));
            }
        } else {
            self.event(event);
        }
        true
    }
    fn settle(&mut self) {
        for _ in 0..1000 {
            if !self.tick() {
                return;
            }
        }
        panic!("model did not quiesce");
    }
    fn copy(&mut self, token: u64, value: Option<Content>, recipients: &[u8]) {
        self.os = value.clone();
        self.event(Event::Observe {
            history: None,
            token,
            id: id(token as u8),
            offer: Some(offer(token, recipients)),
            origin: false,
            clear: false,
        });
        self.event(Event::Read {
            token,
            content: value,
            fingerprints: Vec::new(),
            debounce: false,
        });
    }
    fn consistent(&self) {
        if let Some(key) = &self.state.register
            && let Some(Some(expected)) = self.winners.get(key)
        {
            assert_eq!(
                self.os.as_ref(),
                Some(expected),
                "wrong OS winner at {key:?}"
            );
        }
    }
}
#[test]
fn local_copies_at_every_prepare_authorize_and_marker_boundary_win_once_quiescent() {
    for boundary in 0..7 {
        let mut model = Compositor::default();
        model.event(Event::Submit(remote(10, 2, 100, "X")));
        for _ in 0..boundary {
            model.tick();
        }
        model.copy(20, Some(content("Y")), &[2, 3]);
        model.settle();
        model.consistent();
        assert_eq!(model.os, Some(content("Y")), "boundary {boundary}");
        assert_eq!(model.published.len(), 1, "boundary {boundary}");
        assert_eq!(model.published[0].content, Some(content("Y")));
    }
}
#[test]
fn manager_takeover_before_and_after_ack_never_originates_text_or_image() {
    for before_ack in [true, false] {
        for image in [true, false] {
            let mut model = Compositor::default();
            let mut operation = remote(10, 2, 100, "X");
            if image {
                operation.content = Some(Content::Image(b"\x89PNG\r\n\x1a\nX".to_vec()));
                operation.hashes = hashes(operation.content.as_ref().unwrap());
            }
            let received = operation.content.clone();
            model.event(Event::Submit(operation));
            while model
                .pending
                .front()
                .is_some_and(|e| !matches!(e, Event::Installed(_)))
            {
                model.tick();
            }
            if before_ack {
                // The compositor has accepted the helper, then another client
                // re-offers the same bytes without the origin/marker MIME.
                model.os = received.clone();
                model.copy(20, received.clone(), &[2, 3]);
            } else {
                model.settle();
                model.copy(20, received.clone(), &[2, 3]);
            }
            model.settle();
            assert!(model.published.is_empty());
            assert_eq!(model.os, received);
        }
    }
}
#[test]
fn delayed_local_read_before_marker_is_restored_after_marker() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "X")));
    while model
        .pending
        .front()
        .is_some_and(|e| !matches!(e, Event::Installed(_)))
    {
        model.tick();
    }
    model.os = Some(content("Y"));
    model.event(Event::Observe {
        history: None,
        token: 20,
        id: id(20),
        offer: Some(offer(20, &[2, 3])),
        origin: false,
        clear: false,
    });
    model.tick();
    assert_eq!(model.os, Some(content("X")));
    model.event(Event::Read {
        token: 20,
        content: Some(content("Y")),
        fingerprints: Vec::new(),
        debounce: false,
    });
    model.settle();
    model.consistent();
    assert_eq!(model.os, Some(content("Y")));
}
#[test]
fn stale_read_clock_prepare_marker_and_timeout_tokens_have_no_effect() {
    let mut model = Compositor::default();
    model.copy(20, Some(content("new")), &[2]);
    model.settle();
    let register = model.state.register.clone();
    let hashes = model.state.current_hashes.clone();
    let published = model.published.len();
    for event in [
        Event::Read {
            token: 19,
            content: Some(content("stale")),
            fingerprints: Vec::new(),
            debounce: false,
        },
        Event::Clock {
            token: 19,
            result: Some(Key {
                hlc: 999,
                source: id(2),
            }),
        },
        Event::Prepared {
            ticket: 999,
            hashes: vec![[1; 32]],
        },
        Event::Installed(999),
        Event::Stopped(999),
        Event::Timeout(999),
    ] {
        model.event(event);
        model.settle();
        assert_eq!(model.state.register, register);
        assert_eq!(model.state.current_hashes, hashes);
        assert_eq!(model.published.len(), published);
    }
}
#[test]
fn explicit_mesh_share_stops_unresolved_install_before_accepting_new_origination() {
    for boundary in 0..3 {
        let mut model = Compositor::default();
        model.event(Event::Submit(remote(10, 2, 100, "old")));
        for _ in 0..boundary {
            model.tick();
        }
        let mut share = remote(20, 1, 0, "explicit");
        share.received = false;
        share.key = None;
        share.offer = Some(offer(20, &[2]));
        model.event(Event::Submit(share));
        model.settle();
        model.consistent();
        assert_eq!(model.os, Some(content("explicit")));
        assert_eq!(model.published.len(), 1);
        assert_eq!(
            model.published[0].offer.as_ref().unwrap().recipients.len(),
            1
        );
    }
}
#[test]
fn recipient_sets_only_narrow_across_settings_unpair_and_late_connections() {
    let mut model = Compositor::default();
    model.event(Event::Observe {
        history: None,
        token: 20,
        id: id(20),
        offer: Some(offer(20, &[2, 3])),
        origin: false,
        clear: false,
    });
    model.event(Event::Policy {
        allowed: offer(21, &[2, 4]).recipients,
    });
    model.event(Event::Revoke(id(3)));
    model.os = Some(content("local"));
    model.event(Event::Read {
        token: 20,
        content: model.os.clone(),
        fingerprints: Vec::new(),
        debounce: false,
    });
    model.settle();
    assert_eq!(
        model.published[0]
            .offer
            .as_ref()
            .unwrap()
            .recipients
            .iter()
            .map(|r| r.target.device.clone())
            .collect::<Vec<_>>(),
        vec![id(2)]
    );
}
#[test]
fn storage_failure_after_os_success_retains_memory_ordering_and_echo_evidence() {
    let mut model = Compositor {
        fail_persist: true,
        ..Default::default()
    };
    model.event(Event::Submit(remote(10, 2, 100, "X")));
    model.settle();
    assert_eq!(model.state.register.as_ref().unwrap().hlc, 100);
    model.fail_persist = false;
    model.copy(20, Some(content("X")), &[2, 3]);
    model.settle();
    assert!(model.published.is_empty());
    model.event(Event::Submit(remote(30, 3, 99, "older")));
    model.settle();
    assert_eq!(model.os, Some(content("X")));
    assert_eq!(model.replies[&10].1, Some(Failure::Storage));
}
#[test]
fn persisted_history_is_bounded_pins_current_and_survives_helper_loss_and_restart() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "X")));
    model.settle();
    let register = model.state.register.clone();
    let current = model.state.pinned.clone();
    for n in 0..100 {
        model.state.remember(Fingerprint {
            id: id(n),
            key: Key {
                hlc: n as u64,
                source: id(3),
            },
            hashes: vec![[n; 32]],
        });
    }
    assert_eq!(model.state.history.len(), 64);
    assert!(model.state.history.iter().any(|f| {
        current
            .as_ref()
            .is_some_and(|(source, id)| &f.key.source == source && &f.id == id)
    }));
    let mut restarted = State::load(
        register,
        HashMap::new(),
        model.state.history.into(),
        model.state.fingerprint_key,
    );
    assert!(restarted.echo(&content("X")));
    let effects = restarted.step(Event::Stopped(1));
    assert!(effects.is_empty());
    assert!(restarted.echo(&content("X")));
}
#[test]
fn three_computers_do_not_manufacture_an_origination_or_forward_received_x_over_y() {
    let mut computers = [
        Compositor::default(),
        Compositor::default(),
        Compositor::default(),
    ];
    computers[0].copy(20, Some(content("X")), &[2, 3]);
    computers[0].settle();
    let x = computers[0].published[0].clone();
    for computer in &mut computers[1..] {
        let mut received = x.clone();
        received.hashes = received.content.as_ref().map(hashes).unwrap_or_default();
        received.payload = None;
        received.received = true;
        received.offer = None;
        received.observation = None;
        computer.event(Event::Submit(received));
        computer.settle();
        computer.copy(30, Some(content("X")), &[1, 2, 3]);
        computer.settle();
        assert!(computer.published.is_empty());
    }
    computers[1].copy(40, Some(content("Y")), &[1, 3]);
    computers[1].settle();
    let y = computers[1].published[0].clone();
    for n in [0, 2] {
        let mut received = y.clone();
        received.hashes = received.content.as_ref().map(hashes).unwrap_or_default();
        received.payload = None;
        received.received = true;
        received.offer = None;
        received.observation = None;
        computers[n].event(Event::Submit(received));
        computers[n].settle();
        if n == 2 {
            computers[n].copy(50, Some(content("X")), &[1, 2, 3]);
            computers[n].settle();
            computers[n].consistent();
        }
        assert_eq!(computers[n].state.register, y.key);
        assert_eq!(computers[n].published.len(), usize::from(n == 0));
    }
}
#[test]
fn filtered_and_targetless_local_observations_cancel_slow_preparation_and_fence_its_late_results() {
    for readable in [false, true] {
        let mut model = Compositor::default();
        model.event(Event::Submit(remote(10, 2, 100, "remote")));
        model.os = readable.then(|| content("local"));
        model.event(Event::Observe {
            history: None,
            token: 20,
            id: id(20),
            offer: None,
            origin: false,
            clear: false,
        });
        model.event(Event::Read {
            token: 20,
            content: model.os.clone(),
            fingerprints: Vec::new(),
            debounce: false,
        });
        model.settle();
        assert!(model.published.is_empty());
        assert!(model.state.register.as_ref().unwrap().hlc > 100);
        let winner = model.state.register.clone();
        model.event(Event::Prepared {
            ticket: 1,
            hashes: vec![[0; 32]],
        });
        model.event(Event::Installed(1));
        model.settle();
        assert_eq!(model.state.register, winner);
        if readable {
            model.consistent();
        }
    }
}
#[test]
fn cancellation_during_preparation_does_not_leave_an_installable_candidate() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "remote")));
    model.event(Event::Cancel(10));
    model.settle();
    assert!(model.state.register.is_none());
    assert!(model.live.is_empty());
    assert_eq!(model.replies[&10].1, Some(Failure::Cancelled));
    let count = model.replies.len();
    model.event(Event::Cancel(10));
    model.event(Event::Installed(1));
    model.settle();
    assert_eq!(model.replies.len(), count);
}
#[test]
fn explicit_clock_and_legacy_preparation_share_one_outstanding_store_request() {
    let mut model = Compositor::default();
    let mut legacy = remote(10, 2, 0, "legacy");
    legacy.legacy = true;
    legacy.key = None;
    model.event(Event::Submit(legacy));
    let mut explicit = remote(20, 1, 0, "explicit");
    explicit.received = false;
    explicit.key = None;
    explicit.offer = Some(offer(20, &[2]));
    model.event(Event::Submit(explicit));
    model.settle();
    model.consistent();
    assert!(model.replies.contains_key(&20));
    assert_eq!(model.published.len(), 1);
    assert_eq!(model.os, Some(content("explicit")));
}
#[test]
fn identical_incoming_cannot_skip_an_unfinished_local_restoration() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "X")));
    while model
        .pending
        .front()
        .is_some_and(|e| !matches!(e, Event::Installed(_)))
    {
        model.tick();
    }
    model.event(Event::Observe {
        history: None,
        token: 20,
        id: id(20),
        offer: Some(offer(20, &[2, 3])),
        origin: false,
        clear: false,
    });
    model.os = Some(content("Y"));
    model.tick(); // older authorized helper installed X after the local copy
    model.event(Event::Read {
        token: 20,
        content: Some(content("Y")),
        fingerprints: Vec::new(),
        debounce: false,
    });
    while model
        .state
        .register
        .as_ref()
        .is_none_or(|key| key.hlc <= 100)
    {
        assert!(model.tick());
    }
    model.event(Event::Submit(remote(30, 3, 200, "Y")));
    model.settle();
    model.consistent();
    assert_eq!(model.os, Some(content("Y")));
}
#[test]
fn a_manager_echo_cannot_cancel_a_local_read_before_or_after_the_marker() {
    for before_marker in [true, false] {
        let mut model = Compositor::default();
        model.event(Event::Submit(remote(10, 2, 100, "received")));
        while model
            .pending
            .front()
            .is_some_and(|event| !matches!(event, Event::Installed(_)))
        {
            model.tick();
        }
        if !before_marker {
            model.settle();
        }
        model.os = Some(content("human"));
        model.event(Event::Observe {
            history: None,
            token: 20,
            id: id(20),
            offer: Some(offer(20, &[2, 3])),
            origin: false,
            clear: false,
        });
        if before_marker {
            model.tick();
        }
        model.os = Some(content("received"));
        model.event(Event::Observe {
            history: None,
            token: 30,
            id: id(30),
            offer: Some(offer(30, &[2, 3])),
            origin: false,
            clear: false,
        });
        model.event(Event::Read {
            token: 30,
            content: Some(content("received")),
            fingerprints: Vec::new(),
            debounce: false,
        });
        model.event(Event::Read {
            token: 20,
            content: Some(content("human")),
            fingerprints: Vec::new(),
            debounce: false,
        });
        model.settle();
        model.consistent();
        assert_eq!(model.os, Some(content("human")));
        assert_eq!(model.published.len(), 1);
    }
}
#[test]
fn a_canceled_helper_marker_and_its_clear_do_not_steal_a_local_copy() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "received")));
    while model
        .pending
        .front()
        .is_some_and(|event| !matches!(event, Event::Installed(_)))
    {
        model.tick();
    }
    model.copy(20, Some(content("human")), &[2, 3]);
    model.event(Event::Installed(1)); // ordered watcher evidence queued before reaping
    model.event(Event::Observe {
        history: None,
        token: 30,
        id: id(30),
        offer: Some(offer(30, &[2, 3])),
        origin: false,
        clear: true,
    });
    model.event(Event::Read {
        token: 30,
        content: None,
        fingerprints: Vec::new(),
        debounce: false,
    });
    model.settle();
    model.consistent();
    assert_eq!(model.os, Some(content("human")));
    assert_eq!(model.published.len(), 1);
}
#[test]
fn unshareable_representation_hashes_suppress_received_original_and_converted_echoes() {
    let mut model = Compositor::default();
    let mut received = remote(10, 2, 100, "text");
    let original = [3; 32];
    let converted = [4; 32];
    received.hashes = vec![original, converted];
    model.event(Event::Submit(received));
    model.settle();
    let before = model.state.register.clone();
    for (token, hash) in [(20, original), (30, converted)] {
        model.event(Event::Observe {
            history: None,
            token,
            id: id(token as u8),
            offer: None,
            origin: false,
            clear: false,
        });
        model.event(Event::Read {
            token,
            content: None,
            fingerprints: vec![hash],
            debounce: false,
        });
        model.settle();
        assert_eq!(model.state.register, before);
        assert!(model.published.is_empty());
    }
}

#[test]
fn new_offer_revokes_unflushed_delivery_and_a_late_persist_cannot_republish_it() {
    let mut model = Compositor::default();
    model.copy(10, Some(content("old local")), &[2, 3]);
    while model.state.persistence.is_empty() {
        assert!(model.tick());
    }
    let pending = model.state.persistence.keys().copied().collect::<Vec<_>>();
    model.event(Event::Observe {
        history: None,
        token: 20,
        id: id(20),
        offer: Some(offer(20, &[2, 3])),
        origin: false,
        clear: false,
    });
    for token in pending {
        model
            .pending
            .retain(|event| !matches!(event,Event::Persisted{token:old,..} if *old==token));
        model.event(Event::Persisted {
            token,
            success: true,
        });
    }
    assert!(model.published.is_empty());
    model.event(Event::Read {
        token: 20,
        content: None,
        fingerprints: Vec::new(),
        debounce: false,
    });
    model.settle();
    assert!(model.published.is_empty());
}

#[test]
fn identical_operations_keep_all_still_served_representations_pinned_across_history_eviction() {
    for explicit in [false, true] {
        let mut model = Compositor::default();
        let original = content("original JPEG representation");
        let converted = Content::Image(b"converted PNG representation".to_vec());
        let mut initial = remote(10, 2, 100, "original JPEG representation");
        initial.hashes.extend(hashes(&converted));
        model.event(Event::Submit(initial));
        model.settle();
        let helper_count = model.installed.len();
        // Every newer operation offers only the converted representation, while
        // the unchanged OS helper continues serving both original and converted.
        for n in 20..90 {
            let mut next = remote(n, 3, 101 + n, "unused");
            next.content = Some(converted.clone());
            next.hashes = hashes(&converted);
            model.event(Event::Submit(next));
            model.settle();
        }
        assert_eq!(
            model.installed.len(),
            helper_count,
            "identical content rewrote the OS"
        );
        assert!(
            !model
                .state
                .history
                .iter()
                .any(|f| f.id == id(10) && f.key.source == id(2)),
            "original operation must be evicted to exercise inheritance"
        );
        if explicit {
            let mut share = remote(100, 1, 0, "unused");
            share.content = Some(converted.clone());
            share.hashes = hashes(&converted);
            share.key = None;
            share.received = false;
            share.offer = Some(offer(100, &[2, 3]));
            model.event(Event::Submit(share));
            model.settle();
        }
        let winner = model.state.register.clone();
        let originated = model.published.len();
        // Pending received operations also occupy fingerprint history. Reaping
        // canceled preparation must not unpin the currently served selection.
        for n in 130..200 {
            model.event(Event::Submit(remote(
                n,
                4,
                1000 + n,
                &format!("canceled {n}"),
            )));
            model.event(Event::Cancel(n));
            model.settle();
        }
        model.copy(220, Some(original), &[2, 3]);
        model.settle();
        assert_eq!(
            model.state.register, winner,
            "old representation manufactured a stamp"
        );
        assert_eq!(
            model.published.len(),
            originated,
            "old representation originated or forwarded"
        );
        assert_eq!(originated, usize::from(explicit));
    }
}

#[test]
fn review_timeout_must_not_resurrect_legacy_clock() {
    let mut model = Compositor::default();
    let mut legacy = remote(10, 2, 0, "legacy");
    legacy.legacy = true;
    legacy.key = None;
    model.event(Event::Submit(legacy));
    model.tick(); // Prepared => Clock (delayed).
    assert!(model.state.clocking.is_some());
    model.event(Event::Timeout(1));
    // Deliver timeout's Stopped before the delayed clock result.
    let i = model
        .pending
        .iter()
        .position(|e| matches!(e, Event::Stopped(1)))
        .unwrap();
    let stopped = model.pending.remove(i).unwrap();
    model.event(stopped);
    assert_eq!(model.replies[&10].1, Some(Failure::Clipboard));
    model.settle();
    assert!(
        model.state.register.is_none(),
        "timed-out legacy op resurrected"
    );
}
#[test]
fn review_timeout_during_clock_must_not_block_new_local() {
    let mut model = Compositor::default();
    let mut legacy = remote(10, 2, 0, "legacy");
    legacy.legacy = true;
    legacy.key = None;
    model.event(Event::Submit(legacy));
    model.tick();
    model.event(Event::Timeout(1));
    model.pending.retain(|e| !matches!(e, Event::Clock { .. }));
    model.settle();
    model.copy(20, Some(content("human")), &[2]);
    model.settle();
    assert!(
        model.state.idle(),
        "orphaned clocking slot prevents future progress"
    );
}
#[test]
fn review_pending_hashes_must_stay_visible_during_cancelled_burst() {
    let mut model = Compositor::default();
    let current = remote(1, 2, 1, "current");
    model.event(Event::Submit(current.clone()));
    model.settle();
    // One authorized pending install; more submissions supersede its preparation.
    for n in 2..75 {
        model.event(Event::Submit(remote(n, 3, n, &format!("pending-{n}"))));
    }
    model.settle();
    model.consistent();
}
#[test]
fn review_marker_for_older_install_must_not_regress_local_register() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "remote")));
    while !matches!(model.pending.front(), Some(Event::Installed(_))) {
        assert!(model.tick());
    }
    model.copy(20, Some(content("local")), &[2, 3]);
    model.settle();
    model.consistent();
}
#[test]
fn review_stale_prepared_hashes_do_not_enter_history() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "remote")));
    model.event(Event::Cancel(10));
    model.settle();
    let before = model
        .state
        .history
        .iter()
        .map(|x| (x.id.clone(), x.hashes.clone()))
        .collect::<Vec<_>>();
    model.event(Event::Prepared {
        ticket: 1,
        hashes: vec![[99; 32]],
    });
    assert_eq!(
        before,
        model
            .state
            .history
            .iter()
            .map(|x| (x.id.clone(), x.hashes.clone()))
            .collect::<Vec<_>>()
    );
}
#[test]
fn review_randomized_interleavings() {
    let seconds = std::env::var("REVIEW_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let seed = std::env::var("REVIEW_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x4261_5cda_9d1f);
    randomized_interleavings(seed, seconds, 3000);
}
#[test]
fn randomized_seed_8019655356852108308_keeps_os_and_register_consistent() {
    randomized_interleavings(8019655356852108308, 0, 1);
}
fn randomized_interleavings(mut seed: u64, seconds: u64, case_limit: u64) {
    fn signature(s: &State) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{:?}|{:?}|{:?}|{:?}",
            s.register,
            s.history,
            s.current_hashes,
            s.current,
            s.pinned,
            s.observation,
            s.held_observations,
            s.queue,
            s.clocking,
            s.installing,
            s.owner,
            s.stopping,
            s.persistence.keys().collect::<Vec<_>>(),
            s.os_token,
            s.last_marker,
            s.stopping_markers,
            s.clock_retry,
            s.shutdown,
            s.physical,
            s.local_completion,
            s.mode,
            s.sharing
        )
    }
    fn check_recipient_sets(s: &State, seen: &mut HashMap<u64, Vec<Id>>) {
        let observations = s
            .observation
            .iter()
            .chain(s.held_observations.iter())
            .filter_map(|o| o.offer.as_ref().map(|f| (o.token, f)));
        let operations = s
            .queue
            .iter()
            .chain(s.clocking.iter())
            .chain(s.installing.iter().map(|i| &i.operation))
            .chain(s.persistence.values().map(|p| &p.operation))
            .chain(s.local_completion.iter().map(|p| &p.operation))
            .chain(s.sharing.values().map(|(_, _, o)| o))
            .filter_map(|o| o.offer.as_ref().map(|f| (o.token, f)));
        for (token, f) in observations.chain(operations) {
            let current = f
                .recipients
                .iter()
                .map(|r| r.target.device.clone())
                .collect::<Vec<_>>();
            if let Some(previous) = seen.insert(token, current.clone()) {
                assert!(
                    current.iter().all(|r| previous.contains(r)),
                    "recipients grew for {token}"
                );
            }
        }
    }
    let start = std::time::Instant::now();
    let mut cases = 0u64;
    let mut events = 0u64;
    while if seconds == 0 {
        cases < case_limit
    } else {
        start.elapsed().as_secs() < seconds
    } {
        cases += 1;
        let initial = seed;
        let mut model = Compositor::default();
        let mut trace = Vec::new();
        let mut token = 1u64;
        let mut seen_recipients = HashMap::new();
        let mut echoes = HashSet::new();
        for _ in 0..150 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let choice = seed % 22;
            let event = match choice {
                0..=3 if !model.pending.is_empty() => {
                    let index = (seed as usize >> 5) % model.pending.len();
                    let e = model.pending.remove(index).unwrap();
                    if let Event::Installed(ticket) = e {
                        if model.live.contains(&ticket) {
                            let completing = &model.installed[&ticket];
                            assert!(
                                completing.key.as_ref() >= model.state.register.as_ref(),
                                "older install after newer acceptance"
                            );
                            model.os = model.installed[&ticket].content.clone();
                        } else {
                            continue;
                        }
                    }
                    e
                }
                4 => {
                    token += 1;
                    let value = format!("human-{token}");
                    model.os = Some(content(&value));
                    let observe = Event::Observe {
                        history: None,
                        token,
                        id: id(token as u8),
                        offer: Some(offer(token, &[2, 3])),
                        origin: false,
                        clear: false,
                    };
                    trace.push(format!("{observe:?}"));
                    model.event(observe);
                    Event::Read {
                        token,
                        content: Some(content(&value)),
                        fingerprints: vec![],
                        debounce: seed & 128 != 0,
                    }
                }
                5 => {
                    token += 1;
                    Event::Submit(remote(
                        token,
                        if seed & 64 == 0 { 2 } else { 3 },
                        model.clock + 1,
                        &format!("remote-{token}"),
                    ))
                }
                6 if !model.state.history.is_empty() => {
                    // Clipboard manager re-offers a previously received representation.
                    let index = (seed as usize >> 6) % model.state.history.len();
                    let f = &model.state.history[index];
                    let old = model
                        .installed
                        .values()
                        .find(|o| o.id == f.id && o.source == f.key.source);
                    let Some(value) = old.and_then(|o| o.content.clone()) else {
                        continue;
                    };
                    token += 1;
                    model.os = Some(value.clone());
                    let observe = Event::Observe {
                        history: None,
                        token,
                        id: id(token as u8),
                        offer: Some(offer(token, &[2, 3])),
                        origin: false,
                        clear: false,
                    };
                    trace.push(format!("{observe:?}"));
                    model.event(observe);
                    echoes.insert(token);
                    Event::Read {
                        token,
                        content: Some(value),
                        fingerprints: vec![],
                        debounce: false,
                    }
                }
                18 => Event::Mode(match seed >> 8 & 3 {
                    0 => ClipboardMode::Automatic,
                    1 => ClipboardMode::Manual,
                    _ => ClipboardMode::Off,
                }),
                19 => {
                    token += 1;
                    Event::Share {
                        token,
                        id: id(token as u8),
                        source: id(1),
                        offer: offer(token, &[2, 3]),
                    }
                }
                20 if !model.state.sharing.is_empty() => {
                    Event::Deadline(*model.state.sharing.keys().min().unwrap())
                }
                21 => Event::ShareReady {
                    token: 1_000_000 + token,
                    content: Some(content("stale share")),
                },
                7 => Event::Narrow(id(if seed & 128 == 0 { 2 } else { 3 })),
                8 => Event::Policy {
                    allowed: offer(token, if seed & 128 == 0 { &[2, 4] } else { &[2, 3, 4] })
                        .recipients,
                },
                9 => Event::Prepared {
                    ticket: 1_000_000 + token as u128,
                    hashes: vec![[99; 32]],
                },
                11 => {
                    // Fail the next real clock, then let its retry timer race
                    // reads, supersession, policy changes and incoming work.
                    model.fail_clock = true;
                    continue;
                }
                12 => {
                    // Saturate admission while a watcher read is unresolved.
                    if !model.retained_claims.is_empty() {
                        continue;
                    }
                    token += 1;
                    let local = token;
                    model.os = Some(content(&format!("saturated-{local}")));
                    let observe = Event::Observe {
                        history: None,
                        token: local,
                        id: id(local as u8),
                        offer: Some(offer(local, &[2, 3])),
                        origin: false,
                        clear: false,
                    };
                    trace.push(format!("{observe:?}"));
                    model.event(observe);
                    for _ in 0..crate::work::BLOCKING_LIMIT {
                        token += 1;
                        model.retained_claims.insert(token);
                        let submit = Event::Submit(remote(
                            token,
                            2,
                            model.clock + 1,
                            &format!("saturated-remote-{token}"),
                        ));
                        trace.push(format!("{submit:?}"));
                        model.event(submit);
                    }
                    Event::Read {
                        token: local,
                        content: model.os.clone(),
                        fingerprints: vec![],
                        debounce: seed & 128 != 0,
                    }
                }
                13 if !model.pending.is_empty() => {
                    // Storage can fail both before installation and during
                    // receipt completion; late success must not revive a job.
                    let Some(index) = model.pending.iter().position(|e| {
                        matches!(e, Event::HistorySaved { .. } | Event::Persisted { .. })
                    }) else {
                        continue;
                    };
                    match model.pending.remove(index).unwrap() {
                        Event::HistorySaved { ticket, .. } => Event::HistorySaved {
                            ticket,
                            success: false,
                        },
                        Event::Persisted { token, .. } => Event::Persisted {
                            token,
                            success: false,
                        },
                        _ => unreachable!(),
                    }
                }
                14 => Event::RetryClock(1_000_000 + token),
                15 => {
                    // A reader can disappear before sending any result.
                    token += 1;
                    model.os = Some(content(&format!("unread-{token}")));
                    Event::Observe {
                        history: None,
                        token,
                        id: id(token as u8),
                        offer: Some(offer(token, &[2, 3])),
                        origin: false,
                        clear: false,
                    }
                }
                16 if !model.read_timeouts.is_empty() => {
                    let token = *model.read_timeouts.iter().min().unwrap();
                    Event::ReadTimeout(token)
                }
                10 => Event::Clock {
                    token: 1_000_000 + token,
                    result: Some(Key {
                        hlc: u64::MAX - 1,
                        source: id(8),
                    }),
                },
                _ => Event::Persisted {
                    token: 1_000_000 + token,
                    success: true,
                },
            };
            trace.push(format!("{event:?}"));
            let stale = matches!(&event,Event::Prepared{ticket,..} if *ticket>=1_000_000)
                || matches!(&event,Event::Clock{token,..}|Event::Persisted{token,..} if *token>=1_000_000);
            let before = signature(&model.state);
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| model.event(event)));
            if result.is_err() {
                panic!("seed={initial} event failure trace={trace:#?}");
            }
            if stale {
                assert_eq!(before, signature(&model.state), "stale token changed state");
            }
            assert!(
                model.state.pending_count() <= 64,
                "seed={initial} pending bound trace={trace:#?}"
            );
            if let Some(retry) = &model.state.clock_retry {
                assert_eq!(
                    model.state.clocking.as_ref().map(|o| o.token),
                    Some(retry.token)
                );
                if retry.waiting {
                    assert!(
                        model.pending.iter().any(
                            |e| matches!(e, Event::RetryClock(token) if *token == retry.token)
                        )
                    );
                }
            }
            check_recipient_sets(&model.state, &mut seen_recipients);
            assert!(
                model.published.iter().all(|o| !echoes.contains(&o.token)),
                "manager echo originated an operation"
            );
            events += 1;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            model.fail_clock = false;
            for token in model.read_timeouts.clone() {
                model.event(Event::ReadTimeout(token));
            }
            model.settle();
            model.consistent();
            assert!(
                model.retained_claims.is_empty(),
                "retained claims after recovery"
            );
            assert!(model.state.idle(), "not idle after quiescence");
        }));
        if result.is_err() {
            panic!("seed={initial} quiescence failure trace={trace:#?}");
        }
    }
    eprintln!(
        "RANDOMIZED_OK cases={cases} events={events} seconds={}",
        start.elapsed().as_secs_f64()
    );
}

#[test]
fn review_identical_incoming_updates_key_without_prepare() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "same")));
    model.settle();
    let installs = model.installed.len();
    model.event(Event::Submit(remote(20, 3, 100, "same")));
    model.settle();
    assert_eq!(
        model.state.register,
        Some(Key {
            hlc: 100,
            source: id(3)
        })
    );
    assert_eq!(model.installed.len(), installs);
    assert!(model.replies[&20].0);
}
#[test]
fn review_unrelated_events_do_not_grow_recipients() {
    let mut model = Compositor::default();
    model.copy(10, Some(content("human")), &[2, 3]);
    model.event(Event::Narrow(id(3)));
    model.event(Event::Policy {
        allowed: offer(11, &[2, 3, 4]).recipients,
    });
    model.settle();
    let recipients = &model.published[0].offer.as_ref().unwrap().recipients;
    assert_eq!(
        recipients
            .iter()
            .map(|r| r.target.device.clone())
            .collect::<Vec<_>>(),
        vec![id(2)]
    );
}
#[test]
fn review_human_read_may_be_delayed_across_arbitrary_other_completions() {
    for delay in 0..8 {
        let mut model = Compositor::default();
        model.event(Event::Submit(remote(10, 2, 100, "remote")));
        for _ in 0..delay {
            model.tick();
        }
        model.os = Some(content("human"));
        model.event(Event::Observe {
            history: None,
            token: 20,
            id: id(20),
            offer: Some(offer(20, &[2, 3])),
            origin: false,
            clear: false,
        });
        model.settle();
        model.event(Event::Read {
            token: 20,
            content: Some(content("human")),
            fingerprints: vec![],
            debounce: false,
        });
        model.settle();
        model.consistent();
        assert_eq!(model.os, Some(content("human")));
    }
}

#[test]
fn review_copy_after_stop_requested_but_before_marker_must_be_restored() {
    for revoke in [true, false] {
        let mut model = Compositor::default();
        model.event(Event::Submit(remote(10, 2, 100, "remote")));
        while !matches!(model.pending.front(), Some(Event::Installed(1))) {
            assert!(model.tick());
        }
        model.event(if revoke {
            Event::Revoke(id(2))
        } else {
            Event::Timeout(1)
        });
        // Stop is requested, but the helper has not yet been reaped. Its G
        // request was already sent, so it can have changed the compositor
        // selection after this human copy, with its marker still queued.
        model.copy(20, Some(content("human")), &[3]);
        model.os = Some(content("remote"));
        model.event(Event::Installed(1));
        // Reaping clears the helper's now-current selection.
        model.os = None;
        model.event(Event::Observe {
            history: None,
            token: 30,
            id: id(30),
            offer: Some(offer(30, &[3])),
            origin: false,
            clear: true,
        });
        model.event(Event::Read {
            token: 30,
            content: None,
            fingerprints: vec![],
            debounce: false,
        });
        model.settle();
        assert_eq!(
            model.os,
            Some(content("human")),
            "revoke={revoke}: a local copy ordered before the marker was lost"
        );
        model.consistent();
    }
}

#[test]
fn review_mixed_local_formats_must_keep_served_received_representation_pinned() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "received text")));
    model.settle();
    model.copy(
        20,
        Some(Content::Formats(
            "received text".into(),
            b"new human image".to_vec(),
        )),
        &[2, 3],
    );
    model.settle();
    assert!(
        matches!(
            model.published.last().unwrap().content,
            Some(Content::Image(_))
        ),
        "received text must not be forwarded"
    );
    for n in 30..110 {
        model.event(Event::Submit(remote(
            n,
            4,
            1000 + n,
            &format!("canceled-{n}"),
        )));
        model.event(Event::Cancel(n));
        model.settle();
    }
    let register = model.state.register.clone();
    let published = model.published.len();
    // The physical mixed selection still serves the original received text.
    // A manager may take only its text representation.
    model.copy(200, Some(content("received text")), &[2, 3]);
    model.settle();
    assert_eq!(
        model.published.len(),
        published,
        "still-served received representation originated after eviction"
    );
    assert_eq!(model.state.register, register);
}

#[test]
fn probe_restart_at_each_install_and_finalize_stage() {
    for phase in 0..8 {
        let op = remote(1, 2, 100, "remote");
        let mut state = State::default();
        let initial = state.step(Event::Submit(op.clone()));
        let ticket = initial
            .iter()
            .find_map(|e| match e {
                Effect::Prepare { ticket, .. } => Some(*ticket),
                _ => None,
            })
            .unwrap();
        let mut durable_history = vec![];
        let mut durable_key = None;
        if phase >= 1 {
            state.step(Event::Prepared {
                ticket,
                hashes: state.fingerprint_key.hashes(&op.hashes),
            });
        }
        if phase >= 2 {
            durable_history = state.history.iter().cloned().collect();
        }
        if phase >= 3 {
            state.step(Event::HistorySaved {
                ticket,
                success: true,
            });
        }
        // phase 4: helper has received G / OS selection exists, watcher marker not delivered yet.
        if phase >= 5 {
            state.step(Event::Installed(ticket));
        }
        if phase >= 6 {
            durable_key = state.register.clone();
        }
        if phase >= 7 {
            state.step(Event::Persisted {
                token: 1,
                success: true,
            });
        }
        let mut restarted = State::load(
            durable_key.clone(),
            HashMap::new(),
            durable_history,
            state.fingerprint_key.clone(),
        );
        assert_eq!(restarted.register, durable_key);
        if phase >= 2 {
            assert!(restarted.echo(&content("remote")));
        }
        let effects = restarted.step(Event::Submit(op));
        if phase >= 6 {
            assert!(effects.iter().all(|e| !matches!(e, Effect::Prepare { .. })));
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::Persist { applied: false, .. }))
            );
        } else {
            assert!(
                effects.iter().any(|e| matches!(e, Effect::Prepare { .. })),
                "unfinished best-effort receipt must remain retryable"
            );
        }
    }
    println!(
        "PASS modeled restart at 8 prepare/history/authorize/OS-marker/register/finalize boundaries"
    );
}

#[test]
fn local_observations_coalesce_persistence_and_leave_receipt_capacity() {
    let mut state = State::default();
    for token in 1..=256 {
        state.step(Event::Observe {
            history: None,
            token,
            id: id(token as u8),
            offer: Some(offer(token, &[2])),
            origin: false,
            clear: false,
        });
        let effects = state.step(Event::Read {
            token,
            content: Some(content(&format!("local-{token}"))),
            fingerprints: vec![],
            debounce: false,
        });
        assert!(effects.iter().any(|e| matches!(e, Effect::Clock { .. })));
        state.step(Event::Clock {
            token,
            result: Some(Key {
                hlc: token,
                source: id(1),
            }),
        });
        assert!(state.pending_count() <= 64);
    }
    assert_eq!(state.persistence.len(), 1);
    assert_eq!(
        state.local_completion.as_ref().unwrap().operation.token,
        256
    );
    assert_eq!(state.register.as_ref().unwrap().hlc, 256);
    let effects = state.step(Event::Submit(remote(999, 2, 1000, "remote")));
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Prepare { .. })),
        "local burst consumed receipt capacity"
    );
    let effects = state.step(Event::Persisted {
        token: 1,
        success: true,
    });
    assert!(
        effects
            .iter()
            .any(|e| matches!(e,Effect::Persist{operation,..} if operation.token==256))
    );
}
#[test]
fn binding_reset_keeps_keyed_physical_echo_provenance() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(1, 2, 10, "1234")));
    model.settle();
    model.event(Event::BindingReset(id(2)));
    model.settle();
    let raw: [u8; 32] = Sha256::digest(b"1234").into();
    assert!(model.state.history.iter().all(|f| !f.hashes.contains(&raw)));
    assert!(model.state.echo(&content("1234")));
    let key = model.state.register.clone();
    model.copy(200, Some(content("1234")), &[3]);
    model.settle();
    assert!(model.published.is_empty());
    assert_eq!(model.state.register, key);
}
#[test]
fn every_install_terminal_path_retires_legacy_clock_and_ignores_late_results() {
    for terminal in [
        Event::Timeout(1),
        Event::Deadline(10),
        Event::Failed(1),
        Event::Stopped(1),
        Event::Cancel(10),
        Event::Revoke(id(2)),
        Event::Shutdown,
    ] {
        let mut model = Compositor::default();
        let mut op = remote(10, 2, 0, "legacy");
        op.legacy = true;
        op.key = None;
        model.event(Event::Submit(op));
        model.tick();
        model.event(terminal.clone());
        model.settle();
        assert!(model.state.clocking.is_none(), "terminal={terminal:?}");
        assert!(model.state.installing.is_none(), "terminal={terminal:?}");
        assert_eq!(model.replies.len(), 1);
        let key = model.state.register.clone();
        for late in [
            Event::Clock {
                token: 10,
                result: Some(Key {
                    hlc: 999,
                    source: id(2),
                }),
            },
            Event::Prepared {
                ticket: 1,
                hashes: vec![[99; 32]],
            },
            Event::HistorySaved {
                ticket: 1,
                success: true,
            },
            Event::Installed(1),
            Event::Failed(1),
            Event::Stopped(1),
        ] {
            model.event(late);
        }
        model.settle();
        assert_eq!(model.state.register, key);
        assert_eq!(model.replies.len(), 1);
    }
}

#[test]
fn mixed_local_restoration_keeps_full_physical_selection_and_received_provenance() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(1, 2, 1, "received text")));
    model.settle();
    model.event(Event::Submit(remote(10, 3, 100, "pending remote")));
    while !matches!(model.pending.front(), Some(Event::Installed(_))) {
        assert!(model.tick());
    }
    let mixed = Content::Formats("received text".into(), b"human image".to_vec());
    model.copy(20, Some(mixed.clone()), &[2, 3]);
    model.settle();
    assert_eq!(model.os, Some(mixed));
    assert_eq!(model.published.len(), 1);
    assert_eq!(
        model.published[0].content,
        Some(Content::Image(b"human image".to_vec()))
    );
    assert!(model.state.physical.iter().any(|r| r.received.is_some()));
    for n in 30..110 {
        model.event(Event::Submit(remote(
            n,
            4,
            1000 + n,
            &format!("canceled-{n}"),
        )));
        model.event(Event::Cancel(n));
        model.settle();
    }
    let published = model.published.len();
    let register = model.state.register.clone();
    let restarted = State::load(
        register.clone(),
        HashMap::new(),
        model.state.history.iter().cloned().collect(),
        model.state.fingerprint_key.clone(),
    );
    assert!(
        restarted.echo(&content("received text")),
        "physical provenance was evicted from durable history"
    );
    model.copy(200, Some(content("received text")), &[3]);
    model.settle();
    assert_eq!(model.published.len(), published);
    assert_eq!(model.state.register, register);
}
#[test]
fn saturated_admitted_receipts_and_local_burst_share_the_pending_cap() {
    let mut state = State::default();
    state.step(Event::Submit(remote(1, 2, 1, "same")));
    state.step(Event::Prepared {
        ticket: 1,
        hashes: vec![],
    });
    state.step(Event::HistorySaved {
        ticket: 1,
        success: true,
    });
    state.step(Event::Installed(1));
    for token in 2..=61 {
        state.step(Event::Submit(remote(token, 2, token, "same")));
    }
    assert_eq!(state.persistence.len(), 61);
    for token in 100..=355 {
        state.step(Event::Observe {
            history: None,
            token,
            id: id(token as u8),
            offer: Some(offer(token, &[2])),
            origin: false,
            clear: false,
        });
        state.step(Event::Read {
            token,
            content: Some(content(&format!("local-{token}"))),
            fingerprints: vec![],
            debounce: false,
        });
        state.step(Event::Clock {
            token,
            result: Some(Key {
                hlc: token + 1000,
                source: id(1),
            }),
        });
        assert!(state.pending_count() <= 64);
    }
    for token in 1..=61 {
        let effects = state.step(Event::Persisted {
            token,
            success: true,
        });
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e,Effect::Reply{token:t,failure:None,..} if *t==token))
                .count(),
            1
        );
        assert!(
            state
                .step(Event::Persisted {
                    token,
                    success: true
                })
                .is_empty()
        );
    }
    let effects = state.step(Event::Persisted {
        token: 100,
        success: true,
    });
    assert!(
        effects
            .iter()
            .any(|e| matches!(e,Effect::Persist{operation,..} if operation.token==355))
    );
    state.step(Event::Persisted {
        token: 355,
        success: true,
    });
    assert!(state.idle());
}

#[test]
fn confirmed_os_effect_keeps_receipt_completion_when_request_deadline_expires() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "remote")));
    while !model.state.persistence.contains_key(&10) {
        assert!(model.tick());
    }
    model.event(Event::Deadline(10));
    model.settle();
    assert_eq!(model.replies.get(&10), Some(&(false, None)));
    assert_eq!(model.os, Some(content("remote")));
    assert!(model.state.idle());
}

#[test]
fn retired_markers_reoffer_received_formats_and_restore_the_register_winner() {
    for legacy in [false, true] {
        for format in 0..3 {
            let mut model = Compositor::default();
            let mut older = remote(10, 2, 100, "old remote");
            older.content = Some(match format {
                0 => content("old remote"),
                1 => Content::Image(b"old image".to_vec()),
                _ => Content::Formats("old remote".into(), b"old image".to_vec()),
            });
            older.legacy = legacy;
            if legacy {
                older.key = None;
            }
            older.hashes = hashes(older.content.as_ref().unwrap());
            let offered = older.content.clone();
            model.event(Event::Submit(older));
            model.settle();
            let retired = model.state.owner.unwrap();
            let mut newer = remote(20, 2, 200, "winner");
            newer.legacy = legacy;
            if legacy {
                newer.key = None;
            }
            model.event(Event::Submit(newer));
            model.settle();
            let winner = model.state.register.clone();
            // The watcher asks the actor before treating a MIME as confirmation.
            assert!(!model.state.confirms_install(retired));
            model.copy(30, offered, &[2, 3]);
            model.settle();
            model.consistent();
            assert_eq!(model.state.register, winner);
            assert_eq!(model.os, Some(content("winner")));
            assert!(model.published.is_empty());
        }
    }
}

#[test]
fn only_current_authorized_install_accepts_a_marker_confirmation() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "remote")));
    let ticket = model.state.installing.as_ref().unwrap().ticket;
    assert!(!model.state.confirms_install(ticket));
    assert!(!model.state.confirms_install(ticket.wrapping_add(1)));
    while !matches!(model.pending.front(), Some(Event::Installed(_))) {
        assert!(model.tick());
    }
    assert!(model.state.confirms_install(ticket));
    model.settle();
    assert!(!model.state.confirms_install(ticket));
}
#[test]
fn retired_marker_with_received_and_human_formats_preserves_per_format_provenance() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "received")));
    model.settle();
    let retired = model.state.owner.unwrap();
    model.event(Event::Submit(remote(20, 3, 200, "winner")));
    model.settle();
    assert!(!model.state.confirms_install(retired));
    model.copy(
        30,
        Some(Content::Formats("received".into(), b"human image".to_vec())),
        &[2, 3],
    );
    model.settle();
    assert_eq!(model.published.len(), 1);
    assert_eq!(
        model.published[0].content,
        Some(Content::Image(b"human image".to_vec()))
    );
    model.consistent();
}

fn repair_formats(label: &str) -> Vec<Content> {
    vec![
        content(label),
        Content::Image(format!("{label} image").into_bytes()),
        Content::Formats(label.into(), format!("{label} image").into_bytes()),
    ]
}

#[test]
fn repair_payload_tracks_local_and_deduplicated_winners_across_all_formats() {
    for older in repair_formats("older") {
        for winner in repair_formats("winner") {
            let mut candidates = vec![None, Some(winner.clone())];
            if let Content::Formats(text, image) = &winner {
                candidates.extend([
                    Some(Content::Text(text.clone())),
                    Some(Content::Image(image.clone())),
                    Some(Content::Formats(
                        text.clone(),
                        b"uninstalled image".to_vec(),
                    )),
                ]);
            }
            for duplicate in candidates {
                let mut model = Compositor::default();
                let mut operation = remote(10, 2, 100, "older");
                operation.content = Some(older.clone());
                operation.hashes = hashes(&older);
                model.event(Event::Submit(operation));
                model.settle();
                model.copy(20, Some(winner.clone()), &[2, 3]);
                model.settle();
                let before = model.installed.len();
                if let Some(duplicate) = duplicate {
                    let mut operation = remote(30, 3, model.clock + 1, "winner");
                    operation.hashes = hashes(&duplicate);
                    // Received images have no outbound Content. Their exact
                    // opaque payload still belongs to the register candidate.
                    operation.content = match &duplicate {
                        Content::Image(_) => None,
                        _ => Some(duplicate.clone()),
                    };
                    operation.payload = Some(Arc::new(Payload::Content(duplicate)));
                    model.event(Event::Submit(operation));
                    model.settle();
                    assert_eq!(model.replies.get(&30), Some(&(true, None)));
                }
                assert_eq!(model.installed.len(), before, "dedup installed a helper");
                let register = model.state.register.clone();
                let physical_hashes = model.state.current_hashes.clone();
                let publications = model.published.len();
                model.copy(40, Some(older.clone()), &[2, 3]);
                model.settle();
                assert_eq!(model.os, Some(winner.clone()));
                assert_eq!(model.state.register, register);
                assert_eq!(model.state.current_hashes, physical_hashes);
                assert_eq!(model.published.len(), publications);
                assert_eq!(model.installed.len(), before + 1);
            }
        }
    }
}

#[test]
fn restart_repairs_only_when_winner_payload_was_captured() {
    for captured in [false, true] {
        let mut model = Compositor::default();
        model.event(Event::Submit(remote(10, 2, 100, "older")));
        model.settle();
        model.event(Event::Submit(remote(20, 2, 200, "winner")));
        model.settle();
        model.state = State::load(
            model.state.register.clone(),
            HashMap::new(),
            model.state.history.iter().cloned().collect(),
            model.state.fingerprint_key.clone(),
        );
        model.installed.clear();
        model.live.clear();
        model.stopped.clear();
        model.os = Some(content("winner"));
        model.event(Event::Observe {
            history: None,
            token: 30,
            id: id(30),
            offer: None,
            origin: false,
            clear: false,
        });
        // Unsupported representations can still report echo fingerprints without
        // a usable payload. Only actual captured bytes may enable later repairs.
        model.event(Event::Read {
            token: 30,
            content: captured.then(|| content("winner")),
            fingerprints: hashes(&content("winner")),
            debounce: false,
        });
        model.settle();
        model.event(Event::Submit(remote(40, 3, 300, "winner")));
        model.settle();
        assert_eq!(
            model.state.current.as_ref().unwrap().payload.is_some(),
            captured
        );
        let register = model.state.register.clone();
        model.copy(50, Some(content("older")), &[2]);
        model.settle();
        assert_eq!(model.installed.len(), usize::from(captured));
        assert_eq!(
            model.os,
            Some(content(if captured { "winner" } else { "older" }))
        );
        assert_eq!(model.state.register, register);
        assert!(model.state.os_current);
        assert_eq!(
            model.state.current.as_ref().unwrap().key == register,
            captured
        );
    }
}

#[test]
fn repair_rejects_prepared_bytes_that_no_longer_match_the_winner() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "older")));
    model.settle();
    model.event(Event::Submit(remote(20, 2, 200, "winner")));
    model.settle();
    let register = model.state.register.clone();
    model.copy(30, Some(content("older")), &[2]);
    let ticket = model.state.installing.as_ref().unwrap().ticket;
    model
        .pending
        .retain(|event| !matches!(event, Event::Prepared { .. }));
    model.event(Event::Prepared {
        ticket,
        hashes: hashes(&content("changed file")),
    });
    model.settle();
    assert_eq!(model.os, Some(content("older")));
    assert_eq!(model.state.register, register);
    assert!(!model.state.os_current);
    assert!(model.stopped.contains(&ticket));
}

#[test]
fn local_clock_completes_with_all_claims_retained_during_read_or_debounce() {
    for debounce in [false, true] {
        let mut model = Compositor {
            os: Some(content("human")),
            ..Default::default()
        };
        model.event(Event::Observe {
            history: None,
            token: 1,
            id: id(1),
            offer: Some(offer(1, &[2])),
            origin: false,
            clear: false,
        });
        if debounce {
            model.event(Event::Read {
                token: 1,
                content: model.os.clone(),
                fingerprints: vec![],
                debounce: true,
            });
        }
        for token in 2..=17 {
            model.retained_claims.insert(token);
            model.event(Event::Submit(remote(token, token as u8, token, "incoming")));
        }
        assert_eq!(model.retained_claims.len(), crate::work::BLOCKING_LIMIT);
        assert!(model.state.clocking.is_none());
        if debounce {
            model.event(Event::Debounce(1));
        } else {
            model.event(Event::Read {
                token: 1,
                content: model.os.clone(),
                fingerprints: vec![],
                debounce: false,
            });
        }
        assert_eq!(model.state.clocking.as_ref().unwrap().token, 1);
        assert_eq!(model.retained_claims.len(), crate::work::BLOCKING_LIMIT);
        model.settle();
        assert!(model.state.idle());
        assert!(model.retained_claims.is_empty());
        assert!(model.replies.values().all(|(_, failure)| failure.is_none()));
        assert_eq!(model.published.len(), 1);
        model.consistent();
    }
}

#[test]
fn failed_observation_clock_retries_with_bounded_backoff_and_unblocks_reception() {
    let mut model = Compositor {
        fail_clock: true,
        ..Default::default()
    };
    model.copy(10, Some(content("local")), &[2, 3]);
    model.event(Event::Submit(remote(20, 2, 200, "incoming")));
    for expected in [100, 200, 400, 800, 1600, 2000, 2000, 2000] {
        let index = model
            .pending
            .iter()
            .position(|e| matches!(e, Event::Clock { token: 10, .. }))
            .unwrap();
        let event = model.pending.remove(index).unwrap();
        model.event(event);
        let retry = model.state.clock_retry.as_ref().unwrap();
        assert_eq!(retry.delay_ms, expected);
        assert!(retry.waiting);
        assert_eq!(model.state.clocking.as_ref().unwrap().id, id(10));
        assert!(!model.replies.contains_key(&10));
        assert!(!model.replies.contains_key(&20));
        assert!(model.state.pending_count() <= 64);
        let index = model
            .pending
            .iter()
            .position(|e| matches!(e, Event::RetryClock(10)))
            .unwrap();
        let event = model.pending.remove(index).unwrap();
        // Exercise the capped delay repeatedly before allowing recovery.
        model.fail_clock = true;
        model.event(event);
        let effects = model.state.step(Event::RetryClock(10));
        assert!(effects.is_empty(), "duplicate retry started a second clock");
    }
    model.event(Event::Narrow(id(3)));
    let index = model
        .pending
        .iter()
        .position(|e| matches!(e, Event::Clock { token: 10, .. }))
        .unwrap();
    let event = model.pending.remove(index).unwrap();
    model.event(event);
    model.fail_clock = false;
    model.settle();
    assert!(model.state.idle());
    assert!(model.state.clock_retry.is_none());
    assert_eq!(model.replies[&10].1, None);
    assert_eq!(model.replies[&20].1, None);
    assert_eq!(model.published.len(), 1);
    assert_eq!(model.published[0].id, id(10));
    assert_eq!(
        model.published[0].offer.as_ref().unwrap().recipients.len(),
        1
    );
    model.consistent();
}

#[test]
fn failed_clock_retry_is_retired_by_newer_local_cancellation_and_shutdown() {
    for terminal in 0..3 {
        let mut model = Compositor {
            fail_clock: true,
            ..Default::default()
        };
        model.copy(10, Some(content("old")), &[2]);
        while model.state.clock_retry.is_none() {
            assert!(model.tick());
        }
        match terminal {
            0 => model.copy(20, Some(content("new")), &[2]),
            1 => model.event(Event::Cancel(10)),
            _ => model.event(Event::Shutdown),
        }
        let effects = model.state.step(Event::RetryClock(10));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Clock { token: 10, .. }))
        );
        assert!(model.state.clock_retry.is_none());
        model.settle();
        assert!(model.state.idle());
        assert_eq!(model.replies[&10].1, Some(Failure::Cancelled));
        if terminal == 0 {
            assert_eq!(model.os, Some(content("new")));
            assert_eq!(model.published.len(), 1);
        }
    }
}

#[test]
fn lost_reader_completion_has_a_terminal_timer_and_cannot_block_reception() {
    let mut model = Compositor::default();
    model.event(Event::Observe {
        history: None,
        token: 10,
        id: id(10),
        offer: Some(offer(10, &[2])),
        origin: false,
        clear: false,
    });
    model.event(Event::Submit(remote(20, 2, 200, "remote")));
    assert!(model.state.observation.is_some());
    assert!(model.read_timeouts.contains(&10));
    model.event(Event::ReadTimeout(10));
    model.settle();
    assert!(model.state.idle());
    assert_eq!(model.replies[&20].1, None);
    assert!(
        model.published.is_empty(),
        "unread bytes must never be published"
    );
    assert!(model.state.step(Event::ReadTimeout(10)).is_empty());
}

#[test]
fn initial_selection_is_manual_only_across_modes_and_watcher_restarts() {
    for initial_mode in [
        ClipboardMode::Automatic,
        ClipboardMode::Manual,
        ClipboardMode::Off,
    ] {
        for value in [
            content("startup"),
            Content::Image(vec![1, 2, 3]),
            Content::Formats("startup".into(), vec![1, 2, 3]),
        ] {
            for change_during_read in [false, true] {
                let mut m = Compositor::default();
                m.event(Event::Mode(initial_mode));
                for (token, mode) in [
                    (10, initial_mode),
                    (30, ClipboardMode::Off),
                    (50, ClipboardMode::Automatic),
                ] {
                    m.event(Event::Mode(mode));
                    m.os = Some(value.clone());
                    // Every watcher generation starts without an automatic offer.
                    m.event(Event::Observe {
                        history: None,
                        token,
                        id: id(token as u8),
                        offer: None,
                        origin: false,
                        clear: false,
                    });
                    if change_during_read {
                        m.event(Event::Mode(ClipboardMode::Manual));
                    }
                    m.event(Event::Read {
                        token,
                        content: Some(value.clone()),
                        fingerprints: vec![],
                        debounce: false,
                    });
                    m.settle();
                    assert!(m.published.is_empty());
                    assert!(m.state.os_current);
                    assert_eq!(
                        m.state
                            .current
                            .as_ref()
                            .unwrap()
                            .payload
                            .as_deref()
                            .map(|p| match p {
                                Payload::Content(c) => c,
                                _ => panic!("in-memory selection"),
                            }),
                        Some(&value)
                    );
                }
                m.event(Event::Mode(ClipboardMode::Manual));
                m.event(Event::Share {
                    token: 60,
                    id: id(60),
                    source: id(1),
                    offer: offer(60, &[2, 3]),
                });
                m.settle();
                assert_eq!(m.replies[&60].1, None);
                assert_eq!(m.published.len(), 1);
                assert_eq!(m.published[0].content, Some(value.clone()));
            }
        }
    }
}

#[test]
fn initial_received_echo_restores_payload_without_advancing_or_repairing_register() {
    for older in [false, true] {
        for value in [
            content("received"),
            Content::Image(vec![4, 5]),
            Content::Formats("received".into(), vec![4, 5]),
        ] {
            let fingerprint_key = FingerprintKey::default();
            let fingerprint = Fingerprint {
                id: id(2),
                key: Key {
                    hlc: 10,
                    source: id(2),
                },
                hashes: fingerprint_key.hashes(&hashes(&value)),
            };
            let register = if older {
                Key {
                    hlc: 20,
                    source: id(3),
                }
            } else {
                fingerprint.key.clone()
            };
            let mut m = Compositor {
                state: State::load(
                    Some(register.clone()),
                    HashMap::new(),
                    vec![fingerprint],
                    fingerprint_key,
                ),
                clock: register.hlc,
                ..Default::default()
            };
            m.event(Event::Mode(ClipboardMode::Automatic));
            m.os = Some(value.clone());
            m.event(Event::Observe {
                history: None,
                token: 30,
                id: id(30),
                offer: None,
                origin: false,
                clear: false,
            });
            m.event(Event::Read {
                token: 30,
                content: Some(value.clone()),
                fingerprints: hashes(&value),
                debounce: false,
            });
            m.settle();
            assert_eq!(m.state.register, Some(register.clone()));
            assert_eq!(m.clock, register.hlc);
            assert!(m.published.is_empty());
            assert!(m.installed.is_empty());
            assert!(m.replies[&30].0);
            // A stripped-marker manager reoffer is still an echo after capture.
            m.copy(40, Some(value.clone()), &[2, 3]);
            m.settle();
            assert_eq!(m.state.register, Some(register));
            assert!(m.published.is_empty());
            m.event(Event::Mode(ClipboardMode::Manual));
            m.event(Event::Share {
                token: 50,
                id: id(50),
                source: id(1),
                offer: offer(50, &[2, 3]),
            });
            m.settle();
            assert_eq!(m.replies[&50].1, None);
            assert_eq!(m.published.len(), 1);
            assert_eq!(m.published[0].content, Some(value));
            assert_eq!(
                m.published[0]
                    .offer
                    .as_ref()
                    .unwrap()
                    .recipients
                    .iter()
                    .map(|r| r.target.device.clone())
                    .collect::<Vec<_>>(),
                vec![id(3)]
            );
        }
    }
}

#[test]
fn initial_older_echo_can_recover_the_current_winner_payload_on_next_observation() {
    let fingerprint_key = FingerprintKey::default();
    let history = [(2, "older"), (3, "winner")]
        .into_iter()
        .map(|(n, text)| Fingerprint {
            id: id(n),
            key: Key {
                hlc: n as u64,
                source: id(n),
            },
            hashes: fingerprint_key.hashes(&hashes(&content(text))),
        })
        .collect::<Vec<_>>();
    let register = history[1].key.clone();
    let mut m = Compositor {
        state: State::load(
            Some(register.clone()),
            HashMap::new(),
            history,
            fingerprint_key,
        ),
        clock: register.hlc,
        ..Default::default()
    };
    m.os = Some(content("older"));
    m.event(Event::Observe {
        history: None,
        token: 10,
        id: id(10),
        offer: None,
        origin: false,
        clear: false,
    });
    m.event(Event::Read {
        token: 10,
        content: m.os.clone(),
        fingerprints: vec![],
        debounce: false,
    });
    m.settle();
    assert_ne!(
        m.state.current.as_ref().unwrap().key,
        Some(register.clone())
    );
    m.copy(20, Some(content("winner")), &[2, 3, 4]);
    m.settle();
    assert_eq!(m.state.register, Some(register.clone()));
    assert_eq!(m.state.current.as_ref().unwrap().key, Some(register));
    assert!(m.installed.is_empty());
    assert!(m.published.is_empty());
    m.event(Event::Mode(ClipboardMode::Manual));
    m.event(Event::Share {
        token: 30,
        id: id(30),
        source: id(1),
        offer: offer(30, &[2, 3, 4]),
    });
    m.settle();
    assert_eq!(m.replies[&30].1, None);
    assert_eq!(m.published[0].content, Some(content("winner")));
    assert_eq!(
        m.published[0]
            .offer
            .as_ref()
            .unwrap()
            .recipients
            .iter()
            .map(|r| r.target.device.clone())
            .collect::<Vec<_>>(),
        vec![id(2), id(4)]
    );
}

#[test]
fn initial_mixed_echo_keeps_each_representations_received_source() {
    let value = Content::Formats("received text".into(), vec![7, 8, 9]);
    let fingerprint_key = FingerprintKey::default();
    let fingerprints = fingerprint_key.hashes(&hashes(&value));
    let history: Vec<_> = [2, 3]
        .into_iter()
        .enumerate()
        .map(|(i, source)| Fingerprint {
            id: id(source),
            key: Key {
                hlc: source as u64,
                source: id(source),
            },
            hashes: vec![fingerprints[i]],
        })
        .collect();
    let register = history[1].key.clone();
    let mut m = Compositor {
        state: State::load(
            Some(register.clone()),
            HashMap::new(),
            history,
            fingerprint_key,
        ),
        clock: register.hlc,
        ..Default::default()
    };
    m.os = Some(value.clone());
    m.event(Event::Observe {
        history: None,
        token: 10,
        id: id(10),
        offer: None,
        origin: false,
        clear: false,
    });
    m.event(Event::Read {
        token: 10,
        content: Some(value),
        fingerprints: vec![],
        debounce: false,
    });
    m.settle();
    assert_eq!(m.state.register, Some(register));
    assert!(m.published.is_empty());
    m.event(Event::Mode(ClipboardMode::Manual));
    m.event(Event::Share {
        token: 20,
        id: id(20),
        source: id(1),
        offer: offer(20, &[2, 3, 4]),
    });
    m.settle();
    let recipients = &m.published[0].offer.as_ref().unwrap().recipients;
    assert_eq!(
        recipients
            .iter()
            .map(|r| (r.target.device.clone(), r.text, r.image))
            .collect::<Vec<_>>(),
        vec![
            (id(2), false, true),
            (id(3), true, false),
            (id(4), true, true)
        ]
    );
}

#[test]
fn initial_read_timeout_recovers_on_next_watcher_snapshot_without_publication() {
    let mut m = Compositor::default();
    m.event(Event::Mode(ClipboardMode::Off));
    m.event(Event::Observe {
        history: None,
        token: 10,
        id: id(10),
        offer: None,
        origin: false,
        clear: false,
    });
    m.event(Event::Mode(ClipboardMode::Automatic));
    m.event(Event::ReadTimeout(10));
    m.settle();
    assert!(m.state.idle());
    assert!(m.published.is_empty());
    m.os = Some(content("recovered"));
    m.event(Event::Observe {
        history: None,
        token: 20,
        id: id(20),
        offer: None,
        origin: false,
        clear: false,
    });
    m.event(Event::Read {
        token: 20,
        content: m.os.clone(),
        fingerprints: vec![],
        debounce: false,
    });
    m.settle();
    assert!(m.published.is_empty());
    m.event(Event::Mode(ClipboardMode::Manual));
    m.event(Event::Share {
        token: 30,
        id: id(30),
        source: id(1),
        offer: offer(30, &[2]),
    });
    m.settle();
    assert_eq!(m.replies[&30].1, None);
    assert_eq!(m.published[0].content, Some(content("recovered")));
}

#[test]
fn clipboard_mode_matrix_preserves_ordering_and_received_provenance() {
    for mode in [
        ClipboardMode::Automatic,
        ClipboardMode::Manual,
        ClipboardMode::Off,
    ] {
        let mut m = Compositor::default();
        m.event(Event::Mode(mode));
        m.os = Some(content("human"));
        m.event(Event::Observe {
            history: None,
            token: 1,
            id: id(1),
            offer: Some(offer(1, &[2, 3])),
            origin: false,
            clear: false,
        });
        m.event(Event::Read {
            token: 1,
            content: Some(content("human")),
            fingerprints: vec![],
            debounce: false,
        });
        m.settle();
        assert!(m.state.register.is_some());
        assert_eq!(
            m.published.len(),
            usize::from(mode == ClipboardMode::Automatic)
        );
        m.event(Event::Share {
            token: 2,
            id: id(2),
            source: id(1),
            offer: offer(2, &[2, 3]),
        });
        m.settle();
        assert_eq!(
            m.published.len(),
            if mode == ClipboardMode::Off { 0 } else { 1 }
        );
        m.event(Event::Submit(remote(3, 2, m.clock + 10, "received")));
        m.settle();
        assert_eq!(
            m.os.as_ref().and_then(|c| if let Content::Text(t) = c {
                Some(t.as_str())
            } else {
                None
            }),
            Some("received")
        );
        let before = m.published.len();
        m.event(Event::Share {
            token: 4,
            id: id(4),
            source: id(1),
            offer: offer(4, &[2, 3]),
        });
        m.settle();
        assert_eq!(
            m.published.len(),
            before + usize::from(mode == ClipboardMode::Manual)
        );
        if mode == ClipboardMode::Manual {
            assert_eq!(
                m.published
                    .last()
                    .unwrap()
                    .offer
                    .as_ref()
                    .unwrap()
                    .recipients
                    .iter()
                    .map(|r| r.target.device.clone())
                    .collect::<Vec<_>>(),
                vec![id(3)]
            );
        }
    }
}
#[test]
fn changing_clipboard_mode_cannot_republish_pending_or_late_manual_content() {
    let mut m = Compositor::default();
    m.event(Event::Mode(ClipboardMode::Manual));
    m.os = Some(content("human"));
    m.event(Event::Observe {
        history: None,
        token: 1,
        id: id(1),
        offer: Some(offer(1, &[2])),
        origin: false,
        clear: false,
    });
    m.event(Event::Read {
        token: 1,
        content: Some(content("human")),
        fingerprints: vec![],
        debounce: false,
    });
    m.settle();
    m.event(Event::Share {
        token: 2,
        id: id(2),
        source: id(1),
        offer: offer(2, &[2]),
    });
    m.event(Event::Mode(ClipboardMode::Off));
    m.event(Event::Mode(ClipboardMode::Automatic));
    m.settle();
    assert!(m.published.is_empty());
}

#[test]
fn pairing_link_hint_is_local_current_and_does_not_change_sharing() {
    for mode in [
        ClipboardMode::Automatic,
        ClipboardMode::Manual,
        ClipboardMode::Off,
    ] {
        let mut state = State::default();
        state.step(Event::Mode(mode));
        state.step(Event::Observe {
            history: None,
            token: 1,
            id: id(1),
            offer: Some(offer(1, &[2])),
            origin: false,
            clear: false,
        });
        let link = content("pneu://pair?ui-hint");
        let effects = state.step(Event::Read {
            token: 1,
            fingerprints: hashes(&link),
            content: Some(link.clone()),
            debounce: false,
        });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::PairingLink(Some(s)) if s == "pneu://pair?ui-hint"))
        );
        let local = state
            .clocking
            .as_ref()
            .expect("normal local classification still clocks");
        assert_eq!(local.content, Some(link));
        assert_eq!(local.offer.is_some(), mode == ClipboardMode::Automatic);
        let effects = state.step(Event::Observe {
            history: None,
            token: 2,
            id: id(2),
            offer: None,
            origin: false,
            clear: false,
        });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::PairingLink(None)))
        );
        let stale = content("pneu://pair?stale");
        let effects = state.step(Event::Read {
            token: 1,
            fingerprints: hashes(&stale),
            content: Some(stale),
            debounce: false,
        });
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::PairingLink(Some(_))))
        );
        let effects = state.step(Event::Read {
            token: 2,
            fingerprints: vec![],
            content: Some(content("ordinary text")),
            debounce: false,
        });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::PairingLink(None)))
        );
        let effects = state.step(Event::Submit(remote(3, 2, 50, "pneu://pair?remote")));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::PairingLink(Some(_))))
        );
    }
}

#[test]
fn manual_queued_share_keeps_captured_identity_through_clock_and_incoming() {
    let mut model = Compositor::default();
    model.event(Event::Mode(ClipboardMode::Manual));
    model.copy(10, Some(content("human")), &[2, 3]);
    model.settle();
    for token in [20, 21] {
        model.event(Event::Share {
            token,
            id: id(token as u8),
            source: id(1),
            offer: offer(token, &[2, 3]),
        });
    }
    // Finish both reads while the first clock is outstanding. The second share
    // must retain the original register, rather than becoming a new local edit.
    for token in [20, 21] {
        model.event(Event::ShareReady {
            token,
            content: Some(content("human")),
        });
    }
    model.event(Event::Submit(remote(30, 2, model.clock + 10, "remote")));
    model.settle();
    model.consistent();
    assert_eq!(model.os, Some(content("remote")));
    assert!(model.published.is_empty());
    assert!(model.replies[&21].1.is_some());
    assert!(model.state.manual.is_empty());
}
#[test]
fn manual_clocking_share_is_cancelled_by_new_physical_selection() {
    let mut model = Compositor::default();
    model.event(Event::Mode(ClipboardMode::Manual));
    model.copy(10, Some(content("first")), &[2]);
    model.settle();
    model.event(Event::Share {
        token: 20,
        id: id(20),
        source: id(1),
        offer: offer(20, &[2]),
    });
    model.event(Event::ShareReady {
        token: 20,
        content: Some(content("first")),
    });
    model.copy(30, Some(content("second")), &[2]);
    model.settle();
    model.consistent();
    assert!(model.published.is_empty());
    assert!(model.replies[&20].1.is_some());
    assert_eq!(model.os, Some(content("second")));
}
#[test]
fn manual_publication_requires_at_least_one_accepted_recipient() {
    for narrowed in [false, true] {
        let mut model = Compositor::default();
        model.event(Event::Mode(ClipboardMode::Manual));
        model.copy(10, Some(content("share")), &[2]);
        model.settle();
        model.event(Event::Share {
            token: 20,
            id: id(20),
            source: id(1),
            offer: offer(20, &[2]),
        });
        while !model
            .pending
            .iter()
            .any(|e| matches!(e, Event::Persisted { token: 20, .. }))
        {
            assert!(model.tick());
        }
        if narrowed {
            model.event(Event::Narrow(id(2)));
            model.settle();
        } else {
            while !model
                .pending
                .iter()
                .any(|e| matches!(e, Event::Published { token: 20, .. }))
            {
                assert!(model.tick());
            }
            assert!(!model.replies.contains_key(&20));
            model
                .pending
                .retain(|e| !matches!(e, Event::Published { token: 20, .. }));
            model.event(Event::Published {
                token: 20,
                accepted: false,
            });
            model.settle();
        }
        assert!(model.replies[&20].1.is_some());
        assert!(model.state.manual.is_empty());
    }
}
#[test]
fn manual_mixed_formats_exclude_received_source_per_representation() {
    let mut model = Compositor::default();
    model.event(Event::Mode(ClipboardMode::Manual));
    let mut incoming = remote(10, 2, 100, "unused");
    incoming.content = Some(Content::Image(vec![1, 2, 3]));
    incoming.hashes = hashes(incoming.content.as_ref().unwrap());
    model.event(Event::Submit(incoming));
    model.settle();
    model.copy(
        20,
        Some(Content::Formats("local".into(), vec![1, 2, 3])),
        &[2, 3],
    );
    model.settle();
    model.event(Event::Share {
        token: 30,
        id: id(30),
        source: id(1),
        offer: offer(30, &[2, 3]),
    });
    model.settle();
    let recipients = &model.published[0].offer.as_ref().unwrap().recipients;
    let source = recipients
        .iter()
        .find(|r| r.target.device == id(2))
        .unwrap();
    assert!(source.text);
    assert!(!source.image);
    let other = recipients
        .iter()
        .find(|r| r.target.device == id(3))
        .unwrap();
    assert!(other.text && other.image);
}
#[test]
fn manual_changed_image_bytes_are_rejected_before_clocking() {
    let mut model = Compositor::default();
    model.event(Event::Mode(ClipboardMode::Manual));
    let mut incoming = remote(10, 2, 100, "unused");
    incoming.content = Some(Content::Image(vec![1, 2, 3]));
    incoming.hashes = hashes(incoming.content.as_ref().unwrap());
    model.event(Event::Submit(incoming));
    model.settle();
    let register = model.state.register.clone();
    model.event(Event::Share {
        token: 20,
        id: id(20),
        source: id(1),
        offer: offer(20, &[3]),
    });
    model.event(Event::ShareReady {
        token: 20,
        content: Some(Content::Image(vec![4, 5, 6])),
    });
    model.settle();
    assert_eq!(model.state.register, register);
    assert!(model.published.is_empty());
    assert!(model.replies[&20].1.is_some());
}

#[test]
fn marker_read_observation_keeps_recipients_frozen_across_policy_expansion() {
    let mut model = Compositor::default();
    // The macOS watcher must capture this before its marker subprocess, exactly
    // as Wayland captures policy before reading an offered secret-hint marker.
    model.event(Event::Observe {
        history: None,
        token: 20,
        id: id(20),
        offer: Some(offer(20, &[2])),
        origin: false,
        clear: false,
    });
    model.event(Event::Policy {
        allowed: offer(20, &[2, 3]).recipients,
    });
    model.event(Event::Read {
        token: 20,
        content: Some(content("human copy before peer 3 connected")),
        fingerprints: Vec::new(),
        debounce: false,
    });
    model.settle();
    assert_eq!(model.published.len(), 1);
    let recipients = &model.published[0].offer.as_ref().unwrap().recipients;
    assert_eq!(recipients.len(), 1);
    assert_eq!(recipients[0].target.device, id(2));
    assert!(model.state.idle());
}

#[test]
fn discarded_reader_then_manager_echo_releases_incoming_install_without_read_timeout() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "received")));
    model.settle();
    for token in [20, 30] {
        model.event(Event::Observe {
            history: None,
            token,
            id: id(token as u8),
            offer: Some(offer(token, &[2, 3])),
            origin: false,
            clear: false,
        });
    }
    assert_eq!(model.state.held_observations.len(), 1);
    // Reaping reader 20 must also retire its held actor observation. Completing
    // only the manager echo at 30 would retain 20 until the six-second timeout.
    model.event(Event::AbandonRead(20));
    model.event(Event::Read {
        token: 30,
        content: Some(content("received")),
        fingerprints: Vec::new(),
        debounce: false,
    });
    model.event(Event::Submit(remote(40, 3, 200, "next incoming")));
    model.settle(); // this model never fires ReadTimeout implicitly
    assert!(model.state.held_observations.is_empty());
    assert!(model.read_timeouts.is_empty());
    assert!(model.state.idle());
    assert_eq!(model.os, Some(content("next incoming")));
    assert!(model.published.is_empty());
    model.consistent();
}

#[test]
fn confirmed_marker_retires_provisional_observation_without_a_local_clock() {
    // A watcher may first see the selection after an authorized write,
    // including on restart. Initial marker reads carry no recipients.
    for initial in [false, true] {
        let mut model = Compositor::default();
        let mut operation = remote(10, 2, 100, "received");
        operation.sensitive = true;
        model.event(Event::Submit(operation));
        while !matches!(model.pending.front(), Some(Event::Installed(_))) {
            assert!(model.tick());
        }
        let ticket = model.state.installing.as_ref().unwrap().ticket;
        model.event(Event::Observe {
            history: None,
            token: 20,
            id: id(20),
            offer: (!initial).then(|| offer(20, &[2, 3])),
            origin: false,
            clear: false,
        });
        assert!(model.state.confirms_install(ticket));
        model.tick(); // independent watcher confirms the authorized install
        model.event(Event::AbandonRead(20));
        model.settle();
        assert!(model.read_timeouts.is_empty());
        assert!(model.state.idle());
        assert_eq!(model.state.register.as_ref().unwrap().hlc, 100);
        assert!(model.published.is_empty());
        model.consistent();
    }
}

#[test]
fn history_capture_follows_echo_classification_and_only_records_once() {
    let mut model = Compositor::default();
    model.event(Event::Submit(remote(10, 2, 100, "received")));
    model.settle();
    for (token, text, expected) in [(20, "received", false), (30, "new human copy", true)] {
        model.state.step(Event::Observe {
            token,
            id: id(token as u8),
            offer: None,
            origin: false,
            clear: false,
            history: Some(7),
        });
        let effects = model.state.step(Event::Read {
            token,
            content: Some(content(text)),
            fingerprints: vec![],
            debounce: true,
        });
        assert_eq!(
            effects.iter().any(|e| matches!(e,
            Effect::RecordHistory { content: Content::Text(t), generation: 7 } if t == text)),
            expected
        );
        assert!(
            !model
                .state
                .step(Event::Debounce(token))
                .iter()
                .any(|e| matches!(e, Effect::RecordHistory { .. }))
        );
    }
}
