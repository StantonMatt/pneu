//! Opt-in user-visible clipboard history. Independent of protocol echo receipts.
use super::*;
use crate::actions::ClipboardInput;
use image::GenericImageView;

pub(crate) const MAX_TEXT: usize = 1024 * 1024;
pub(crate) const MAX_IMAGE: usize = 20 * 1024 * 1024;
const LIMIT: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub title: String,
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
    items: Vec<Item>,
}
pub(super) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS clipboard_items(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,digest TEXT UNIQUE NOT NULL,text TEXT,origin TEXT,at INTEGER NOT NULL);")?;
    Ok(())
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
        .prepare("SELECT id,text,origin,at FROM clipboard_items ORDER BY seq DESC")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)? as u64,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut items = Vec::new();
    for (id, text, origin, at) in rows {
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
    pub(crate) fn history_generation(&self) -> Option<u64> {
        let state = self.history.lock().expect("clipboard history");
        state.enabled.then_some(state.generation)
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
            state.generation = state.generation.wrapping_add(1);
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
        generation: u64,
    ) -> Result<()> {
        let gate = self.history_gate.clone().lock_owned().await;
        if self.history_generation() != Some(generation) {
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
                let id = c
                    .query_row(
                        "SELECT id FROM clipboard_items WHERE digest=?",
                        [&digest],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .unwrap_or_else(|| crate::random_hex(16));
                if let Some(png) = png {
                    crate::config::atomic_private(&image_path(&folder, &id, false), &png)?;
                }
                if let Some(thumb) = thumbnail {
                    crate::config::atomic_private(&image_path(&folder, &id, true), &thumb)?;
                }
                let tx = c.transaction()?;
                tx.execute("DELETE FROM clipboard_items WHERE id=?", [&id])?;
                tx.execute(
                    "INSERT INTO clipboard_items(id,digest,text,origin,at) VALUES (?,?,?,?,?)",
                    params![id, digest, text, origin, crate::now_ms() as i64],
                )?;
                let old = tx
                    .prepare("SELECT id FROM clipboard_items ORDER BY seq DESC LIMIT -1 OFFSET ?")?
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
        self.history.lock().expect("clipboard history").generation += 1;
        let folder = self.history_folder();
        let state = self.history.clone();
        self.history_call(move |c| {
            let _gate = gate;
            if let Some(id) = id {
                c.execute("DELETE FROM clipboard_items WHERE id=?", [&id])?;
                remove_images(&folder, &id)?;
            } else {
                clear(c, &folder)?;
            }
            let items = load(c, &folder)?;
            state.lock().expect("clipboard history").items = items;
            Ok(())
        })
        .await
    }
    pub(crate) async fn history_input(&self, id: String) -> Result<(ClipboardInput, u64)> {
        ensure!(valid_id(&id), "invalid history id");
        let gate = self.history_gate.clone().lock_owned().await;
        let generation = self
            .history_generation()
            .context("clipboard history is off")?;
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
        Ok((input, generation))
    }
    pub(crate) async fn history_touch(&self, id: String, generation: u64) -> Result<()> {
        let gate = self.history_gate.clone().lock_owned().await;
        if self.history_generation() != Some(generation) {
            return Ok(());
        }
        let folder = self.history_folder();
        let state = self.history.clone();
        self.history_call(move|c| {
            let _gate=gate;
            let tx=c.transaction()?;
            tx.execute("UPDATE clipboard_items SET seq=(SELECT coalesce(max(seq),0)+1 FROM clipboard_items),at=? WHERE id=?",params![crate::now_ms() as i64,id])?;
            tx.commit()?;
            let items=load(c,&folder)?;
            state.lock().expect("clipboard history").items=items;
            Ok(())
        }).await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    async fn record(store: &Store, text: &str, origin: Option<&str>) -> Result<()> {
        store
            .history_record(
                ClipboardInput::Text(text.into()),
                origin.map(str::to_owned),
                store.history_generation().context("enabled history")?,
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
                store.history_generation().unwrap(),
            )
            .await
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
        let stale = store.history_generation().unwrap();
        store.history_delete(None).await?;
        store
            .history_record(ClipboardInput::Text("late old read".into()), None, stale)
            .await?;
        assert!(store.history_items().is_empty());
        assert_eq!(fs::read_dir(store.history_folder())?.count(), 0);
        record_image(&store, png(30, image::codecs::png::CompressionType::Fast)?).await?;
        let stale = store.history_generation().unwrap();
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
                .history_touch(item.id.clone(), store.history_generation().unwrap())
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
