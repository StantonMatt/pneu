use super::*;

impl Core {
    pub(super) fn pair_allows(&self, device: &Id, from: &Id, to: &Id, feature: &str) -> bool {
        if self.store.replacement_pending(device) {
            return false;
        }
        let key = format!("{from}/{to}/{feature}");
        self.store
            .pair_settings
            .lock()
            .expect("pair settings")
            .get(device)
            .and_then(|fields| fields.iter().find(|f| f.key == key))
            .is_none_or(|f| f.enabled)
    }
    pub(super) fn content_ready(&self, store: &Store, device: &Id) -> bool {
        !self.store.replacement_pending(device)
            && (!store.mesh
                || self
                    .settings_ready
                    .lock()
                    .expect("settings ready")
                    .get(device)
                    .is_some_and(|connection| Some(*connection) == store.connection_id))
    }
    pub(super) fn publish_settings_ready(&self, store: &Store, device: &Id, synchronized: bool) {
        // Settings persistence can await across a replacement. Publish only for
        // the current session, without letting an old completion hide its state.
        let sessions = self.sessions.lock().expect("sessions");
        if sessions
            .sessions
            .get(device)
            .is_none_or(|s| Some(s.id) != store.connection_id)
        {
            return;
        }
        let mut ready = self.settings_ready.lock().expect("settings ready");
        if synchronized {
            ready.insert(device.clone(), store.connection_id.expect("connection id"));
        } else if ready.get(device) == store.connection_id.as_ref() {
            ready.remove(device);
        }
    }
    pub(super) fn incoming_kind_allowed(&self, store: &Store, device: &Id, kind: FileKind) -> bool {
        if kind != FileKind::File && self.store.replacement_pending(device) {
            return false;
        }
        if !store.mesh {
            return true;
        }
        let feature = match kind {
            FileKind::ClipboardImage => "clipboard",
            FileKind::Screenshot | FileKind::Photo | FileKind::Recording => "automatic_media",
            FileKind::File => return true,
        };
        self.pair_allows(device, device, &self.identity.device_id, feature)
    }
    pub(super) async fn initialize_settings(
        &self,
        store: &Store,
        hello: &Hello,
    ) -> Result<Vec<PeerSetting>> {
        if !store.mesh {
            return Ok(Vec::new());
        }
        let _gate = self.settings_gate.lock().await;
        let fields = store
            .settings_fields(
                self.identity.device_id.clone(),
                hello.device_id.clone(),
                hello.role == Some(DeviceRole::Phone),
            )
            .await?;
        Ok(fields)
    }
    /// UI switch values include an individual pending flag and the target name.
    pub async fn peer_settings(&self, device: Id) -> Result<serde_json::Value> {
        let _gate = self.settings_gate.lock().await;
        let peer = self
            .store
            .peer_info()
            .await?
            .into_iter()
            .find(|p| p.id == device)
            .context("device is not paired")?;
        let can_sync = self.store.settings_can_sync(device.clone()).await?;
        let fields = if can_sync {
            self.store
                .settings_fields(
                    self.identity.device_id.clone(),
                    device.clone(),
                    peer.role == "phone",
                )
                .await?;
            self.store.settings_status(device.clone()).await?.into_iter().map(|(field,pending)|serde_json::json!({"key":field.key,"enabled":field.enabled,"hlc":field.hlc,"writer_device":field.writer_device,"pending":pending,"applies_when":if pending {Some(peer.name.clone())} else {None}})).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        Ok(serde_json::json!({"device":device,"can_sync":can_sync,"settings":fields}))
    }
    pub async fn set_peer_setting(
        &self,
        device: Id,
        key: String,
        enabled: bool,
    ) -> Result<serde_json::Value> {
        {
            let _gate = self.settings_gate.lock().await;
            let peer = self
                .store
                .peer_info()
                .await?
                .into_iter()
                .find(|p| p.id == device)
                .context("device is not paired")?;
            ensure!(
                self.store.settings_can_sync(device.clone()).await?,
                "update_required"
            );
            self.store
                .settings_edit(
                    self.identity.device_id.clone(),
                    device.clone(),
                    peer.role == "phone",
                    key,
                    enabled,
                )
                .await?;
            // Stop unsent clipboard work immediately when its direction closes.
            if !self.pair_allows(&device, &self.identity.device_id, &device, "clipboard") {
                self.cancel_automatic_clipboard(&device);
            }
        }
        self.wake(&device);
        self.peer_settings(device).await
    }
}
