//! Durable pair registers and a recent receipt window. All merges share one
//! transaction with their receipt; evicted snapshots safely re-merge under LWW.
use super::*;
use pneu_proto::{PeerSetting, PeerSettings};

pub(crate) const SETTINGS_RECEIPT_WINDOW: i64 = 4096;

pub(crate) fn pair_keys(local: &Id, peer: &Id, phone: bool) -> Vec<String> {
    let mut keys = vec![
        format!("{local}/{peer}/clipboard"),
        format!("{peer}/{local}/clipboard"),
    ];
    if phone {
        keys.push(format!("{peer}/{local}/automatic_media"));
    }
    keys.sort();
    keys
}
pub(super) fn fields(c: &Connection, peer: &Id) -> Result<Vec<PeerSetting>> {
    let mut q = c.prepare(
        "SELECT key,enabled,hlc,writer_device FROM peer_settings WHERE device=? ORDER BY key",
    )?;
    q.query_map([peer.as_str()], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, bool>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?
    .map(|r| {
        let (key, enabled, hlc, writer) = r?;
        Ok(PeerSetting {
            key,
            enabled,
            hlc: hlc.parse()?,
            writer_device: Id::new(writer)?,
        })
    })
    .collect()
}
pub(super) fn initialize(tx: &Transaction<'_>, local: &Id, peer: &Id, phone: bool) -> Result<()> {
    ensure!(local != peer, SettingsUnexpected);
    for key in pair_keys(local, peer, phone) {
        tx.execute(
            "INSERT OR IGNORE INTO peer_settings VALUES(?,?,1,'0',?)",
            params![peer.as_str(), key, local.as_str().min(peer.as_str())],
        )?;
    }
    Ok(())
}
fn write_field(tx: &Transaction<'_>, peer: &Id, field: &PeerSetting) -> Result<()> {
    tx.execute("INSERT INTO peer_settings VALUES(?,?,?,?,?) ON CONFLICT(device,key) DO UPDATE SET enabled=excluded.enabled,hlc=excluded.hlc,writer_device=excluded.writer_device",params![peer.as_str(),field.key,field.enabled,field.hlc.to_string(),field.writer_device.as_str()])?;
    Ok(())
}
fn canonical(mut snapshot: PeerSettings) -> PeerSettings {
    snapshot.settings.sort_by(|a, b| a.key.cmp(&b.key));
    snapshot
}

impl Store {
    // Publish policy while holding the cache across COMMIT. A sender polling
    // its first byte can never observe a pre-commit value after persistence.
    fn settings_write<T>(
        &self,
        epoch: PeerEpoch,
        peer: &Id,
        f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.locked(|c| {
            let tx = c.transaction()?;
            epoch.check(&tx)?;
            let previous = fields(&tx, peer)?;
            let result = f(&tx)?;
            let state = fields(&tx, peer)?;
            let mut cache = self.pair_settings.lock().expect("pair settings");
            let changed = cache.get(peer) != Some(&state);
            let revoke = state.iter().any(|field| {
                field.key.ends_with(&format!("/{peer}/clipboard"))
                    && !field.enabled
                    && previous
                        .iter()
                        .any(|old| old.key == field.key && old.enabled)
            });
            tx.commit()?;
            if revoke {
                let mut generations = self
                    .settings_generations
                    .lock()
                    .expect("settings generations");
                let generation = generations.entry(peer.clone()).or_default();
                *generation = generation.wrapping_add(1);
            }
            cache.insert(peer.clone(), state);
            // The blocking continuation owns publication even if its async IPC
            // caller was canceled. A watch revision cannot lose a wake-up.
            if changed {
                self.settings_changed
                    .send_modify(|revision| *revision = revision.wrapping_add(1));
            }
            Ok(result)
        })
    }
    pub(crate) fn settings_generation(&self, peer: &Id) -> u64 {
        *self
            .settings_generations
            .lock()
            .expect("settings generations")
            .get(peer)
            .unwrap_or(&0)
    }
    pub(super) fn check_file_setting(
        &self,
        tx: &Transaction<'_>,
        peer: &Id,
        kind: pneu_proto::FileKind,
    ) -> Result<()> {
        if !self.mesh {
            return Ok(());
        }
        let local = self
            .local_device
            .as_ref()
            .context("missing local connection identity")?;
        let feature = match kind {
            pneu_proto::FileKind::ClipboardImage => "clipboard",
            pneu_proto::FileKind::Screenshot
            | pneu_proto::FileKind::Photo
            | pneu_proto::FileKind::Recording => "automatic_media",
            pneu_proto::FileKind::File => return Ok(()),
        };
        let enabled: bool = tx.query_row(
            "SELECT coalesce((SELECT enabled FROM peer_settings WHERE device=? AND key=?),1)",
            params![peer.as_str(), format!("{peer}/{local}/{feature}")],
            |r| r.get(0),
        )?;
        ensure!(enabled, SettingDisabled);
        Ok(())
    }
    #[cfg(test)]
    pub(crate) async fn settings_support(&self, peer: Id, supported: bool) -> Result<()> {
        let epoch = self.peer_epoch(&peer);
        self.blocking(move |s|s.peer_write(epoch,|tx| {tx.execute("INSERT INTO settings_support VALUES(?,?) ON CONFLICT(device) DO UPDATE SET supported=excluded.supported",params![peer.as_str(),supported])?;Ok(())})).await
    }
    pub(crate) async fn settings_can_sync(&self, peer: Id) -> Result<bool> {
        let epoch = self.peer_epoch(&peer);
        self.call(move |c| {
            epoch.check(c)?;
            Ok(c.query_row(
                "SELECT coalesce((SELECT supported FROM settings_support WHERE device=?),0)",
                [peer.as_str()],
                |r| r.get(0),
            )?)
        })
        .await
    }
    pub(crate) async fn settings_fields(
        &self,
        local: Id,
        peer: Id,
        phone: bool,
    ) -> Result<Vec<PeerSetting>> {
        let epoch = self.peer_epoch(&peer);
        self.blocking(move |s| {
            #[cfg(test)]
            s.fault("settings_fields")?;
            s.settings_write(epoch, &peer, |tx| {
                tx.execute("INSERT INTO settings_support VALUES(?,1) ON CONFLICT(device) DO UPDATE SET supported=1",[peer.as_str()])?;
                initialize(tx, &local, &peer, phone)?;
                fields(tx, &peer)
            })
        })
        .await
    }
    /// Replay pending operations first, then the acknowledged snapshot if its
    /// full stamped state is unchanged. Both payloads survive restart.
    pub(crate) async fn settings_outgoing(
        &self,
        local: Id,
        peer: Id,
        phone: bool,
    ) -> Result<PeerSettings> {
        let epoch = self.peer_epoch(&peer);
        self.blocking(move |s| {
            #[cfg(test)]
            s.fault("settings_outgoing")?;
            s.settings_write(epoch, &peer, |tx| {
                initialize(tx, &local, &peer, phone)?;
                if let Some(payload) = tx
                    .query_row(
                        "SELECT payload FROM settings_outgoing WHERE device=?",
                        [peer.as_str()],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                {
                    let snapshot: PeerSettings = serde_json::from_str(&payload)?;
                    pneu_proto::encode_frame(&pneu_proto::Message::PeerSettings(snapshot.clone()))?;
                    return Ok(snapshot);
                }
                let state = fields(tx, &peer)?;
                let retained = tx
                    .query_row(
                        "SELECT payload FROM settings_replay WHERE device=?",
                        [peer.as_str()],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .map(|payload| serde_json::from_str::<PeerSettings>(&payload))
                    .transpose()?;
                let snapshot = match retained {
                    Some(snapshot) if snapshot.settings == state => snapshot,
                    _ => PeerSettings {
                        transfer_id: Id::new(crate::random_hex(16))?,
                        settings: state,
                    },
                };
                pneu_proto::encode_frame(&pneu_proto::Message::PeerSettings(snapshot.clone()))?;
                tx.execute(
                    "INSERT INTO settings_outgoing VALUES(?,?)",
                    params![peer.as_str(), serde_json::to_string(&snapshot)?],
                )?;
                Ok(snapshot)
            })
        })
        .await
    }
    pub(crate) async fn settings_ack(&self, peer: Id, id: Id) -> Result<()> {
        let epoch = self.peer_epoch(&peer);
        self.blocking(move |s| {
            #[cfg(test)]
            s.fault("settings_ack")?;
            s.settings_write(epoch, &peer, |tx| {
                let payload: String = tx.query_row(
                    "SELECT payload FROM settings_outgoing WHERE device=?",
                    [peer.as_str()],
                    |r| r.get(0),
                )?;
                let snapshot: PeerSettings = serde_json::from_str(&payload)?;
                ensure!(snapshot.transfer_id == id, "wrong settings ACK ID");
                for f in snapshot.settings {
                    tx.execute("INSERT INTO settings_confirmed VALUES(?,?,?,?,?) ON CONFLICT(device,key) DO UPDATE SET enabled=excluded.enabled,hlc=excluded.hlc,writer_device=excluded.writer_device", params![peer.as_str(), f.key, f.enabled, f.hlc.to_string(), f.writer_device.as_str()])?;
                }
                tx.execute("INSERT INTO settings_replay VALUES(?,?) ON CONFLICT(device) DO UPDATE SET payload=excluded.payload", params![peer.as_str(), payload])?;
                tx.execute("DELETE FROM settings_outgoing WHERE device=?", [peer.as_str()])?;
                Ok(())
            })
        }).await
    }
    pub(crate) async fn settings_edit(
        &self,
        local: Id,
        peer: Id,
        phone: bool,
        key: String,
        enabled: bool,
    ) -> Result<Vec<PeerSetting>> {
        let epoch = self.peer_epoch(&peer);
        self.blocking(move |s| {
            s.settings_write(epoch, &peer, |tx| {
                ensure!(
                    pair_keys(&local, &peer, phone).contains(&key),
                    "setting does not belong to this pair"
                );
                initialize(tx, &local, &peer, phone)?;
                let current = fields(tx, &peer)?;
                if current.iter().any(|f| f.key == key && f.enabled == enabled) {
                    return Ok(current);
                }
                let hlc = allocate_stamp(tx)?.1;
                write_field(
                    tx,
                    &peer,
                    &PeerSetting {
                        key,
                        enabled,
                        hlc,
                        writer_device: local,
                    },
                )?;
                fields(tx, &peer)
            })
        })
        .await
    }
    /// Application validation precedes every mutation. The typed protocol codec
    /// has already checked grammar; only authenticated membership/roles remain.
    pub(crate) async fn settings_merge(
        &self,
        local: Id,
        peer: Id,
        phone: bool,
        snapshot: PeerSettings,
    ) -> Result<(bool, Vec<PeerSetting>)> {
        ensure!(local != peer, SettingsUnexpected);
        let epoch = self.peer_epoch(&peer);
        self.blocking(move |s| {
            #[cfg(test)]
            s.fault("settings_merge")?;
            s.settings_write(epoch, &peer, |tx| {
                let snapshot = canonical(snapshot);
                let keys: Vec<_> = snapshot.settings.iter().map(|f| f.key.clone()).collect();
                ensure!(keys == pair_keys(&local, &peer, phone), SettingsUnexpected);
                for f in &snapshot.settings {
                    f.validate().map_err(|_| SettingsMalformed)?;
                    validate_stamp(f.hlc, crate::now_ms()).map_err(|_| SettingsMalformed)?;
                }
                reject_partial_id(tx,&peer,&snapshot.transfer_id)?;
                let meta = Metadata::Settings(snapshot.clone());
                if let Some((old, _)) =
                    receipt_row(tx, peer.as_str(), snapshot.transfer_id.as_str())?
                {
                    ensure!(old == meta, ReceiptConflict);
                    return Ok((true, fields(tx, &peer)?));
                }
                let cached: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM failed_cache WHERE device=? AND id=?)",
                    params![peer.as_str(), snapshot.transfer_id.as_str()],
                    |r| r.get(0),
                )?;
                ensure!(!cached, ReceiptConflict);
                capacity(&s.folder, s.reserve.load(Ordering::SeqCst))?;
                initialize(tx, &local, &peer, phone)?;
                let old = fields(tx, &peer)?;
                for incoming in &snapshot.settings {
                    let current = old
                        .iter()
                        .find(|f| f.key == incoming.key)
                        .context("missing pair field")?;
                    let a = (incoming.hlc, incoming.writer_device.as_str());
                    let b = (current.hlc, current.writer_device.as_str());
                    ensure!(
                        a != b || incoming.enabled == current.enabled,
                        SettingsMalformed
                    );
                    observe_hlc(tx, incoming.hlc)?;
                    if a > b {
                        write_field(tx, &peer, incoming)?;
                    }
                }
                tx.execute(
                    "INSERT INTO settings_receipts VALUES(?,?,?)",
                    params![
                        peer.as_str(),
                        snapshot.transfer_id.as_str(),
                        serde_json::to_string(&meta)?
                    ],
                )?;
                // Keep the most recently recorded IDs, not a lifetime quota.
                // Replaying an evicted ID records it again and re-merges its
                // fields: strictly older tuples cannot undo newer state, and
                // equal tuples with conflicting values still fail atomically.
                // rowid order is durable and independent of untrusted clocks;
                // settings_receipts_sequence indexes (device, implicit rowid).
                tx.execute(
                    "DELETE FROM settings_receipts WHERE rowid IN (SELECT rowid FROM settings_receipts WHERE device=? ORDER BY rowid DESC LIMIT -1 OFFSET ?)",
                    params![peer.as_str(), SETTINGS_RECEIPT_WINDOW],
                )?;
                Ok((false, fields(tx, &peer)?))
            })
        })
        .await
    }
    pub(crate) async fn settings_status(&self, peer: Id) -> Result<Vec<(PeerSetting, bool)>> {
        let epoch = self.peer_epoch(&peer);
        self.call(move |c| {
            epoch.check(c)?;
            fields(c,&peer)?.into_iter().map(|f| {
                let confirmed:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM settings_confirmed WHERE device=? AND key=? AND enabled=? AND hlc=? AND writer_device=?)",params![peer.as_str(),f.key,f.enabled,f.hlc.to_string(),f.writer_device.as_str()],|r|r.get(0))?;
                Ok((f,!confirmed))
            }).collect()
        }).await
    }
}
#[derive(Debug)]
pub(crate) struct SettingsUnexpected;
impl std::fmt::Display for SettingsUnexpected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("settings do not name the authenticated pair and roles")
    }
}
impl std::error::Error for SettingsUnexpected {}

#[derive(Debug)]
pub(crate) struct SettingsMalformed;
impl std::fmt::Display for SettingsMalformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("malformed settings snapshot")
    }
}
impl std::error::Error for SettingsMalformed {}

#[derive(Debug)]
pub(super) struct SettingDisabled;
impl std::fmt::Display for SettingDisabled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("pair setting disabled")
    }
}
impl std::error::Error for SettingDisabled {}
