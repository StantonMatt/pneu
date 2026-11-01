//! Opt-in user-visible clipboard history. Independent of protocol echo receipts.
use super::*;
use crate::actions::ClipboardInput;
use image::GenericImageView;
use std::sync::Weak;

pub(crate) const MAX_TEXT: usize = 1024 * 1024;
pub(crate) const MAX_IMAGE: usize = 20 * 1024 * 1024;
const LIMIT: usize = 50;
pub(crate) const PIN_LIMIT: usize = 25;
pub(crate) const PIN_LIMIT_ERROR: &str = "You can pin up to 25 items";

#[derive(Debug)]
pub(crate) struct Capture {
    generation: u64,
    pub(crate) seq: i64,
    at: u64,
    deleted: Mutex<HashSet<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub title: String,
    // ClipboardHistorySearch sends complete keys separately from status, in bounded chunks.
    #[serde(skip)]
    pub search: String,
    #[serde(default)]
    pub pinned: Option<i64>,
    pub url: bool,
    pub image: bool,
    pub origin: Option<String>,
    pub at: u64,
    pub thumbnail: Option<PathBuf>,
}
#[derive(Default)]
pub(super) struct State {
    enabled: bool,
    generation: u64,
    sequence: i64,
    captures: Vec<Weak<Capture>>,
    items: Vec<Item>,
}
pub(super) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS clipboard_items(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,digest TEXT UNIQUE NOT NULL,text TEXT,origin TEXT,at INTEGER NOT NULL);")?;
    let pinned: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('clipboard_items') WHERE name='pinned')",
        [],
        |r| r.get(0),
    )?;
    if !pinned {
        tx.execute_batch("ALTER TABLE clipboard_items ADD COLUMN pinned INTEGER;")?;
    }
    Ok(())
}
/// The complete text is normalized before presentation truncates or drops a URL scheme.
pub(crate) fn search_key(text: &str) -> String {
    use caseless::Caseless;
    use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .nfd()
        .filter(|c| !is_combining_mark(*c))
        .default_case_fold()
        .collect()
}
fn evict(c: &Connection, protected: &str) -> Result<Vec<String>> {
    let old = c.prepare("SELECT id FROM clipboard_items WHERE pinned IS NULL ORDER BY (id=?) DESC,seq DESC LIMIT -1 OFFSET ?")?
        .query_map(params![protected, LIMIT as i64], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in &old {
        c.execute("DELETE FROM clipboard_items WHERE id=?", [id])?;
    }
    Ok(old)
}
pub(crate) fn is_url(text: &str) -> bool {
    let text = text.trim();
    !text.chars().any(char::is_whitespace)
        && url::Url::parse(text)
            .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
}
pub(crate) fn title(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = if is_url(&text) {
        text.strip_prefix("https://")
            .or_else(|| text.strip_prefix("http://"))
            .unwrap_or(&text)
    } else {
        &text
    };
    let mut chars = text.chars();
    let mut title: String = chars.by_ref().take(512).collect();
    if chars.next().is_some() {
        title.push('…');
    }
    title
}
fn image_path(folder: &Path, id: &str, thumbnail: bool) -> PathBuf {
    folder.join(format!("{id}{}.png", if thumbnail { "-thumb" } else { "" }))
}
fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn remove_images(folder: &Path, id: &str) -> Result<()> {
    ensure!(valid_id(id), "invalid history id");
    for thumb in [false, true] {
        match fs::remove_file(image_path(folder, id, thumb)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn load(c: &Connection, folder: &Path) -> Result<Vec<Item>> {
    let rows = c
        .prepare("SELECT id,text,origin,at,pinned FROM clipboard_items ORDER BY pinned IS NOT NULL DESC,pinned DESC,seq DESC")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)? as u64,
                r.get::<_, Option<i64>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut items = Vec::new();
    for (id, text, origin, at, pinned) in rows {
        if !valid_id(&id) {
            c.execute("DELETE FROM clipboard_items WHERE id=?", [&id])?;
            continue;
        }
        let image = text.is_none();
        if image && !image_path(folder, &id, false).is_file() {
            c.execute("DELETE FROM clipboard_items WHERE id=?", [&id])?;
            remove_images(folder, &id)?;
            continue;
        }
        let thumb = image_path(folder, &id, true);
        items.push(Item {
            id,
            pinned,
            search: text.as_deref().map(search_key).unwrap_or_default(),
            title: text.as_deref().map(title).unwrap_or_else(|| "Image".into()),
            url: text.as_deref().is_some_and(is_url),
            image,
            origin,
            at,
            thumbnail: image.then_some(thumb).filter(|p| p.is_file()),
        });
    }
    Ok(items)
}
fn clear(c: &Connection, folder: &Path) -> Result<()> {
    c.execute("DELETE FROM clipboard_items", [])?;
    // This directory contains only task-owned history artifacts, including a
    // file written just before a crash, before its SQLite row was committed.
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
        }
    }
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}

impl Store {
    // Recording an admitted receive must not reserve another receive slot.
    // The owned history gate bounds these completion jobs to one at a time.
    async fn history_call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.start_completion(move |store| store.locked(f)).await?
    }
    fn history_folder(&self) -> PathBuf {
        self.history_folder.clone()
    }
    fn history_generation(&self) -> Option<u64> {
        let state = self.history.lock().expect("clipboard history");
        state.enabled.then_some(state.generation)
    }
    pub(crate) fn history_capture(&self) -> Option<Arc<Capture>> {
        let mut state = self.history.lock().expect("clipboard history");
        if !state.enabled {
            return None;
        }
        // Reserve the existing SQLite seq at observation time, before any read.
        state.sequence = state.sequence.checked_add(1)?;
        let capture = Arc::new(Capture {
            generation: state.generation,
            seq: state.sequence,
            at: crate::now_ms(),
            deleted: Mutex::new(HashSet::new()),
        });
        state.captures.retain(|capture| capture.strong_count() != 0);
        state.captures.push(Arc::downgrade(&capture));
        Some(capture)
    }
    pub(crate) fn history_items(&self) -> Vec<Item> {
        self.history
            .lock()
            .expect("clipboard history")
            .items
            .clone()
    }
    pub(crate) async fn history_disable_if_empty(&self) -> usize {
        let _gate = self.history_gate.lock().await;
        let mut state = self.history.lock().expect("clipboard history");
        let count = state.items.len();
        if count == 0 {
            // Fence queued captures before releasing the same gate they use.
            state.enabled = false;
            state.generation = state.generation.wrapping_add(1);
        }
        count
    }
    pub(crate) async fn history_configure(&self, enabled: bool, startup: bool) -> Result<()> {
        let gate = self.history_gate.clone().lock_owned().await;
        let folder = self.history_folder();
        let state = self.history.clone();
        self.history_call(move |c| {
            let _gate = gate;
            crate::config::private_dir(&folder)?;
            if !enabled || !startup {
                clear(c, &folder)?;
            }
            let items = load(c, &folder)?;
            let retained: HashSet<_> = items
                .iter()
                .filter(|i| i.image)
                .flat_map(|i| {
                    [
                        image_path(&folder, &i.id, false),
                        image_path(&folder, &i.id, true),
                    ]
                })
                .collect();
            for entry in fs::read_dir(&folder)? {
                let entry = entry?;
                if entry.file_type()?.is_file() && !retained.contains(&entry.path()) {
                    fs::remove_file(entry.path())?;
                }
            }
            let mut state = state.lock().expect("clipboard history");
            let sequence: i64 = c.query_row(
                "SELECT coalesce(max(seq),0) FROM clipboard_items",
                [],
                |r| r.get(0),
            )?;
            state.sequence = state.sequence.max(sequence);
            state.generation = state.generation.wrapping_add(1);
            state.captures.clear();
            state.enabled = enabled;
            state.items = items;
            Ok(())
        })
        .await
    }
    pub(crate) async fn history_record(
        &self,
        input: ClipboardInput,
        origin: Option<String>,
        capture: Arc<Capture>,
    ) -> Result<()> {
        let gate = self.history_gate.clone().lock_owned().await;
        if self.history_generation() != Some(capture.generation) {
            return Ok(());
        }
        let folder = self.history_folder();
        let state = self.history.clone();
        self.history_call(move |c| {
            let _gate = gate;
            let items = (|| {
                let (text, bytes) = match input {
                    ClipboardInput::Text(t) => (Some(t), None),
                    ClipboardInput::Formats { png, .. } => (None, Some(png)),
                    ClipboardInput::Bytes { bytes, .. } => (None, Some(bytes)),
                    ClipboardInput::Image(file, _) => match file.read_bounded(MAX_IMAGE) {
                        Ok(bytes) => (None, Some(bytes)),
                        Err(_) => return load(c, &folder),
                    },
                };
                if text
                    .as_ref()
                    .is_some_and(|t| t.trim().is_empty() || t.len() > MAX_TEXT)
                    || bytes.as_ref().is_some_and(|b| b.len() > MAX_IMAGE)
                {
                    return load(c, &folder);
                }
                let mut png = None;
                let mut thumbnail = None;
                let digest = if let Some(text) = &text {
                    format!("text:{}", hex::encode(Sha256::digest(text.as_bytes())))
                } else {
                    let bytes = bytes.context("history image")?;
                    let image = match crate::actions::decode_image(&bytes) {
                        Ok(image) => image,
                        Err(_) => return load(c, &folder),
                    };
                    let mut hash = Sha256::new();
                    hash.update(image.width().to_be_bytes());
                    hash.update(image.height().to_be_bytes());
                    hash.update(image.to_rgba8().as_raw());
                    let mut output = std::io::Cursor::new(Vec::new());
                    image.write_to(&mut output, image::ImageFormat::Png)?;
                    if output.get_ref().len() > MAX_IMAGE {
                        return load(c, &folder);
                    }
                    png = Some(output.into_inner());
                    let mut output = std::io::Cursor::new(Vec::new());
                    let thumb = image.resize_to_fill(72, 72, image::imageops::FilterType::Triangle);
                    debug_assert_eq!(thumb.dimensions(), (72, 72));
                    thumb.write_to(&mut output, image::ImageFormat::Png)?;
                    thumbnail = Some(output.into_inner());
                    format!("image:{}", hex::encode(hash.finalize()))
                };
                if capture.deleted.lock().expect("deleted history captures").contains(&digest) {
                    return load(c, &folder);
                }
                let existing = c
                    .query_row(
                        "SELECT id,seq FROM clipboard_items WHERE digest=?",
                        [&digest],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                    )
                    .optional()?;
                // A late older copy must not replace a newer duplicate's metadata.
                if existing.as_ref().is_some_and(|(_, seq)| *seq >= capture.seq) {
                    return load(c, &folder);
                }
                let id = existing.map(|(id, _)| id)
                    .unwrap_or_else(|| crate::random_hex(16));
                if let Some(png) = png {
                    crate::config::atomic_private(&image_path(&folder, &id, false), &png)?;
                }
                if let Some(thumb) = thumbnail {
                    crate::config::atomic_private(&image_path(&folder, &id, true), &thumb)?;
                }
                let tx = c.transaction()?;
                tx.execute(
                    "INSERT INTO clipboard_items(seq,id,digest,text,origin,at) VALUES (?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET seq=excluded.seq,origin=excluded.origin,at=excluded.at",
                    params![capture.seq, id, digest, text, origin, capture.at as i64],
                )?;
                let old = tx
                    .prepare("SELECT id FROM clipboard_items WHERE pinned IS NULL ORDER BY seq DESC LIMIT -1 OFFSET ?")?
                    .query_map([LIMIT as i64], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for id in &old {
                    tx.execute("DELETE FROM clipboard_items WHERE id=?", [id])?;
                }
                tx.commit()?;
                for id in old {
                    remove_images(&folder, &id)?;
                }
                load(c, &folder)
            })()?;
            state.lock().expect("clipboard history").items = items;
            Ok(())
        })
        .await
    }
    pub(crate) async fn history_delete(&self, id: Option<String>) -> Result<()> {
        let gate = self.history_gate.clone().lock_owned().await;
        if let Some(id) = &id {
            ensure!(valid_id(id), "invalid history id");
        }
        let cutoff = {
            let mut state = self.history.lock().expect("clipboard history");
            if id.is_none() {
                state.generation = state.generation.wrapping_add(1);
                state.captures.clear();
            }
            state.sequence
        };
        let folder = self.history_folder();
        let state = self.history.clone();
        self.history_call(move |c| {
            let _gate = gate;
            if let Some(id) = id {
                let digest: Option<String> = c
                    .query_row(
                        "SELECT digest FROM clipboard_items WHERE id=?",
                        [&id],
                        |r| r.get(0),
                    )
                    .optional()?;
                c.execute("DELETE FROM clipboard_items WHERE id=?", [&id])?;
                if let Some(digest) = digest {
                    // Delete cancels pre-delete captures of this content; a new copy may add it again.
                    state
                        .lock()
                        .expect("clipboard history")
                        .captures
                        .retain(|capture| {
                            if let Some(capture) = capture.upgrade() {
                                if capture.seq <= cutoff {
                                    capture
                                        .deleted
                                        .lock()
                                        .expect("deleted history captures")
                                        .insert(digest.clone());
                                }
                                true
                            } else {
                                false
                            }
                        });
                }
                remove_images(&folder, &id)?;
            } else {
                c.execute("DELETE FROM clipboard_items WHERE pinned IS NULL", [])?;
                let retained: HashSet<_> = load(c, &folder)?
                    .into_iter()
                    .filter(|item| item.image)
                    .flat_map(|item| {
                        [
                            image_path(&folder, &item.id, false),
                            image_path(&folder, &item.id, true),
                        ]
                    })
                    .collect();
                // A failed recording can leave files before its row commits.
                for entry in fs::read_dir(&folder)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() && !retained.contains(&entry.path()) {
                        fs::remove_file(entry.path())?;
                    }
                }
                c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            }
            let items = load(c, &folder)?;
            state.lock().expect("clipboard history").items = items;
            Ok(())
        })
        .await
    }
    pub(crate) async fn history_pin(&self, id: String, pinned: bool) -> Result<()> {
        ensure!(valid_id(&id), "invalid history id");
        let gate = self.history_gate.clone().lock_owned().await;
        ensure!(
            self.history_generation().is_some(),
            "clipboard history is off"
        );
        let folder = self.history_folder();
        let state = self.history.clone();
        self.history_call(move |c| {
            let _gate = gate;
            let tx = c.transaction()?;
            let current: Option<i64> = tx.query_row("SELECT pinned FROM clipboard_items WHERE id=?", [&id], |r| r.get(0)).context("history item was deleted")?;
            let mut evicted = Vec::new();
            if pinned && current.is_none() {
                let count: i64 = tx.query_row("SELECT count(*) FROM clipboard_items WHERE pinned IS NOT NULL", [], |r| r.get(0))?;
                ensure!(count < PIN_LIMIT as i64, PIN_LIMIT_ERROR);
                tx.execute("UPDATE clipboard_items SET pinned=(SELECT coalesce(max(pinned),0)+1 FROM clipboard_items) WHERE id=?", [&id])?;
            } else if !pinned && current.is_some() {
                tx.execute("UPDATE clipboard_items SET pinned=NULL WHERE id=?", [&id])?;
                // Unpin always retains this item, even if it is older than all 50 Recent items.
                evicted = evict(&tx, &id)?;
            }
            tx.commit()?;
            for id in evicted { remove_images(&folder, &id)?; }
            state.lock().expect("clipboard history").items = load(c, &folder)?;
            Ok(())
        }).await
    }
    pub(crate) async fn history_input(&self, id: String) -> Result<(ClipboardInput, Arc<Capture>)> {
        ensure!(valid_id(&id), "invalid history id");
        let gate = self.history_gate.clone().lock_owned().await;
        let capture = self.history_capture().context("clipboard history is off")?;
        let folder = self.history_folder();
        let input = self
            .history_call(move |c| {
                let _gate = gate;
                let text: Option<String> = c
                    .query_row("SELECT text FROM clipboard_items WHERE id=?", [&id], |r| {
                        r.get(0)
                    })
                    .context("history item was deleted")?;
                Ok(match text {
                    Some(t) => ClipboardInput::Text(t),
                    None => ClipboardInput::Image(
                        crate::actions::ReceivedFile::open(&image_path(&folder, &id, false))?,
                        "image/png".into(),
                    ),
                })
            })
            .await?;
        Ok((input, capture))
    }
    pub(crate) async fn history_touch(&self, id: String, capture: Arc<Capture>) -> Result<()> {
        let gate = self.history_gate.clone().lock_owned().await;
        if self.history_generation() != Some(capture.generation) {
            return Ok(());
        }
        let folder = self.history_folder();
        let state = self.history.clone();
        self.history_call(move |c| {
            let _gate = gate;
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE clipboard_items SET seq=?,at=? WHERE id=? AND seq<?",
                params![capture.seq, capture.at as i64, id, capture.seq],
            )?;
            tx.commit()?;
            let items = load(c, &folder)?;
            state.lock().expect("clipboard history").items = items;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn record(store: &Store, text: &str, origin: Option<&str>) -> Result<()> {
        store
            .history_record(
                ClipboardInput::Text(text.into()),
                origin.map(str::to_owned),
                store.history_capture().context("enabled history")?,
            )
            .await
    }
    fn png(color: u8, compression: image::codecs::png::CompressionType) -> Result<Vec<u8>> {
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new_with_quality(
            &mut bytes,
            compression,
            image::codecs::png::FilterType::Adaptive,
        )
        .write_image(&[color, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)?;
        Ok(bytes)
    }
    async fn record_image(store: &Store, bytes: Vec<u8>) -> Result<()> {
        store
            .history_record(
                ClipboardInput::Bytes {
                    bytes,
                    mime: "image/png".into(),
                    cache: store.history_folder(),
                },
                None,
                store.history_capture().unwrap(),
            )
            .await
    }
    #[test]
    fn search_folds_case_accents_whitespace_and_full_unicode_case() {
        assert_eq!(
            search_key("  CAFÉ\tmañana\n Straße Σς  "),
            "cafe manana strasse σσ"
        );
        assert_eq!(search_key("CAFE\u{301}"), "cafe");
        assert_eq!(search_key("Ა"), search_key("ა")); // Modern Georgian capitals.

        assert_eq!(search_key("\t  "), "");
        assert!(search_key("https://example.org/Café").contains(&search_key("HTTPS://EXAMPLE")));
    }
    #[tokio::test]
    async fn pinned_image_files_survive_clear_and_restart_then_off_removes_them() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        let id = {
            let store = Store::open(&path)?;
            store.history_configure(true, true).await?;
            record_image(&store, png(1, image::codecs::png::CompressionType::Fast)?).await?;
            let id = store.history_items()[0].id.clone();
            store.history_pin(id.clone(), true).await?;
            record_image(&store, png(2, image::codecs::png::CompressionType::Fast)?).await?;
            store.history_delete(None).await?;
            assert_eq!(fs::read_dir(store.history_folder())?.count(), 2);
            id
        };
        let store = Store::open(&path)?;
        store.history_configure(true, true).await?;
        assert_eq!(store.history_items()[0].id, id);
        assert!(store.history_items()[0].pinned.is_some());
        assert!(
            store.history_items()[0]
                .thumbnail
                .as_ref()
                .unwrap()
                .is_file()
        );
        store.history_configure(false, false).await?;
        assert!(store.history_items().is_empty());
        assert_eq!(fs::read_dir(store.history_folder())?.count(), 0);
        Ok(())
    }
    #[tokio::test]
    async fn clear_removes_failed_recording_orphans_and_preserves_pinned_images() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.history_configure(true, true).await?;
        record_image(&store, png(1, image::codecs::png::CompressionType::Fast)?).await?;
        let id = store.history_items()[0].id.clone();
        store.history_pin(id.clone(), true).await?;
        let folder = store.history_folder();
        let original = image_path(&folder, &id, false);
        let thumbnail = image_path(&folder, &id, true);
        let original_bytes = fs::read(&original)?;
        let thumbnail_bytes = fs::read(&thumbnail)?;
        record_image(&store, png(2, image::codecs::png::CompressionType::Fast)?).await?;
        store.locked(|c| {
            c.execute_batch("CREATE TEMP TRIGGER fail_history_insert BEFORE INSERT ON clipboard_items BEGIN SELECT RAISE(FAIL, 'history insert failed'); END;")?;
            Ok(())
        })?;
        let error = record_image(&store, png(3, image::codecs::png::CompressionType::Fast)?)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("history insert failed"));
        let rows = store.locked(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM clipboard_items", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })?;
        assert_eq!(rows, 2);
        assert_eq!(fs::read_dir(&folder)?.count(), 6);

        store.history_delete(None).await?;

        assert_eq!(fs::read_dir(&folder)?.count(), 2);
        assert_eq!(fs::read(&original)?, original_bytes);
        assert_eq!(fs::read(&thumbnail)?, thumbnail_bytes);
        let items = store.history_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
        assert!(items[0].pinned.is_some());
        store.history_pin(id, false).await?;
        store.history_delete(None).await?;
        assert!(store.history_items().is_empty());
        assert_eq!(fs::read_dir(&folder)?.count(), 0);
        Ok(())
    }
    #[tokio::test]
    async fn pins_survive_dedupe_restart_eviction_and_clear_but_not_off() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        let store = Store::open(&path)?;
        store.history_configure(true, true).await?;
        record(&store, "first pin", None).await?;
        let first = store.history_items()[0].id.clone();
        store.history_pin(first.clone(), true).await?;
        record(&store, "second pin", None).await?;
        let second = store
            .history_items()
            .iter()
            .find(|i| i.title == "second pin")
            .unwrap()
            .id
            .clone();
        store.history_pin(second.clone(), true).await?;
        let pin_order = store.history_items()[1].pinned;
        let (_, capture) = store.history_input(first.clone()).await?;
        store.history_touch(first.clone(), capture).await?;
        record(&store, "first pin", Some("Phone")).await?;
        let items = store.history_items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, second);
        assert_eq!(items[1].pinned, pin_order);
        assert_eq!(items[1].origin.as_deref(), Some("Phone"));
        for i in 0..55 {
            record(&store, &format!("recent {i}"), None).await?;
        }
        assert_eq!(store.history_items().len(), 52);
        assert_eq!(store.history_items()[1].id, first);
        store.history_configure(true, true).await?;
        assert_eq!(store.history_items()[0].id, second);
        let pending = store.history_capture().unwrap();
        store.history_delete(None).await?;
        store
            .history_record(ClipboardInput::Text("late".into()), None, pending)
            .await?;
        assert_eq!(store.history_items().len(), 2);
        store.history_delete(Some(second)).await?;
        assert_eq!(store.history_items().len(), 1);
        store.history_configure(false, false).await?;
        assert!(store.history_items().is_empty());
        Ok(())
    }
    #[tokio::test]
    async fn pin_cap_is_atomic_and_unpin_protects_the_oldest_item() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.history_configure(true, true).await?;
        let mut ids = Vec::new();
        for i in 0..PIN_LIMIT {
            record(&store, &format!("pin {i}"), None).await?;
            let id = store
                .history_items()
                .iter()
                .find(|i| i.pinned.is_none())
                .unwrap()
                .id
                .clone();
            store.history_pin(id.clone(), true).await?;
            ids.push(id);
        }
        record(&store, "26th", None).await?;
        let extra = store.history_items().last().unwrap().id.clone();
        let before = store.history_items();
        assert_eq!(
            store
                .history_pin(extra, true)
                .await
                .unwrap_err()
                .to_string(),
            PIN_LIMIT_ERROR
        );
        assert_eq!(store.history_items(), before);
        store.history_pin(ids[0].clone(), true).await?; // Idempotent at the cap.
        for i in 0..50 {
            record(&store, &format!("new {i}"), None).await?;
        }
        store.history_pin(ids[0].clone(), false).await?;
        let items = store.history_items();
        assert_eq!(items.iter().filter(|i| i.pinned.is_none()).count(), LIMIT);
        assert_eq!(items.last().unwrap().id, ids[0]);
        assert!(!items.iter().any(|i| i.title == "new 0"));
        store.history_pin(ids[0].clone(), true).await?;
        assert_eq!(store.history_items()[0].id, ids[0]);
        Ok(())
    }
    #[tokio::test]
    async fn v4_history_migration_preserves_content_sequence_and_images() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        let id = "a".repeat(32);
        {
            let store = Store::open(&path)?;
            store.history_configure(true, true).await?;
            record_image(&store, png(19, image::codecs::png::CompressionType::Fast)?).await?;
            store.locked(move |c| {
                c.execute_batch("ALTER TABLE clipboard_items DROP COLUMN pinned; PRAGMA user_version=4;")?;
                c.execute("INSERT INTO clipboard_items(seq,id,digest,text,origin,at) VALUES(42,?,'text:legacy','café','Phone',123)", [&id])?;
                Ok(())
            })?;
        }
        let store = Store::open(&path)?;
        store.history_configure(true, true).await?;
        let items = store.history_items();
        assert_eq!(items.len(), 2);
        assert!(items[1].image);
        assert!(items[1].thumbnail.as_ref().unwrap().is_file());
        let item = &items[0];
        assert_eq!(item.title, "café");
        assert_eq!(item.search, "cafe");
        assert_eq!(item.at, 123);
        assert_eq!(item.pinned, None);
        assert!(store.history_capture().unwrap().seq > 42);
        store.history_pin(item.id.clone(), true).await?;
        store.history_configure(true, true).await?;
        assert!(store.history_items()[0].pinned.is_some());
        Ok(())
    }
    #[tokio::test]
    async fn older_duplicate_completion_preserves_newer_time_and_origin() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.history_configure(true, true).await?;
        let older = store.history_capture().unwrap();
        let newer = store.history_capture().unwrap();
        let copied_at = newer.at;
        store
            .history_record(ClipboardInput::Text("same".into()), None, newer)
            .await?;
        let newest = store.history_items()[0].clone();
        assert_eq!(newest.at, copied_at);
        store
            .history_record(
                ClipboardInput::Text("same".into()),
                Some("Phone".into()),
                older,
            )
            .await?;
        assert_eq!(store.history_items(), vec![newest]);
        Ok(())
    }
    #[tokio::test]
    async fn delete_keeps_unrelated_pending_capture() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.history_configure(true, true).await?;
        record(&store, "A", None).await?;
        let a = store.history_items()[0].id.clone();
        let pending = store.history_capture().unwrap();
        store.history_delete(Some(a)).await?;
        store
            .history_record(ClipboardInput::Text("B".into()), None, pending)
            .await?;
        let items = store.history_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "B");
        Ok(())
    }
    #[tokio::test]
    async fn delete_fences_same_content_only_until_a_new_copy() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.history_configure(true, true).await?;
        record(&store, "X", None).await?;
        let x = store.history_items()[0].id.clone();
        let pending = store.history_capture().unwrap();
        store.history_delete(Some(x)).await?;
        store
            .history_record(
                ClipboardInput::Text("X".into()),
                Some("old".into()),
                pending.clone(),
            )
            .await?;
        assert!(store.history_items().is_empty());
        record(&store, "X", None).await?;
        assert_eq!(store.history_items().len(), 1);
        assert_eq!(store.history_items()[0].origin, None);
        let newest = store.history_items();
        store
            .history_record(
                ClipboardInput::Text("X".into()),
                Some("old".into()),
                pending,
            )
            .await?;
        assert_eq!(store.history_items(), newest);
        Ok(())
    }
    #[tokio::test]
    async fn late_history_selection_does_not_supersede_a_newer_duplicate() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.history_configure(true, true).await?;
        record(&store, "same", Some("Phone")).await?;
        let id = store.history_items()[0].id.clone();
        let (_, pending) = store.history_input(id.clone()).await?;
        record(&store, "same", None).await?;
        let newest = store.history_items();
        store.history_touch(id, pending).await?;
        assert_eq!(store.history_items(), newest);
        Ok(())
    }
    #[tokio::test]
    async fn cap_exact_text_dedupe_moves_to_top_and_replaces_origin() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.history_configure(true, true).await?;
        for n in 0..51 {
            record(&store, &format!("item {n}"), Some("Phone")).await?;
        }
        let items = store.history_items();
        assert_eq!(items.len(), 50);
        assert_eq!(items[49].title, "item 1");
        let id = items[49].id.clone();
        record(&store, "item 1", None).await?;
        let items = store.history_items();
        assert_eq!(items.len(), 50);
        assert_eq!(items[0].id, id);
        assert_eq!(items[0].origin, None);
        assert!(items[0].at >= items[1].at);
        record(&store, "item 1 ", Some("Mac")).await?;
        assert_eq!(store.history_items().len(), 50);
        assert_ne!(store.history_items()[0].id, id);
        Ok(())
    }
    #[tokio::test]
    async fn image_pixels_dedupe_and_delete_clear_disable_remove_files() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.history_configure(true, true).await?;
        record_image(&store, png(10, image::codecs::png::CompressionType::Fast)?).await?;
        let id = store.history_items()[0].id.clone();
        assert_eq!(fs::read_dir(store.history_folder())?.count(), 2);
        record(&store, "text", None).await?;
        record_image(&store, png(10, image::codecs::png::CompressionType::Best)?).await?;
        assert_eq!(store.history_items().len(), 2);
        assert_eq!(store.history_items()[0].id, id);
        store.history_delete(Some(id)).await?;
        assert_eq!(fs::read_dir(store.history_folder())?.count(), 0);
        record_image(&store, png(20, image::codecs::png::CompressionType::Fast)?).await?;
        let stale = store.history_capture().unwrap();
        store.history_delete(None).await?;
        store
            .history_record(ClipboardInput::Text("late old read".into()), None, stale)
            .await?;
        assert!(store.history_items().is_empty());
        assert_eq!(fs::read_dir(store.history_folder())?.count(), 0);
        record_image(&store, png(30, image::codecs::png::CompressionType::Fast)?).await?;
        let stale = store.history_capture().unwrap();
        store.history_configure(false, false).await?;
        assert!(store.history_items().is_empty());
        assert_eq!(fs::read_dir(store.history_folder())?.count(), 0);
        store.history_configure(true, false).await?;
        store
            .history_record(
                ClipboardInput::Text("old before reenable".into()),
                None,
                stale,
            )
            .await?;
        assert!(store.history_items().is_empty());
        Ok(())
    }
    #[tokio::test]
    async fn size_limits_whitespace_restart_missing_data_and_orphan_cleanup() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        let store = Store::open(&path)?;
        store.history_configure(true, true).await?;
        record(&store, " \t\n\u{2003}", None).await?;
        record(&store, &"x".repeat(MAX_TEXT + 1), None).await?;
        let mut oversized = png(55, image::codecs::png::CompressionType::Fast)?;
        oversized.resize(MAX_IMAGE + 1, 0);
        assert!(crate::actions::decode_image(&oversized).is_ok());
        record_image(&store, oversized).await?;
        assert!(store.history_items().is_empty());
        record(&store, &"x".repeat(MAX_TEXT), Some("My phone")).await?;
        record_image(&store, png(10, image::codecs::png::CompressionType::Fast)?).await?;
        let image = store.history_items()[0].clone();
        fs::remove_file(image_path(&store.history_folder(), &image.id, false))?;
        fs::write(store.history_folder().join("orphan.png"), b"orphan")?;
        store.drain().await;
        drop(store);
        let store = Store::open(&path)?;
        store.history_configure(true, true).await?;
        let items = store.history_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].origin.as_deref(), Some("My phone"));
        assert_eq!(fs::read_dir(store.history_folder())?.count(), 0);
        let (ClipboardInput::Text(text), _) = store.history_input(items[0].id.clone()).await?
        else {
            panic!("text history")
        };
        assert_eq!(text.len(), MAX_TEXT);
        Ok(())
    }
    #[tokio::test]
    async fn cap_eviction_deletes_original_and_thumbnail() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.history_configure(true, true).await?;
        record_image(&store, png(10, image::codecs::png::CompressionType::Fast)?).await?;
        for n in 0..50 {
            record(&store, &format!("text {n}"), None).await?;
        }
        assert_eq!(store.history_items().len(), 50);
        assert_eq!(fs::read_dir(store.history_folder())?.count(), 0);
        Ok(())
    }
    #[tokio::test]
    async fn history_completion_does_not_wait_for_receive_admission_capacity() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.history_configure(true, true).await?;
        let mut slots = Vec::new();
        for _ in 0..crate::work::BLOCKING_LIMIT {
            slots.push(store.reserve().await);
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            record(&store, "admitted clipboard", None).await?;
            let item = store.history_items()[0].clone();
            store
                .history_touch(item.id.clone(), store.history_capture().unwrap())
                .await?;
            store.history_delete(Some(item.id)).await?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        drop(slots);
        store.drain().await;
        Ok(())
    }
}
