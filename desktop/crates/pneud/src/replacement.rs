//! Trusted computer's local reinstall confirmation. Registration shares the
//! pairing gate, so the final offline check remains true through COMMIT.
use super::*;
use crate::outbox::Authority;

impl Core {
    pub async fn decide_replacement(
        &self,
        intent: Id,
        replace: bool,
        implied_old: Option<Authority>,
    ) -> Result<()> {
        let guard = self.pairing_gate.clone().lock_owned().await;
        let _settings = self.settings_gate.lock().await;
        let offer = self
            .store
            .replacements
            .lock()
            .expect("replacement offers")
            .iter()
            .find(|o| o.id == intent)
            .cloned()
            .context("replacement offer is no longer available")?;
        let old = implied_old.as_ref().unwrap_or(&offer.old).clone();
        if implied_old.is_some() {
            ensure!(
                replace && offer.choices.iter().any(|(a, _)| *a == old),
                "Pair Again choice no longer matches"
            );
        }
        if replace && self.connected_devices().contains(&old.device) {
            self.store
                .replacement_decide_selected(
                    self.identity.device_id.clone(),
                    offer,
                    false,
                    None,
                    self.setup_publish(guard, None, None),
                )
                .await?;
            bail!("The old device connected. Both pairings were kept.");
        }
        let new = offer.new.device.clone();
        self.store
            .replacement_decide_selected(
                self.identity.device_id.clone(),
                offer,
                replace,
                implied_old,
                self.setup_publish(guard, Some(old.device), None),
            )
            .await?;
        self.wake(&new);
        self.mesh.changed.notify_one();
        // Revocation already committed even if deferred spool cleanup fails.
        if let Err(error) = self.outbox.reconcile().await {
            self.error(None, format!("outbox cleanup deferred: {error}"));
        }
        let _ = self.events.send(Event::ConfigChanged);
        Ok(())
    }
}
