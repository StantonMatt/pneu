//! Setup control uses the same pinned READY sessions as pair settings. It never
//! adds a content recipient or forwards a received file/clipboard operation.
use super::*;
use crate::store::members::{MembersError, SetupDelta, SetupFence};
#[derive(Clone)]
pub(super) struct IncomingTarget {
    pub peer: Option<Id>,
    pub pin: Hash,
    pub cancelled: Arc<AtomicBool>,
    pub scope: bool,
}
pub(super) struct IncomingGuard {
    targets: Arc<Mutex<HashMap<u64, IncomingTarget>>>,
    id: u64,
}
impl Drop for IncomingGuard {
    fn drop(&mut self) {
        self.targets
            .lock()
            .expect("setup incoming")
            .remove(&self.id);
    }
}

impl Core {
    pub(super) fn track_setup_incoming(&self, id: u64, pin: Hash) -> IncomingGuard {
        self.setup_incoming.lock().expect("setup incoming").insert(
            id,
            IncomingTarget {
                peer: None,
                pin,
                cancelled: Arc::new(AtomicBool::new(false)),
                scope: self.setup_invitation_scope.load(Ordering::SeqCst),
            },
        );
        IncomingGuard {
            targets: self.setup_incoming.clone(),
            id,
        }
    }
    pub async fn set_pairing_scope(&self, enabled: bool) -> Result<()> {
        let _guard = self.pairing_gate.lock().await;
        ensure!(
            self.admission
                .lock()
                .expect("admission")
                .invitation
                .as_ref()
                .is_some_and(Invitation::is_active),
            "no active invitation"
        );
        self.setup_invitation_scope.store(enabled, Ordering::SeqCst);
        Ok(())
    }
    pub async fn dismiss_setup_notice(&self, device: Id, generation: u64) -> Result<()> {
        self.store
            .call(move |c| {
                c.execute(
                    "DELETE FROM setup_notices WHERE device=? AND generation=?",
                    rusqlite::params![device.as_str(), generation as i64],
                )?;
                Ok(())
            })
            .await
    }
    pub(super) fn self_member(&self) -> Member {
        Member {
            device_id: self.identity.device_id.clone(),
            fingerprint: self.identity.fingerprint.clone(),
            role: DeviceRole::Computer,
            name: self.config().name,
            status: MemberStatus::Member,
            hlc: 0,
            writer_device: self.identity.device_id.clone(),
            reach: Some(MemberReach {
                addresses: self.computer_hello().addresses.unwrap_or_default(),
                relay: Vec::new(),
                hlc: 0,
                writer_device: self.identity.device_id.clone(),
            }),
        }
    }
    pub(super) fn setup_fence(&self, store: &Store, hello: &Hello) -> Result<SetupFence> {
        let current = self
            .action_scope
            .lock()
            .expect("actions")
            .get(&hello.device_id)
            .context("revoked binding")?
            .registration_generation
            .clone();
        let pin = self
            .admission
            .lock()
            .expect("admission")
            .peers
            .iter()
            .find(|(_, id)| **id == hello.device_id)
            .map(|(pin, _)| pin.clone())
            .context("revoked binding")?;
        Ok(SetupFence {
            generation: store
                .setup_generation
                .unwrap_or_else(|| self.store.setup_state().generation),
            binding_generation: store
                .pairing_generation(&hello.device_id)
                .context("missing binding generation")?,
            device: hello.device_id.clone(),
            fingerprint: pin,
            connection: store.connection_id.context("missing session")?,
            current,
        })
    }
    pub(super) fn setup_publish(
        &self,
        guard: tokio::sync::OwnedMutexGuard<()>,
        unpair: Option<Id>,
        final_ack: Option<(Id, Id)>,
    ) -> impl FnOnce(SetupDelta) + Send + 'static {
        let store = self.store.clone();
        let admission = self.admission.clone();
        let scopes = self.action_scope.clone();
        let sessions = self.sessions.clone();
        let clipboard = self.clipboard.clone();
        let automatic = self.automatic.clone();
        let events = self.events.clone();
        let ui = self.ui.clone();
        let changed = self.mesh.changed.clone();
        let mesh = self.mesh.clone();
        let incoming = self.setup_incoming.clone();
        move |delta| {
            let _guard = guard;
            mesh.cancel_removed(&delta.tombstones);
            if delta.withdrawn {
                mesh.cancel_withdrawn_join();
            }
            for target in incoming.lock().expect("setup incoming").values() {
                if delta.withdrawn
                    || delta.tombstones.iter().any(|(id, pin)| {
                        target.pin == *pin && target.peer.as_ref().is_none_or(|p| p == id)
                    })
                {
                    target.cancelled.store(true, Ordering::SeqCst);
                }
            }
            let mut connections = sessions.lock().expect("sessions");
            let mut scopes = scopes.lock().expect("actions");
            let mut admission = admission.lock().expect("admission");
            for id in delta.removed {
                clipboard.event(ClipboardEvent::Revoke(id.clone()));
                if let Some(scope) = scopes.remove(&id) {
                    scope.cancelled.send_replace(true);
                }
                admission.peers.retain(|_, p| *p != id);
                if let Some(session) = connections.sessions.remove(&id) {
                    session.close.send_replace(Some(
                        if delta.withdrawn
                            && final_ack.as_ref().is_some_and(|(peer, _)| peer == &id)
                        {
                            CloseReason::MembershipAck(
                                final_ack.as_ref().expect("withdrawal receipt").1.to_bytes(),
                            )
                        } else if unpair.as_ref() == Some(&id) {
                            CloseReason::Unpair
                        } else {
                            CloseReason::Error(ErrorCode::Closing)
                        },
                    ));
                }
                automatic
                    .lock()
                    .expect("automatic clipboard")
                    .invalidate_peer(&id);
                clipboard.event(ClipboardEvent::BindingReset(id.clone()));
                emit_event(&ui, &events, Event::Unpaired { device: id });
            }
            let setup = store.setup_state();
            for id in delta.added {
                if let Some(m) = setup
                    .roster
                    .iter()
                    .find(|m| m.device_id == id && m.status == MemberStatus::Member)
                {
                    admission.peers.insert(m.fingerprint.clone(), id.clone());
                    scopes.insert(id.clone(), ActionScope::new());
                    emit_event(&ui, &events, Event::Paired { device: id });
                }
            }
            if let Some(device) = delta.reconnect_peer
                && let Some(session) = connections.sessions.get(&device)
            {
                session
                    .close
                    .send_replace(Some(CloseReason::Error(ErrorCode::Closing)));
            }
            if delta.reconnect && !delta.withdrawn {
                // Only links whose advertised setup ID changes need a fresh
                // HELLO. Unscoped content connections remain usable.
                for session in connections.sessions.values() {
                    if session.capabilities.iter().any(|c| c == "setup")
                        && setup
                            .links
                            .iter()
                            .any(|l| l.device == session.device && l.scope != SetupScope::Disabled)
                    {
                        session
                            .close
                            .send_replace(Some(CloseReason::Error(ErrorCode::Closing)));
                    }
                }
            }
            select_display(&connections, &admission, &mut ui.lock().expect("UI state"));
            if delta.changed {
                let _ = events.send(Event::ConfigChanged);
                changed.notify_waiters();
            }
        }
    }
    pub(super) async fn reconcile_setup_candidates(&self) -> Result<()> {
        let count = self
            .store
            .call(|c| {
                Ok(
                    c.query_row("SELECT count(*) FROM setup_candidates", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                )
            })
            .await?;
        if count == 0 {
            return Ok(());
        }
        let guard = self.pairing_gate.clone().lock_owned().await;
        self.store
            .setup_reconcile_candidates(
                self.identity.device_id.clone(),
                self.setup_attempts(),
                self.setup_publish(guard, None, None),
            )
            .await
    }
    pub(super) async fn setup_ready(
        &self,
        store: &Store,
        hello: &Hello,
        sent: Option<(SetupHello, u64)>,
    ) -> Result<Option<SetupFence>> {
        if !hello.capabilities.iter().any(|c| c == "setup") {
            if !self
                .store
                .setup_state()
                .links
                .iter()
                .any(|l| l.device == hello.device_id && l.negotiated)
            {
                return Ok(None);
            }
            let _guard = self.pairing_gate.lock().await;
            if self.connection_current(store, &hello.device_id)
                && self
                    .store
                    .setup_state()
                    .links
                    .iter()
                    .any(|l| l.device == hello.device_id && l.negotiated)
            {
                self.store
                    .setup_legacy_ready(self.setup_fence(store, hello)?)
                    .await?;
            }
            return Ok(None);
        }
        let remote = hello.setup.clone().context("missing setup HELLO")?;
        let guard = self.pairing_gate.clone().lock_owned().await;
        ensure!(
            self.connection_current(store, &hello.device_id),
            "obsolete setup session"
        );
        let fence = self.setup_fence(store, hello)?;
        let current = self
            .store
            .setup_hello(Some(&hello.device_id), SetupScope::Enabled);
        if sent
            .as_ref()
            .is_none_or(|(h, g)| *g != fence.generation || *h != current)
        {
            self.reconnect_setup_peer(&hello.device_id);
            return Ok(None);
        }
        let peer = Member {
            device_id: hello.device_id.clone(),
            fingerprint: fence.fingerprint.clone(),
            role: hello.role.context("missing peer role")?,
            name: hello.name.clone(),
            status: MemberStatus::Member,
            hlc: 0,
            writer_device: self.identity.device_id.clone(),
            reach: None,
        };
        let matched = self
            .store
            .setup_activate(
                fence.clone(),
                self.self_member(),
                peer,
                remote,
                self.setup_publish(guard, None, None),
            )
            .await?;
        Ok(matched.then_some(fence))
    }
    pub(super) async fn receive_members(
        &self,
        store: &Store,
        hello: &Hello,
        fence: Option<&SetupFence>,
        snapshot: Members,
    ) -> Result<Message> {
        ensure!(
            hello.capabilities.iter().any(|c| c == "setup"),
            "unexpected MEMBERS without setup"
        );
        let id = snapshot.transfer_id.clone();
        let separate = self
            .store
            .setup_state()
            .setup_id
            .as_ref()
            .is_some_and(|local| *local != snapshot.setup_id);
        if separate {
            self.error(
                Some(hello.device_id.clone()),
                "Direct link kept; separate setups not linked",
            );
        }
        let Some(fence) = fence else {
            return Ok(nack(id, NackCode::UnsupportedKind));
        };
        let guard = self.pairing_gate.clone().lock_owned().await;
        ensure!(
            self.connection_current(store, &hello.device_id),
            "obsolete MEMBERS session"
        );
        let final_ack = Some((hello.device_id.clone(), snapshot.transfer_id.clone()));
        let result = self
            .store
            .members_merge(
                fence.clone(),
                self.self_member(),
                snapshot,
                self.setup_attempts(),
                self.setup_publish(guard, None, final_ack),
            )
            .await;
        match result {
            Ok(deduplicated) => Ok(Message::Ack(Ack {
                transfer_id: id,
                deduplicated,
            })),
            Err(e) if e.is::<MembersError>() => match e.downcast_ref::<MembersError>().unwrap() {
                MembersError::Unsupported => Ok(nack(id, NackCode::UnsupportedKind)),
                MembersError::Malformed => Err(crate::store::SettingsMalformed.into()),
                MembersError::Denied => Err(e),
                MembersError::Capacity => {
                    self.error(Some(hello.device_id.clone()), "setup capacity exceeded");
                    Ok(nack(id, NackCode::StorageFailed))
                }
            },
            Err(e) => Ok(nack(id, crate::store::save_failure_code(&e))),
        }
    }
    pub async fn setup_status(&self) -> Result<serde_json::Value> {
        let state = self.store.setup_state();
        let mut value = serde_json::to_value(&state)?;
        value["binding_generations"] = serde_json::to_value(
            state
                .links
                .iter()
                .map(|l| {
                    (
                        l.device.to_string(),
                        self.store.pairing_generation(&l.device),
                    )
                })
                .collect::<HashMap<_, _>>(),
        )?;
        let (notices, conflicts) = self
            .store
            .call(|c| {
                let read = |table: &str| -> Result<Vec<serde_json::Value>> {
                    let mut q = c.prepare(&format!("SELECT payload FROM {table}"))?;
                    q.query_map([], |r| r.get::<_, String>(0))?
                        .map(|r| Ok(serde_json::from_str(&r?)?))
                        .collect()
                };
                let notices = {
                    let mut q = c.prepare("SELECT n.payload,n.generation FROM setup_notices n JOIN peers p ON p.id=n.device AND p.generation=n.generation")?;
                    q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                        .map(|r| {
                            let (payload, generation) = r?;
                            let mut value: serde_json::Value = serde_json::from_str(&payload)?;
                            value["binding_generation"] = serde_json::json!(generation as u64);
                            Ok(value)
                        }).collect::<Result<Vec<_>>>()?
                };
                Ok((notices, read("setup_conflicts")?))
            })
            .await?;
        if let Some(device) = &state.waiting_for {
            let name = self
                .store
                .peer_info()
                .await?
                .into_iter()
                .find(|p| &p.id == device)
                .map(|p| p.name)
                .unwrap_or_else(|| device.to_string());
            value["waiting"] =
                serde_json::json!({"device":device,"message":format!("waiting for {name}")});
        }
        value["notices"] = serde_json::to_value(notices)?;
        value["conflicts"] = serde_json::to_value(conflicts)?;
        Ok(value)
    }
    fn setup_publish_peer(
        &self,
        guard: tokio::sync::OwnedMutexGuard<()>,
        device: &Id,
    ) -> impl FnOnce(SetupDelta) + Send + 'static {
        let close = self
            .sessions
            .lock()
            .expect("sessions")
            .sessions
            .get(device)
            .map(|s| s.close.clone());
        let publish = self.setup_publish(guard, None, None);
        move |delta| {
            if let Some(close) = close {
                close.send_replace(Some(CloseReason::Error(ErrorCode::Closing)));
            }
            publish(delta);
        }
    }
    pub async fn link_existing_device(&self, device: Id) -> Result<()> {
        let guard = self.pairing_gate.clone().lock_owned().await;
        let connections = self
            .sessions
            .lock()
            .expect("sessions")
            .sessions
            .iter()
            .map(|(id, s)| (id.clone(), s.id))
            .collect();
        self.store
            .setup_begin_link(
                device.clone(),
                connections,
                self.setup_publish_peer(guard, &device),
            )
            .await
    }
    pub async fn decide_setup(
        &self,
        device: Id,
        generation: u64,
        binding: u64,
        allow: bool,
    ) -> Result<()> {
        let guard = self.pairing_gate.clone().lock_owned().await;
        let refresh = self
            .sessions
            .lock()
            .expect("sessions")
            .sessions
            .get(&device)
            .map(|s| s.id);
        self.store
            .setup_decide_then(
                device.clone(),
                generation,
                binding,
                allow,
                refresh,
                self.setup_publish_peer(guard, &device),
            )
            .await
    }
    fn reconnect_setup_peer(&self, device: &Id) {
        if let Some(session) = self.sessions.lock().expect("sessions").sessions.get(device) {
            session
                .close
                .send_replace(Some(CloseReason::Error(ErrorCode::Closing)));
        }
        self.mesh.changed.notify_waiters();
    }
    pub async fn remove_setup_device(&self, device: Id) -> Result<()> {
        let state = self.store.setup_state();
        let member = state
            .roster
            .iter()
            .find(|m| m.device_id == device && m.status == MemberStatus::Member)
            .context("not an active setup member")?;
        let binding = self.store.setup_binding_generation(&device);
        self.remove_setup_device_confirmed(
            device,
            state.generation,
            binding,
            member.fingerprint.clone(),
        )
        .await
    }
    pub async fn remove_setup_device_confirmed(
        &self,
        device: Id,
        generation: u64,
        binding: Option<u64>,
        fingerprint: Hash,
    ) -> Result<()> {
        let guard = self.pairing_gate.clone().lock_owned().await;
        let state = self.store.setup_state();
        ensure!(
            state.generation == generation
                && self.store.setup_binding_generation(&device) == binding
                && state.roster.iter().any(|m| m.device_id == device
                    && m.fingerprint == fingerprint
                    && m.status == MemberStatus::Member),
            "stale setup removal confirmation"
        );
        self.store
            .setup_remove(
                self.identity.device_id.clone(),
                device.clone(),
                self.setup_publish(guard, Some(device), None),
            )
            .await?;
        self.outbox.reconcile().await?;
        Ok(())
    }
    pub(super) async fn withdraw_setup(&self, store: &Store, hello: &Hello) -> Result<bool> {
        let guard = self.pairing_gate.clone().lock_owned().await;
        ensure!(
            self.connection_current(store, &hello.device_id),
            "obsolete UNPAIR"
        );
        if !self.store.setup_scoped(&hello.device_id) {
            return Ok(false);
        }
        let fence = self.setup_fence(store, hello)?;
        self.store
            .setup_withdraw(fence, self.setup_publish(guard, None, None))
            .await?;
        self.outbox.reconcile().await?;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
