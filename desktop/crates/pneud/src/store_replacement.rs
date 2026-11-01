//! Local pairing UI intents. Nothing in this module adds wire authority.
use super::*;
use outbox::Authority;
use unicode_casefold::UnicodeCaseFold;

pub(crate) const REPLACED: &str = "Old device replaced";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub id: Id,
    pub old: Authority,
    pub new: Authority,
    pub name: String,
    pub role: String,
    pub last_seen: u64,
    /// All eligible bindings at this fresh pairing; only Pair Again can select an alternative.
    pub choices: Vec<(Authority, u64)>,
}

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS local_pairing_intents(id TEXT PRIMARY KEY,old_device TEXT NOT NULL,new_device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,payload TEXT NOT NULL,phase TEXT NOT NULL,settings TEXT);
        CREATE TRIGGER IF NOT EXISTS local_pairing_intent_end AFTER DELETE ON peers BEGIN DELETE FROM local_pairing_intents WHERE old_device=OLD.id AND phase<>'applying' AND phase<>'done'; END;")?;
    Ok(())
}

pub(super) fn pending(c: &Connection) -> Result<Vec<Offer>> {
    let mut q =
        c.prepare("SELECT payload FROM local_pairing_intents WHERE phase='offer' ORDER BY rowid")?;
    q.query_map([], |r| r.get::<_, String>(0))?
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect()
}

fn normalized(name: &str) -> String {
    name.trim().case_fold().collect()
}

fn binding(c: &Connection, device: &Id) -> Result<(Authority, String, String)> {
    let (pin, generation, name, role): (String, i64, String, String) = c.query_row(
        "SELECT fp,generation,name,role FROM peers WHERE id=?",
        [device.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok((
        Authority {
            device: device.clone(),
            generation: generation as u64,
            fingerprint: Hash::new(pin)?,
        },
        name,
        role,
    ))
}

/// Called inside the fresh-pair commit, while Core owns the pairing gate.
/// Introductions and reconnects never create a suggestion.
pub(super) fn paired(tx: &Transaction<'_>, new: &Id, offline: &[(Id, u64)]) -> Result<()> {
    let (new, name, role) = binding(tx, new)?;
    let mut candidates = Vec::new();
    for (id, last_seen) in offline {
        if id == &new.device {
            continue;
        }
        let Ok((old, old_name, old_role)) = binding(tx, id) else {
            continue;
        };
        if role == old_role
            && !normalized(&name).is_empty()
            && normalized(&name) == normalized(&old_name)
        {
            candidates.push((old, *last_seen));
        }
    }
    candidates.sort_by(|(a, at), (b, bt)| {
        bt.cmp(at)
            .then_with(|| a.device.as_str().cmp(b.device.as_str()))
    });
    if let Some((old, last_seen)) = candidates.first().cloned() {
        let offer = Offer {
            id: Id::new(crate::random_hex(16))?,
            old,
            new,
            name,
            role,
            last_seen,
            choices: candidates,
        };
        tx.execute("INSERT INTO local_pairing_intents(id,old_device,new_device,payload,phase) VALUES(?,?,?,?,'offer')",
            params![offer.id.as_str(),offer.old.device.as_str(),offer.new.device.as_str(),serde_json::to_string(&offer)?])?;
    }
    Ok(())
}

impl Store {
    pub(crate) fn replacement_pending(&self, device: &Id) -> bool {
        self.replacements
            .lock()
            .expect("replacement offers")
            .iter()
            .any(|o| &o.new.device == device)
    }

    /// The intent, fresh ordinary edits, cancellation and existing removal are
    /// one SQLite transaction. A crash either rolls all of them back (keeping
    /// both pairs and the offer) or recovers all of them; no cross-store phase.
    #[cfg(test)]
    pub(crate) async fn replacement_decide(
        &self,
        local: Id,
        offer: Offer,
        replace: bool,
        finish: impl FnOnce(members::SetupDelta) + Send + 'static,
    ) -> Result<()> {
        self.replacement_decide_selected(local, offer, replace, None, finish)
            .await
    }
    pub(crate) async fn replacement_decide_selected(
        &self,
        local: Id,
        offer: Offer,
        replace: bool,
        selected: Option<Authority>,
        finish: impl FnOnce(members::SetupDelta) + Send + 'static,
    ) -> Result<()> {
        self.setup_write(move |tx, delta| {
            let raw: String = tx.query_row("SELECT payload FROM local_pairing_intents WHERE id=? AND phase='offer'", [offer.id.as_str()], |r| r.get(0))?;
            ensure!(serde_json::from_str::<Offer>(&raw)? == offer, "stale replacement confirmation");
            let mut offer = offer;
            if let Some(selected) = selected {
                let (_, seen) = offer.choices.iter().find(|(a, _)| *a == selected).context("Pair Again choice no longer matches")?;
                offer.last_seen = *seen;
                offer.old = selected;
            }
            let (new, name, role) = binding(tx, &offer.new.device)?;
            ensure!(new == offer.new, "new pairing changed");
            if !replace {
                tx.execute("UPDATE local_pairing_intents SET phase='declined' WHERE id=?", [offer.id.as_str()])?;
                return Ok(());
            }
            let (old, old_name, old_role) = binding(tx, &offer.old.device)?;
            ensure!(old == offer.old && old != new && role == old_role && role == offer.role
                && normalized(&name) == normalized(&old_name) && normalized(&name) == normalized(&offer.name), "replacement identities changed");
            let values = settings::fields(tx, &old.device)?;
            let edits = settings::pair_keys(&local, &new.device, role == "phone").into_iter().map(|key| {
                let old_key = key.replace(new.device.as_str(), old.device.as_str());
                let enabled = values.iter().find(|v| v.key == old_key).is_none_or(|v| v.enabled);
                Ok(pneu_proto::PeerSetting { key, enabled, hlc: allocate_stamp(tx)?.1, writer_device: local.clone() })
            }).collect::<Result<Vec<_>>>()?;
            tx.execute("UPDATE local_pairing_intents SET phase='applying',settings=?,old_device=?,payload=? WHERE id=?", params![serde_json::to_string(&edits)?,offer.old.device.as_str(),serde_json::to_string(&offer)?,offer.id.as_str()])?;
            for field in &edits { settings::write_field(tx, &new.device, field)?; }
            delta.settings.push((new.device.clone(), edits));
            // A retained default snapshot must not be emitted after the edits.
            tx.execute("DELETE FROM settings_outgoing WHERE device=?", [new.device.as_str()])?;
            tx.execute("DELETE FROM settings_replay WHERE device=?", [new.device.as_str()])?;
            if members::state(tx)?.roster.iter().any(|m| m.device_id == old.device && m.status == pneu_proto::MemberStatus::Member) {
                members::remove(tx, &local, &old.device, None, delta)?;
            } else {
                remove_binding(tx, &old.device)?;
                delta.removed.push(old.device.clone());
            }
            tx.execute("UPDATE outbox_deliveries SET error=? WHERE device=? AND generation=? AND fingerprint=? AND state='cancelled' AND error='Pairing changed'",
                params![REPLACED, old.device.as_str(), old.generation as i64, old.fingerprint.as_str()])?;
            tx.execute("UPDATE local_pairing_intents SET phase='done' WHERE id=?", [offer.id.as_str()])?;
            Ok(())
        }, finish).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pneu_proto::{DeviceRole, Hello, SetupScope};

    fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }
    fn pin(n: u8) -> Hash {
        Hash::new(format!("{n:064x}")).unwrap()
    }
    async fn pair(
        s: &Store,
        n: u8,
        role: DeviceRole,
        name: &str,
        offline: Vec<(Id, u64)>,
    ) -> Result<()> {
        s.pair_then_hello_replacement(
            id(n),
            pin(n),
            Some(Hello {
                version: 1,
                device_id: id(n),
                name: name.into(),
                capabilities: vec!["mesh".into()],
                role: Some(role),
                addresses: None,
                setup: None,
                paused: None,
            }),
            None,
            SetupScope::Disabled,
            false,
            offline,
            || {},
        )
        .await
    }
    async fn fixture() -> Result<(tempfile::TempDir, Store)> {
        let dir = crate::test_support::scratch()?;
        let s = Store::open(&dir.path().join("state.sqlite3"))?;
        pair(&s, 1, DeviceRole::Phone, " Galaxy S25+ ", vec![]).await?;
        for (key, enabled) in [
            (format!("{}/{}/clipboard", id(9), id(1)), false),
            (format!("{}/{}/clipboard", id(1), id(9)), true),
            (format!("{}/{}/automatic_media", id(1), id(9)), false),
        ] {
            s.settings_edit(id(9), id(1), true, key, enabled).await?;
        }
        Ok((dir, s))
    }
    fn offer(s: &Store) -> Offer {
        s.replacements.lock().unwrap()[0].clone()
    }

    #[tokio::test]
    async fn replacement_copies_fresh_ordinary_edits_cancels_without_migration_and_retry_uses_new_id()
    -> Result<()> {
        let (dir, s) = fixture().await?;
        let old_hlc = s
            .settings_fields(id(9), id(1), true)
            .await?
            .iter()
            .map(|f| f.hlc)
            .max()
            .unwrap();
        let q = outbox::Outbox::open(s.clone(), &dir.path().join("outbox"))?;
        let source = dir.path().join("report.png");
        fs::write(&source, b"immutable old queue")?;
        let old = Authority {
            device: id(1),
            generation: 1,
            fingerprint: pin(1),
        };
        let artifact = q
            .admit(id(8), source.clone(), vec![old], outbox::PEER_BYTES)
            .await?;
        pair(&s, 2, DeviceRole::Phone, "galaxy s25+", vec![(id(1), 42)]).await?;
        let o = offer(&s);
        assert!(s.replacement_pending(&id(2)));
        s.replacement_decide(id(9), o.clone(), true, |delta| {
            assert_eq!(delta.removed, vec![id(1)])
        })
        .await?;
        assert!(!s.peers().await?.values().any(|p| *p == id(1)));
        assert_eq!(s.pairing_generation(&id(2)), Some(1));
        assert!(!s.replacement_pending(&id(2)));
        let cached = s
            .pair_settings
            .lock()
            .unwrap()
            .get(&id(2))
            .cloned()
            .context("replacement policy published at commit")?;
        assert_eq!(cached.len(), 3);
        assert!(
            cached.iter().all(|f| f.hlc > old_hlc),
            "copying allocates fresh stamps before releasing content"
        );
        let fields = s.settings_fields(id(9), id(2), true).await?;
        assert_eq!(fields.len(), 3, "only clipboard and photos copy");
        for field in &fields {
            assert_eq!(field.writer_device, id(9));
            assert!(field.hlc > 0);
            assert!(field.key.contains(id(2).as_str()) && !field.key.contains(id(1).as_str()));
            assert_eq!(
                field.enabled,
                field.key == format!("{}/{}/clipboard", id(2), id(9))
            );
        }
        assert_eq!(
            fields.iter().map(|f| f.hlc).collect::<HashSet<_>>().len(),
            3
        );
        q.reconcile().await?;
        let rows = q.snapshot(&[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device, id(1));
        assert_eq!(rows[0].state, "cancelled");
        assert_eq!(rows[0].error.as_deref(), Some(REPLACED));
        assert!(
            q.claim(id(2), 1, 1).await?.is_none(),
            "Replace never enqueues to the new identity"
        );
        // Retry survives process restart and does not reread the mutable source.
        fs::write(source, b"changed source")?;
        s.drain().await;
        drop((q, s));
        let s = Store::open(&dir.path().join("state.sqlite3"))?;
        let q = outbox::Outbox::open(s.clone(), &dir.path().join("outbox"))?;
        let new = Authority {
            device: id(2),
            generation: 1,
            fingerprint: pin(2),
        };
        // New work is already queued and newer clipboard activity has reached
        // the replacement. Retry must follow both, rather than replay old order.
        let newer_source = dir.path().join("newer.png");
        fs::write(&newer_source, b"newer image")?;
        let newer = q
            .admit(id(7), newer_source, vec![new.clone()], outbox::PEER_BYTES)
            .await?;
        let (newer_at, newer_hlc) = s.local_clipboard(id(99)).await?;
        assert_eq!(
            q.retry_replaced(
                id(8),
                Some(artifact.clone()),
                id(1),
                vec![Authority {
                    device: id(2),
                    generation: 1,
                    fingerprint: pin(2)
                }],
                id(99),
            )
            .await?,
            Some(id(2))
        );
        let first = q
            .claim(id(2), 1, 2)
            .await?
            .context("newer work stays first")?;
        assert_eq!(first.begin.transfer_id, newer);
        q.receipt(first, None).await?;
        let sent = q.claim(id(2), 1, 2).await?.context("retry queued")?;
        assert_ne!(sent.begin.transfer_id, artifact);
        assert!(sent.begin.hlc.context("fresh retry HLC")? > newer_hlc);
        assert!(sent.begin.created_at > newer_at);
        let retried = q
            .snapshot(&[])
            .into_iter()
            .find(|r| r.transfer_id == sent.begin.transfer_id)
            .context("fresh operation visible")?;
        assert_ne!(retried.operation, id(8));
        assert_eq!(sent.begin.size, b"immutable old queue".len() as u64);
        assert_eq!(
            sent.begin.sha256,
            Hash::new(hex::encode(Sha256::digest(b"immutable old queue")))?
        );
        assert!(q.claim(id(1), 1, 3).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn replacement_decline_persists_and_keeps_both_and_queue() -> Result<()> {
        let (dir, s) = fixture().await?;
        let q = outbox::Outbox::open(s.clone(), &dir.path().join("outbox"))?;
        let source = dir.path().join("waiting.txt");
        fs::write(&source, b"still queued")?;
        q.admit(
            id(8),
            source,
            vec![Authority {
                device: id(1),
                generation: 1,
                fingerprint: pin(1),
            }],
            outbox::PEER_BYTES,
        )
        .await?;
        pair(&s, 2, DeviceRole::Phone, "galaxy s25+", vec![(id(1), 42)]).await?;
        s.replacement_decide(id(9), offer(&s), false, |d| assert!(d.removed.is_empty()))
            .await?;
        assert_eq!(s.peers().await?.len(), 2);
        assert!(s.replacements.lock().unwrap().is_empty());
        q.reconcile().await?;
        assert_eq!(q.snapshot(&[])[0].state, "waiting_for_device");
        s.drain().await;
        drop((q, s));
        let s = Store::open(&dir.path().join("state.sqlite3"))?;
        assert!(s.replacements.lock().unwrap().is_empty());
        assert_eq!(s.peers().await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn replacement_exact_pair_again_choice_and_other_recipient_survive() -> Result<()> {
        let (dir, s) = fixture().await?;
        pair(&s, 4, DeviceRole::Phone, "galaxy s25+", vec![]).await?;
        let q = outbox::Outbox::open(s.clone(), &dir.path().join("outbox"))?;
        let source = dir.path().join("shared.txt");
        fs::write(&source, b"shared bytes")?;
        let old = Authority {
            device: id(1),
            generation: 1,
            fingerprint: pin(1),
        };
        let sibling = Authority {
            device: id(4),
            generation: 1,
            fingerprint: pin(4),
        };
        let artifact = q
            .admit(
                id(8),
                source,
                vec![old.clone(), sibling],
                outbox::PEER_BYTES,
            )
            .await?;
        pair(
            &s,
            2,
            DeviceRole::Phone,
            "galaxy s25+",
            vec![(id(1), 10), (id(4), 20)],
        )
        .await?;
        let o = offer(&s);
        assert_eq!(o.old.device, id(4), "normal suggestion is most recent");
        s.replacement_decide_selected(id(9), o, true, Some(old), |d| {
            assert_eq!(d.removed, vec![id(1)])
        })
        .await?;
        q.reconcile().await?;
        assert!(
            q.snapshot(&[])
                .iter()
                .any(|r| r.device == id(4) && r.state == "waiting_for_device")
        );
        assert!(s.peers().await?.values().any(|peer| *peer == id(4)));
        q.dismiss(id(8), Some(artifact.clone()))
            .await
            .expect_err("live sibling prevents Remove");
        q.change(id(8), Some(artifact.clone()), Some(id(4)), false)
            .await?;
        q.dismiss(id(8), Some(artifact.clone())).await?;
        let retained = s
            .call(move |c| {
                Ok(c.query_row(
                    "SELECT staged FROM outbox_artifacts WHERE id=?",
                    [artifact.as_str()],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await?;
        assert!(!retained, "Remove releases the retained replaced bytes");
        Ok(())
    }

    #[tokio::test]
    async fn replacement_candidates_require_role_name_offline_and_rank_deterministically()
    -> Result<()> {
        let (_dir, s) = fixture().await?;
        pair(
            &s,
            3,
            DeviceRole::Computer,
            "galaxy s25+",
            vec![(id(1), 99)],
        )
        .await?;
        assert!(
            s.replacements.lock().unwrap().is_empty(),
            "same name different role"
        );
        pair(&s, 4, DeviceRole::Phone, "galaxy s25+", vec![]).await?;
        assert!(
            s.replacements.lock().unwrap().is_empty(),
            "online old excluded"
        );
        pair(&s, 5, DeviceRole::Phone, "different", vec![(id(1), 99)]).await?;
        assert!(s.replacements.lock().unwrap().is_empty());
        pair(
            &s,
            6,
            DeviceRole::Phone,
            "galaxy s25+",
            vec![(id(1), 10), (id(4), 20), (id(3), 99)],
        )
        .await?;
        assert_eq!(offer(&s).old.device, id(4));
        pair(
            &s,
            7,
            DeviceRole::Phone,
            "galaxy s25+",
            vec![(id(4), 20), (id(1), 20)],
        )
        .await?;
        assert_eq!(
            s.replacements.lock().unwrap().last().unwrap().old.device,
            id(1)
        );
        assert_eq!(normalized(" Straße "), normalized("STRASSE"));
        Ok(())
    }

    #[tokio::test]
    async fn replacement_ordinary_unpair_releases_stale_offer_and_keeps_new_binding() -> Result<()>
    {
        let (_dir, s) = fixture().await?;
        pair(&s, 2, DeviceRole::Phone, "galaxy s25+", vec![(id(1), 42)]).await?;
        assert!(s.replacement_pending(&id(2)));
        s.unpair_then(id(1), |persisted| assert!(persisted)).await?;
        assert!(!s.replacement_pending(&id(2)));
        assert!(s.peers().await?.values().any(|p| *p == id(2)));
        assert_eq!(s.pairing_generation(&id(2)), Some(1));
        Ok(())
    }

    #[tokio::test]
    async fn replacement_batch_retry_publishes_committed_items_if_a_later_source_is_missing()
    -> Result<()> {
        let (dir, s) = fixture().await?;
        let q = outbox::Outbox::open(s.clone(), &dir.path().join("outbox"))?;
        let old = Authority {
            device: id(1),
            generation: 1,
            fingerprint: pin(1),
        };
        let source = dir.path().join("report.txt");
        fs::write(&source, b"retained bytes")?;
        let first = q
            .admit(id(8), source.clone(), vec![old.clone()], outbox::PEER_BYTES)
            .await?;
        let second = q
            .admit(id(8), source, vec![old], outbox::PEER_BYTES)
            .await?;
        pair(&s, 2, DeviceRole::Phone, "galaxy s25+", vec![(id(1), 42)]).await?;
        s.replacement_decide(id(9), offer(&s), true, |_| {}).await?;
        q.reconcile().await?;
        let folder = Folder::open(&dir.path().join("outbox"), false)?;
        let file = crate::platform::fs::open_read_at(&folder.file, second.as_str())?;
        crate::storage::remove_file(&folder.file, second.as_str(), &file)?;
        assert!(
            q.retry_replaced(
                id(8),
                None,
                id(1),
                vec![Authority {
                    device: id(2),
                    generation: 1,
                    fingerprint: pin(2)
                }],
                id(99),
            )
            .await
            .is_err()
        );
        assert!(
            q.snapshot(&[])
                .iter()
                .any(|r| r.device == id(2) && r.state == "waiting_for_device")
        );
        assert!(
            !dir.path().join("outbox").join(first.as_str()).exists(),
            "partial failure must release the consumed source"
        );
        let consumed = first.clone();
        s.call(move |c| {
            let staged: bool = c.query_row(
                "SELECT staged FROM outbox_artifacts WHERE id=?",
                [consumed.as_str()],
                |r| r.get(0),
            )?;
            assert!(!staged, "consumed bytes no longer count against capacity");
            Ok(())
        })
        .await?;
        let delivery = q
            .claim(id(2), 1, 1)
            .await?
            .context("first resend remains claimable")?;
        assert_ne!(delivery.begin.transfer_id, first);
        let mut file = q.reader(delivery).await?;
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut file, &mut bytes).await?;
        assert_eq!(bytes, b"retained bytes");
        Ok(())
    }

    #[tokio::test]
    async fn replacement_batch_retry_collects_between_copies_near_global_capacity() -> Result<()> {
        let (dir, s) = fixture().await?;
        let q = outbox::Outbox::open(s.clone(), &dir.path().join("outbox"))?;
        let old = Authority {
            device: id(1),
            generation: 1,
            fingerprint: pin(1),
        };
        let mut originals = Vec::new();
        for name in ["first.txt", "second.txt"] {
            let source = dir.path().join(name);
            fs::write(&source, b"retained bytes")?;
            originals.push(
                q.admit(id(8), source, vec![old.clone()], outbox::PEER_BYTES)
                    .await?,
            );
        }
        pair(&s, 2, DeviceRole::Phone, "galaxy s25+", vec![(id(1), 42)]).await?;
        s.replacement_decide(id(9), offer(&s), true, |_| {}).await?;
        q.reconcile().await?;
        // Charge unrelated queued bytes through metadata, leaving room for
        // exactly one copy. No large allocation or file is needed.
        let source = originals[0].clone();
        s.call(move |c| {
            c.execute(
                "INSERT INTO outbox_artifacts(id,operation,meta) SELECT ?,?,json_set(meta,'$.transfer_id',?,'$.size',?) FROM outbox_artifacts WHERE id=?",
                params![id(60).as_str(), id(61).as_str(), id(60).as_str(), (outbox::GLOBAL_BYTES - 3 * b"retained bytes".len() as u64) as i64, source.as_str()],
            )?;
            c.execute(
                "INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES(?,?,1,?)",
                params![id(60).as_str(), id(3).as_str(), pin(3).as_str()],
            )?;
            Ok(())
        }).await?;
        assert_eq!(
            q.retry_replaced(
                id(8),
                None,
                id(1),
                vec![Authority {
                    device: id(2),
                    generation: 1,
                    fingerprint: pin(2),
                }],
                id(99)
            )
            .await?,
            Some(id(2)),
        );
        let rows = q.snapshot(&[]);
        assert_eq!(
            rows.iter()
                .filter(|r| r.device == id(2) && r.state == "waiting_for_device")
                .count(),
            2
        );
        for original in originals {
            assert!(!dir.path().join("outbox").join(original.as_str()).exists());
        }
        Ok(())
    }

    #[tokio::test]
    async fn replacement_stale_generation_and_commit_failure_keep_new_pairing() -> Result<()> {
        let (_dir, s) = fixture().await?;
        pair(&s, 2, DeviceRole::Phone, "galaxy s25+", vec![(id(1), 42)]).await?;
        let o = offer(&s);
        s.fail_next("members_before_commit", 1);
        assert!(
            s.replacement_decide(id(9), o.clone(), true, |_| panic!(
                "failed transaction published"
            ))
            .await
            .is_err()
        );
        assert_eq!(s.peers().await?.len(), 2);
        assert!(s.replacement_pending(&id(2)));
        pair(&s, 1, DeviceRole::Phone, "galaxy s25+", vec![]).await?;
        assert!(
            s.replacement_decide(id(9), o, true, |_| panic!("stale choice published"))
                .await
                .is_err()
        );
        assert_eq!(s.peers().await?.len(), 2);
        Ok(())
    }
}
