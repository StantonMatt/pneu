//! SMS never uses Store receipts, Recent, the outbox or clipboard text sharing.
use super::*;
use crate::{
    actions::ClipboardInput,
    sms::{self, Target},
    ui::notifications::{Notice, escape},
};

impl Core {
    pub(super) fn sms_receive_policy(&self, device: &Id) -> Vec<PeerSetting> {
        self.store
            .pair_settings
            .lock()
            .expect("pair settings")
            .get(device)
            .into_iter()
            .flatten()
            .filter(|f| f.key == format!("{device}/{}/sms", self.identity.device_id))
            .cloned()
            .collect()
    }
    async fn copy_sms_text(
        &self,
        text: String,
        secret: bool,
        received_at: Option<u64>,
        authority: Option<(ActionScope, ActionOrigin)>,
        store: &Store,
    ) -> Result<bool> {
        let input = if secret {
            ClipboardInput::SecretText(sms::SecretText(text, received_at.is_some()))
        } else {
            ClipboardInput::Text(text)
        };
        let token = self.clipboard.token();
        let source = authority
            .as_ref()
            .map(|(_, origin)| origin.device.clone())
            .unwrap_or_else(|| self.identity.device_id.clone());
        let operation = ClipboardOperation {
            token,
            id: Id::new(crate::random_hex(16))?,
            source: source.clone(),
            key: received_at.map(|hlc| clipboard_actor::Key { hlc, source }),
            created_at: received_at.unwrap_or_else(crate::now_ms),
            payload: Some(Arc::new(clipboard_actor::Payload::Input(input.clone()))),
            hashes: Vec::new(),
            content: None,
            offer: None,
            received: true,
            legacy: false,
            restore: false,
            repair: false,
            observation: None,
        };
        let policy = authority
            .as_ref()
            .map(|(_, origin)| self.sms_receive_policy(&origin.device))
            .unwrap_or_default();
        let (reply, _) = tokio::sync::oneshot::channel();
        Ok(self
            .clipboard
            .submit(
                operation,
                clipboard_driver::Request {
                    sms: true,
                    input,
                    claim: None,
                    store: store.clone(),
                    authority,
                    kind: None,
                    policy,
                    reply,
                },
            )
            .await?
            .applied)
    }
    pub(super) fn remember_sms(
        &self,
        store: &Store,
        scope: &ActionScope,
        origin: &ActionOrigin,
        target: Target,
        message: Sms,
    ) -> Option<bool> {
        let mut state = self.sms.lock().expect("SMS");
        // Unpair cancels the scope and purges under this same mutex.
        if !scope.allows(origin) || !self.connection_current(store, &target.device) {
            return None;
        }
        Some(state.remember(target, message))
    }
    pub(super) async fn receive_sms(
        &self,
        store: &Store,
        device: &Id,
        mut message: Sms,
        scope: Option<&ActionScope>,
        tx: &mpsc::Sender<Message>,
    ) -> Result<()> {
        let _gate = self.sms_gate.lock().await;
        let Some(scope) = scope else {
            return Ok(());
        };
        let origin = ActionOrigin::captured(store, device);
        if !scope.allows(&origin) || !self.connection_current(store, device) {
            return Ok(());
        }
        if !self.pair_allows(device, device, &self.identity.device_id, "sms") {
            tx.send(nack(message.transfer_id, NackCode::UnsupportedKind))
                .await?;
            return Ok(());
        }
        let target = Target {
            device: device.clone(),
            id: message.transfer_id.clone(),
        };
        if !sms::fresh(message.received_at, crate::now_ms()) {
            tx.send(Message::Ack(Ack {
                transfer_id: message.transfer_id,
                deduplicated: true,
            }))
            .await?;
            return Ok(());
        }
        message.sender = sms::clean(&message.sender)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        message.text = sms::clean(&message.text);
        let Some(remembered) =
            self.remember_sms(store, scope, &origin, target.clone(), message.clone())
        else {
            return Ok(());
        };
        if !remembered {
            tx.send(Message::Ack(Ack {
                transfer_id: message.transfer_id,
                deduplicated: true,
            }))
            .await?;
            return Ok(());
        }
        let code = sms::detect(&message.text);
        let mut copied = false;
        let mut failed = false;
        if let Some(code) = &code {
            if self.config().copy_sms_codes && !self.actions.screen_locked().await {
                match self
                    .copy_sms_text(
                        code.digits.clone(),
                        true,
                        Some(message.received_at),
                        Some((scope.clone(), origin.clone())),
                        store,
                    )
                    .await
                {
                    Ok(applied) => copied = applied,
                    Err(_) => failed = true,
                }
            }
        }
        if !scope.allows(&origin)
            || !self.pair_allows(device, device, &self.identity.device_id, "sms")
        {
            return Ok(());
        }
        let source = self.arrival_source(device);
        let mut notice = Notice::new(if copied {
            format!("Code copied from {}", message.sender)
        } else if failed {
            format!("Couldn't copy code from {}", message.sender)
        } else {
            message.sender.clone()
        });
        notice.body = escape(&sms::preview(&sms::masked(&message.text, code.as_ref())));
        notice.sms_target = Some(target.clone());
        if code.is_some() {
            notice.actions.push(("sms-code", "Copy Code"));
        }
        notice.actions.push(("sms-text", "Copy Text"));
        let mut replaces = 0;
        if code.is_none() {
            let mut state = self.sms.lock().expect("SMS");
            let burst = state
                .bursts
                .entry(device.clone())
                .or_insert_with(|| sms::Burst {
                    started: std::time::Instant::now(),
                    count: 0,
                    senders: Default::default(),
                    notification: 0,
                });
            if burst.started.elapsed() >= std::time::Duration::from_secs(10) {
                *burst = sms::Burst {
                    started: std::time::Instant::now(),
                    count: 0,
                    senders: Default::default(),
                    notification: 0,
                };
            }
            burst.count += 1;
            if burst.senders.len() < 3 {
                burst.senders.insert(message.sender.clone());
            }
            if burst.count > 3 {
                notice = Notice::new(format!("{} more SMS", burst.count));
                notice.sms_target = Some(target.clone());
                let names = burst
                    .senders
                    .iter()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                notice.body = escape(&format!(
                    "From {names}{}",
                    if burst.senders.len() > 2 {
                        " and others".to_owned()
                    } else {
                        String::new()
                    }
                ));
                replaces = burst.notification;
            }
        }
        if self
            .ui_snapshot()
            .devices
            .iter()
            .filter(|d| d.role == "phone")
            .count()
            > 1
        {
            notice
                .summary
                .push_str(&format!(" · {}", sms::clean(&source.name)));
        }
        if let Ok(id) = self.actions.notify_sms(notice, replaces).await {
            if code.is_none() {
                if let Some(burst) = self.sms.lock().expect("SMS").bursts.get_mut(device) {
                    if burst.count > 3 {
                        burst.notification = id;
                    }
                }
            }
        }
        tx.send(Message::Ack(Ack {
            transfer_id: message.transfer_id,
            deduplicated: false,
        }))
        .await?;
        Ok(())
    }
    /// Notification targets are opaque IDs into this daemon's bounded RAM map.
    /// A cold/stale action cannot recover SMS content from notification history.
    pub(crate) async fn copy_sms(&self, target: Target, code_only: bool) {
        let scope = self
            .action_scope
            .lock()
            .expect("actions")
            .get(&target.device)
            .cloned();
        let Some(scope) = scope.filter(|scope| !*scope.cancelled.borrow()) else {
            return;
        };
        let message = self
            .sms
            .lock()
            .expect("SMS")
            .messages
            .iter()
            .find(|(old, _)| old == &target)
            .map(|(_, m)| m.clone());
        let Some(message) = message else {
            return;
        };
        if !self.pair_allows(
            &target.device,
            &target.device,
            &self.identity.device_id,
            "sms",
        ) {
            return;
        }
        let code = sms::detect(&message.text);
        let text = if code_only {
            let Some(code) = &code else {
                return;
            };
            code.digits.clone()
        } else {
            message.text
        };
        match self
            .copy_sms_text(
                text.clone(),
                code.is_some(),
                None,
                Some((scope, ActionOrigin::captured(&self.store, &target.device))),
                &self.store,
            )
            .await
        {
            Err(_) => {
                let _ = self
                    .actions
                    .notify_sms(Notice::new("Couldn't copy to clipboard"), 0)
                    .await;
            }
            Ok(true)
                if code.is_none()
                    && self.config().clipboard_mode == crate::config::ClipboardMode::Automatic =>
            {
                if let Some(offer) = self.automatic_target() {
                    let _ = self.send_observed_clipboard(offer, text).await;
                }
            }
            _ => {}
        }
    }
}
