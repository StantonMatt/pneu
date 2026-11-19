//! Clipboard persistence primitives. Ordering decisions belong to the actor;
//! these transactions provide the shared Store clock and durable receipts.
use super::*;
use crate::core::clipboard_actor::{Fingerprint, Key};

/// Existing fingerprints and text receipts contain prehashed representations,
/// so migration can key them without recovering clipboard plaintext. File
/// receipt SHA-256 is part of the protocol and remains unchanged.
pub(super) fn migrate_fingerprints(
    tx: &Transaction<'_>,
    key: &crate::clipboard_fingerprint::FingerprintKey,
) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS clipboard_fingerprint_format(singleton INTEGER PRIMARY KEY CHECK(singleton=1),key_tag TEXT NOT NULL);")?;
    let tag = hex::encode(key.digest(&[0; 32]));
    let previous: Option<String> = tx
        .query_row(
            "SELECT key_tag FROM clipboard_fingerprint_format WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(previous) = previous {
        if previous != tag {
            // A missing/truncated pre-release key is regenerated atomically.
            // Key publication and this transaction cannot share a commit, so
            // key-tag mismatch is also the restart-safe recovery signal.
            // Only keyed echo/dedup data is disposable; preserve register and
            // legacy clocks, file receipts, pairing, settings and outbox.
            tx.execute_batch(
                "DELETE FROM clipboard_fingerprints;
                DELETE FROM clipboard_recent WHERE json_extract(meta,'$.operation')='Text';
                DELETE FROM receipts WHERE json_extract(meta,'$.operation')='Text';",
            )?;
            tx.execute(
                "UPDATE clipboard_fingerprint_format SET key_tag=? WHERE singleton=1",
                [tag],
            )?;
        }
        return Ok(());
    }
    let rows = tx
        .prepare("SELECT seq,meta FROM clipboard_fingerprints")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (seq, meta) in rows {
        let mut f: Fingerprint = serde_json::from_str(&meta)?;
        f.hashes = key.hashes(&f.hashes);
        tx.execute(
            "UPDATE clipboard_fingerprints SET meta=? WHERE seq=?",
            params![serde_json::to_string(&f)?, seq],
        )?;
    }
    for table in ["receipts", "clipboard_recent"] {
        let rows = tx
            .prepare(&format!("SELECT device,id,meta FROM {table}"))?
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (device, id, meta) in rows {
            // Old file receipts may have fields from earlier schema versions.
            // Only the explicitly tagged text metadata belongs to this format.
            let mut value: serde_json::Value = serde_json::from_str(&meta)?;
            if value.get("operation").and_then(|v| v.as_str()) != Some("Text") {
                continue;
            }
            let hash = value
                .get("sha256")
                .and_then(|v| v.as_str())
                .context("text receipt hash")?;
            let raw: [u8; 32] = hex::decode(hash)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid text receipt hash"))?;
            value["sha256"] = hex::encode(key.digest(&raw)).into();
            tx.execute(
                &format!("UPDATE {table} SET meta=? WHERE device=? AND id=?"),
                params![serde_json::to_string(&value)?, device, id],
            )?;
        }
    }
    tx.execute(
        "INSERT INTO clipboard_fingerprint_format VALUES (1,?)",
        [tag],
    )?;
    Ok(())
}

pub(crate) type ClipboardSnapshot = (Option<Key>, HashMap<Id, u64>, Vec<Fingerprint>);
impl Store {
    pub(crate) async fn clipboard_snapshot(&self) -> Result<ClipboardSnapshot> {
        self.call(|c| {
            let register: Option<(Option<String>, String)> = c
                .query_row(
                    "SELECT hlc,source_device FROM clipboard_clock WHERE singleton=1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let register = register
                .and_then(|(hlc, source)| hlc.map(|hlc| (hlc, source)))
                .map(|(hlc, source)| {
                    Ok::<_, anyhow::Error>(Key {
                        hlc: hlc.parse()?,
                        source: Id::new(source)?,
                    })
                })
                .transpose()?;
            let mut q = c.prepare("SELECT device,created_at FROM legacy_clipboard_clock")?;
            let legacy = q
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .map(|row| {
                    let (id, at) = row?;
                    Ok((Id::new(id)?, at as u64))
                })
                .collect::<Result<HashMap<_, _>>>()?;
            let mut q = c.prepare("SELECT meta FROM clipboard_fingerprints ORDER BY seq")?;
            let history = q
                .query_map([], |r| r.get::<_, String>(0))?
                .map(|row| Ok(serde_json::from_str(&row?)?))
                .collect::<Result<Vec<_>>>()?;
            Ok((register, legacy, history))
        })
        .await
    }
    pub(crate) async fn allocate_clipboard_clock(
        &self,
        source: Id,
        floor: Option<Key>,
        validate: impl FnOnce(u64) -> Result<()> + Send + 'static,
    ) -> Result<Key> {
        #[cfg(test)]
        self.fault("local_observation_checked")?;
        let job = move |store: &Self| {
            store.locked(|c| {
                let tx = c.transaction()?;
                if let Some(floor) = floor {
                    observe_hlc(&tx, floor.hlc)?;
                }
                let (_, hlc) = allocate_stamp(&tx)?;
                validate(hlc)?;
                tx.commit()?;
                Ok(Key { hlc, source })
            })
        };
        // Claims queued behind the actor may retain every general admission
        // permit. No actor-awaited clock may use that pool, including explicit
        // shares and legacy receives. Two independent slots also bound detached
        // closures after cancellation; the closure owns its slot until it exits.
        let permit = self.clipboard_clock_slots.clone().acquire_owned().await?;
        self.start_completion(move |store| {
            let _permit = permit;
            job(store)
        })
        .await?
    }
    pub(crate) async fn persist_clipboard_register(&self, key: Key, created_at: u64) -> Result<()> {
        self.start_completion(move |store| {
            #[cfg(test)]
            store.fault("applied_image")?;
            store.locked(|c| {
                let tx = c.transaction()?;
                apply_stamp(&tx, key.hlc, &key.source, created_at)?;
                tx.commit()?;
                Ok(())
            })
        })
        .await?
    }
    pub(crate) fn persist_clipboard_history(
        &self,
        revision: u64,
        history: Vec<Fingerprint>,
    ) -> futures_util::future::BoxFuture<'static, Result<()>> {
        use futures_util::FutureExt;
        let store = self.clone();
        async move {
            #[cfg(test)]
            store.fault("persist_clipboard_history")?;
            let current = store.clipboard_history_revision.clone();
            store
                .start_completion(move |store| {
                    store.locked(|c| {
                        let tx = c.transaction()?;
                        if current.load(Ordering::SeqCst) > revision {
                            return Ok(());
                        }
                        tx.execute("DELETE FROM clipboard_fingerprints", [])?;
                        for fingerprint in history
                            .into_iter()
                            .rev()
                            .take(64)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                        {
                            tx.execute(
                                "INSERT INTO clipboard_fingerprints(meta) VALUES(?)",
                                [serde_json::to_string(&fingerprint)?],
                            )?;
                        }
                        tx.commit()?;
                        current.store(revision, Ordering::SeqCst);
                        Ok(())
                    })
                })
                .await?
        }
        .boxed()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn fingerprint(n: u8) -> Fingerprint {
        Fingerprint {
            id: Id::from_bytes([n; 16]),
            key: Key {
                hlc: n as u64,
                source: Id::from_bytes([1; 16]),
            },
            hashes: vec![[n; 32]],
        }
    }
    #[tokio::test]
    async fn history_ignores_late_old_snapshots_and_failed_new_writes_do_not_fence_recovery()
    -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store
            .persist_clipboard_history(2, vec![fingerprint(2)])
            .await?;
        store
            .persist_clipboard_history(1, vec![fingerprint(1)])
            .await?;
        assert_eq!(store.clipboard_snapshot().await?.2[0].id, fingerprint(2).id);
        store.fail_next("persist_clipboard_history", 1);
        assert!(
            store
                .persist_clipboard_history(4, vec![fingerprint(4)])
                .await
                .is_err()
        );
        store
            .persist_clipboard_history(3, vec![fingerprint(3)])
            .await?;
        assert_eq!(store.clipboard_snapshot().await?.2[0].id, fingerprint(3).id);
        Ok(())
    }
    #[tokio::test]
    async fn actor_clock_and_history_complete_with_every_admission_slot_reserved() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        let mut reservations = Vec::new();
        for _ in 0..crate::work::BLOCKING_LIMIT {
            reservations.push(store.reserve().await);
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let key = store
                .allocate_clipboard_clock(Id::from_bytes([1; 16]), None, |_| Ok(()))
                .await?;
            store
                .persist_clipboard_history(1, vec![fingerprint(1)])
                .await?;
            store.persist_clipboard_register(key, 1).await?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        drop(reservations);
        store.drain().await;
        Ok(())
    }
    #[tokio::test]
    async fn canceled_clock_jobs_keep_their_independent_capacity_until_the_closure_exits()
    -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let first = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .allocate_clipboard_clock(Id::from_bytes([1; 16]), None, move |_| {
                        let _ = entered.send(());
                        wait.recv_timeout(std::time::Duration::from_secs(5))?;
                        Ok(())
                    })
                    .await
            }
        });
        started.await?;
        let second = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .allocate_clipboard_clock(Id::from_bytes([2; 16]), None, |_| Ok(()))
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while store.clipboard_clock_slots.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        first.abort();
        second.abort();
        let _ = first.await;
        let _ = second.await;
        assert_eq!(store.clipboard_clock_slots.available_permits(), 0);
        let mut third = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .allocate_clipboard_clock(Id::from_bytes([3; 16]), None, |_| Ok(()))
                    .await
            }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut third)
                .await
                .is_err()
        );
        release.send(())?;
        tokio::time::timeout(std::time::Duration::from_secs(2), third).await???;
        store.drain().await;
        assert_eq!(store.clipboard_clock_slots.available_permits(), 2);
        assert_eq!(store.active_jobs(), 0);
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod privacy_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[tokio::test]
    async fn fingerprint_key_is_private_stable_outside_db_and_defeats_dictionary() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        let store = Store::open(&path)?;
        let raw: [u8; 32] = Sha256::digest(b"1234").into();
        let keyed = store.clipboard_fingerprint_key.digest(&raw);
        let key_path = dir.path().join("clipboard-fingerprint.key");
        assert_eq!(fs::metadata(&key_path)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&key_path)?.len(), 32);
        let f = Fingerprint {
            id: Id::from_bytes([1; 16]),
            key: Key {
                hlc: 1,
                source: Id::from_bytes([2; 16]),
            },
            hashes: vec![keyed],
        };
        store.persist_clipboard_history(1, vec![f]).await?;
        for guess in 0..10_000 {
            let raw: [u8; 32] = Sha256::digest(format!("{guess:04}").as_bytes()).into();
            assert_ne!(keyed, raw);
        }
        store.drain().await;
        drop(store);
        let store = Store::open(&path)?;
        assert_eq!(store.clipboard_fingerprint_key.digest(&raw), keyed);
        assert_eq!(store.clipboard_snapshot().await?.2[0].hashes, vec![keyed]);
        let bytes = fs::read(&path)?;
        let secret = fs::read(&key_path)?;
        assert!(!bytes.windows(32).any(|w| w == secret));
        let other = crate::test_support::scratch()?;
        assert_ne!(
            Store::open(&other.path().join("state.sqlite3"))?
                .clipboard_fingerprint_key
                .digest(&raw),
            keyed
        );
        Ok(())
    }
    #[tokio::test]
    async fn plain_fingerprint_and_text_receipt_migration_is_atomic_and_idempotent() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        let store = Store::open(&path)?;
        let text = Clipboard {
            sensitive: false,
            transfer_id: Id::from_bytes([1; 16]),
            text: "1234".into(),
            created_at: 1,
            origin: ClipboardOrigin::Clipboard,
            hlc: Some(1),
            source_device: Some(Id::from_bytes([2; 16])),
        };
        let raw: [u8; 32] = Sha256::digest(text.text.as_bytes()).into();
        let keyed = store.clipboard_fingerprint_key.digest(&raw);
        store.locked(|c| {
            c.execute("DELETE FROM clipboard_fingerprint_format", [])?;
            c.execute(
                "INSERT INTO peers(id,fp) VALUES (?,?)",
                params![Id::from_bytes([2; 16]).as_str(), "a".repeat(64)],
            )?;
            c.execute(
                "INSERT INTO clipboard_fingerprints(meta) VALUES (?)",
                [serde_json::to_string(&Fingerprint {
                    id: text.transfer_id.clone(),
                    key: Key {
                        hlc: 1,
                        source: Id::from_bytes([2; 16]),
                    },
                    hashes: vec![raw],
                })?],
            )?;
            c.execute(
                "INSERT INTO clipboard_recent VALUES (?,?,?)",
                params![
                    Id::from_bytes([2; 16]).as_str(),
                    text.transfer_id.as_str(),
                    serde_json::to_string(&Metadata::Text((&text).into()))?
                ],
            )?;
            Ok(())
        })?;
        drop(store);
        for _ in 0..2 {
            let store = Store::open(&path)?;
            assert_eq!(store.clipboard_snapshot().await?.2[0].hashes, vec![keyed]);
            let expected = Metadata::Text(store.text_receipt(&text));
            assert_eq!(
                store
                    .receipt(Id::from_bytes([2; 16]), text.transfer_id.clone())
                    .await?,
                Some((expected, true))
            );
            drop(store);
        }
        assert!(
            !fs::read(path)?
                .windows(64)
                .any(|w| w == hex::encode(raw).as_bytes())
        );
        Ok(())
    }
    #[test]
    fn fingerprint_key_rejects_symlinks_wrong_permissions_and_recovers_truncation() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("key");
        let target = dir.path().join("target");
        fs::write(&target, [7; 32])?;
        std::os::unix::fs::symlink(&target, &path)?;
        assert!(crate::clipboard_fingerprint::FingerprintKey::load_or_create(&path).is_err());
        fs::remove_file(&path)?;
        fs::write(&path, [7; 32])?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        assert!(crate::clipboard_fingerprint::FingerprintKey::load_or_create(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        fs::write(&path, [7; 31])?;
        crate::clipboard_fingerprint::FingerprintKey::load_or_create(&path)?;
        assert_eq!(fs::metadata(&path)?.len(), 32);
        Ok(())
    }
    #[tokio::test]
    async fn corrupt_key_recovery_discards_only_keyed_history_and_text_receipts() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        let store = Store::open(&path)?;
        let peer = Id::from_bytes([2; 16]);
        store.pair(peer.clone(), Hash::new("a".repeat(64))?).await?;
        let key = Key {
            hlc: 100,
            source: peer.clone(),
        };
        store.persist_clipboard_register(key.clone(), 100).await?;
        store
            .persist_clipboard_history(
                1,
                vec![Fingerprint {
                    id: Id::from_bytes([3; 16]),
                    key: key.clone(),
                    hashes: vec![[4; 32]],
                }],
            )
            .await?;
        let text = Clipboard {
            sensitive: false,
            transfer_id: Id::from_bytes([5; 16]),
            text: "private".into(),
            created_at: 100,
            origin: ClipboardOrigin::Clipboard,
            hlc: Some(100),
            source_device: Some(peer.clone()),
        };
        let meta = serde_json::to_string(&Metadata::Text(store.text_receipt(&text)))?;
        store.locked(|c| {
            c.execute(
                "INSERT INTO clipboard_recent VALUES (?,?,?)",
                params![peer.as_str(), text.transfer_id.as_str(), meta],
            )?;
            c.execute(
                "INSERT INTO receipts(device,id,meta) VALUES (?,?,'{\"operation\":\"File\"}')",
                params![peer.as_str(), Id::from_bytes([6; 16]).as_str()],
            )?;
            Ok(())
        })?;
        store.drain().await;
        drop(store);
        fs::write(dir.path().join("clipboard-fingerprint.key"), [0; 7])?;
        let mut previous_key = None;
        for _ in 0..2 {
            let store = Store::open(&path)?;
            let bytes = fs::read(dir.path().join("clipboard-fingerprint.key"))?;
            assert_eq!(bytes.len(), 32);
            if let Some(previous) = previous_key {
                assert_eq!(bytes, previous);
            }
            previous_key = Some(bytes);
            let (register, _, history) = store.clipboard_snapshot().await?;
            assert_eq!(register, Some(key.clone()));
            assert!(history.is_empty());
            let counts: (i64, i64, i64) = store.call(|c| Ok(c.query_row(
                "SELECT (SELECT count(*) FROM peers),(SELECT count(*) FROM receipts),(SELECT count(*) FROM clipboard_recent)",
                [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)).await?;
            assert_eq!(counts, (1, 1, 0));
        }
        Ok(())
    }
}
