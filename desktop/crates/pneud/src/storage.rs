//! Descriptor-relative receive storage. Pathnames are display/recovery hints;
//! live staging, placement and deletion stay anchored to opened directories.
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::platform::fs;
pub(crate) use fs::{Folder, open_regular_file, remove_directory, remove_file};

pub(crate) const STAGING_DIR: &str = ".pneu-staging";
pub(crate) const CACHE_PREFIX: &str = ".pneu-cache-";
const NAME_MAX: usize = 255;
const NAME_ATTEMPTS: usize = 10_000;

fn name_parts(name: &str) -> (&str, &str) {
    name.rsplit_once('.')
        .filter(|(stem, ext)| !stem.is_empty() && ext.len() < 32)
        .map(|(stem, _)| (stem, &name[stem.len()..]))
        .unwrap_or((name, ""))
}

fn truncate_utf8(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(crate) fn safe_basename(name: &str, max: usize) -> String {
    let (stem, extension) = name_parts(name);
    format!(
        "{}{extension}",
        truncate_utf8(stem, max.saturating_sub(extension.len()))
    )
}

pub(crate) fn cache_basename(name: &str) -> String {
    format!(
        "{CACHE_PREFIX}{}-{}",
        crate::random_hex(16),
        safe_basename(name, 200)
    )
}

fn numbered_name(name: &str, number: usize) -> String {
    let (stem, extension) = name_parts(name);
    let suffix = format!(" ({number})");
    format!(
        "{}{suffix}{extension}",
        truncate_utf8(stem, NAME_MAX - suffix.len() - extension.len())
    )
}
/// Startup-only sweep. Never called while a transfer owns a live stage.
pub(crate) fn clean_staging(folder: &Folder) -> Result<()> {
    clean_legacy_staging(folder)?;
    match fs::open_directory_at(&folder.file, STAGING_DIR) {
        Ok(fd) => {
            let directory = owned_staging(fd)?;
            wipe(&directory.file)?;
            directory.file.sync_all()?;
        }
        Err(error) if fs::is_missing(&error) => {}
        Err(e) => return Err(e),
    }
    Ok(())
}
/// Retire the old per-transfer private directories too, including ones that
/// crashed before journal registration. Reserved names alone are insufficient:
/// require an owned private directory, and never follow symlinks.
pub(crate) fn clean_legacy_staging(folder: &Folder) -> Result<()> {
    for entry in fs::entries(&folder.file)? {
        let name = entry?;
        let name = name.as_os_str();
        let bytes = name.as_encoded_bytes();
        let Some(suffix) = bytes.strip_prefix(b".pneu-stage-") else {
            continue;
        };
        if suffix.len() != 32 || !suffix.iter().all(u8::is_ascii_hexdigit) {
            continue;
        }
        let fd = match fs::open_directory_at(&folder.file, name) {
            Ok(fd) => fd,
            Err(error)
                if fs::is_link(&error)
                    || fs::is_not_directory(&error)
                    || fs::is_missing(&error) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        let directory = fd;
        if !fs::is_private_directory(&directory)? {
            continue;
        }
        crate::platform::private::check(&directory)?;
        wipe(&directory)?;
        remove_directory(&folder.file, name, &directory)?;
    }
    folder.file.sync_all()?;
    Ok(())
}
pub(crate) fn staging_directory(folder: &Folder) -> Result<Folder> {
    let created = match fs::create_private_directory_at(&folder.file, STAGING_DIR) {
        Ok(()) => {
            folder.file.sync_all()?;
            true
        }
        Err(error) if fs::is_exists(&error) => false,
        Err(e) => return Err(e),
    };
    let file = fs::open_directory_at(&folder.file, STAGING_DIR)?;
    if created {
        crate::platform::private::created(&file)?;
    }
    owned_staging(file)
}
fn owned_staging(file: File) -> Result<Folder> {
    crate::platform::private::check(&file)?;
    ensure!(
        fs::is_owned_private(&file)?,
        "staging directory is not owned and private"
    );
    // Existing roots are opened and checked, never chmodded during cleanup or
    // transfer admission. Only mkdir sets permissions on newly created roots.
    Ok(Folder {
        file: Arc::new(file),
    })
}

pub(crate) fn wipe(directory: &File) -> Result<()> {
    for entry in fs::entries(directory)? {
        let name = entry?;
        let name = name.as_os_str();
        if name.as_encoded_bytes() == b"." || name.as_encoded_bytes() == b".." {
            continue;
        }
        if fs::is_directory_at(directory, name)? {
            let child = fs::open_directory_at(directory, name)?;
            crate::platform::private::check(&child)?;
            wipe(&child)?;
            // The tree is daemon-owned, but still never follow swapped entries.
            let own = fs::identity(&child)?;
            let named = fs::identity_at(directory, name)?;
            ensure!(
                own == named,
                "staging directory was replaced during cleanup"
            );
            fs::unlink_directory(directory, name)?;
        } else {
            let file = fs::metadata_file(directory, name)?;
            if file.metadata()?.is_file() {
                crate::platform::private::check(&file)?;
            }
            remove_file(directory, name, &file)?;
        }
    }
    Ok(())
}
#[derive(Clone, Debug)]
pub(crate) struct Stage(pub Arc<StageInner>);
#[derive(Debug)]
pub(crate) struct Cleanup {
    pub store: crate::store::Store,
    pub _work: crate::work::Token,
}
#[derive(Debug)]
pub(crate) struct StageInner {
    pub folder: Folder,
    pub directory: Folder,
    pub file: File,
    pub name: String,
    pub destination: String,
    placed_name: Mutex<Option<String>>,
    pub cleanup: Mutex<Option<Cleanup>>,
}
impl Stage {
    pub fn create(
        folder: Folder,
        directory: Folder,
        destination: String,
        size: u64,
    ) -> Result<Self> {
        // Reject an ambiguous derived/display name before staging or exposing
        // it to a path-based desktop API. Local filename mapping is a later
        // milestone; this slice deliberately fails closed on invalid names.
        #[cfg(windows)]
        let destination = safe_basename(&destination, NAME_MAX);
        #[cfg(windows)]
        fs::validate_name(std::ffi::OsStr::new(&destination))?;
        ensure!(
            folder.space()? >= size.saturating_add(16 * 1024 * 1024),
            fs::no_space()
        );
        let name = crate::random_hex(16);
        let file = fs::create_private_at(&directory.file, &name, fs::Access::ReadWrite)?;
        crate::platform::private::created(&file)?;
        Ok(Self(Arc::new(StageInner {
            folder,
            directory,
            file,
            name,
            destination,
            placed_name: Mutex::new(None),
            cleanup: Mutex::new(None),
        })))
    }
    pub fn record_placed(&self) -> Result<OwnedFile> {
        Ok(OwnedFile {
            folder: self.0.folder.clone(),
            name: self
                .0
                .placed_name
                .lock()
                .expect("placed name")
                .clone()
                .context("file has not been placed")?,
            #[cfg(windows)]
            identity: fs::Version::record_written(&self.0.file)?,
            #[cfg(unix)]
            identity: fs::Version::read_regular(&self.0.file)?,
        })
    }
    pub fn destination_path(&self) -> Result<PathBuf> {
        let placed = self.0.placed_name.lock().expect("placed name");
        Ok(self
            .0
            .folder
            .path()?
            .join(placed.as_ref().unwrap_or(&self.0.destination)))
    }
    #[cfg(test)]
    pub fn path(&self) -> Result<PathBuf> {
        Ok(self.0.directory.path()?.join(&self.0.name))
    }
    pub fn cleanup(&self) -> Result<()> {
        remove_file(&self.0.directory.file, &self.0.name, &self.0.file)?;
        self.0.directory.file.sync_all()?;
        self.0.cleanup.lock().expect("stage cleanup").take();
        Ok(())
    }
    pub fn place(&self) -> Result<()> {
        // Windows rename needs only DELETE access so frozen read-only files
        // remain publishable. Flush data on the retained writer beforehand.
        #[cfg(windows)]
        self.0.file.sync_all()?;
        let mut placed_name = self.0.placed_name.lock().expect("placed name");
        ensure!(placed_name.is_none(), "file has already been placed");
        let before = fs::identity(&self.0.file)?;
        let named = fs::identity_at(&self.0.directory.file, &self.0.name)?;
        ensure!(before == named, "staging file was replaced");
        // Only the no-replace rename decides whether a name is free. Every
        // contender is relative to the retained folder FD, including fallback.
        for attempt in 1..=NAME_ATTEMPTS + 16 {
            let candidate = if attempt == 1 {
                safe_basename(&self.0.destination, NAME_MAX)
            } else if attempt <= NAME_ATTEMPTS {
                numbered_name(&self.0.destination, attempt)
            } else {
                // Cache fallback names must remain in the disposable namespace.
                let (namespace, basename) = self
                    .0
                    .destination
                    .strip_prefix(CACHE_PREFIX)
                    .map(|basename| (CACHE_PREFIX, basename))
                    .unwrap_or(("", self.0.destination.as_str()));
                let prefix = format!("{}-", crate::random_hex(16));
                format!(
                    "{namespace}{prefix}{}",
                    safe_basename(basename, NAME_MAX - namespace.len() - prefix.len())
                )
            };
            match fs::rename(
                &self.0.directory.file,
                &self.0.name,
                &self.0.folder.file,
                &candidate,
                false,
            ) {
                Ok(()) => {
                    *placed_name = Some(candidate);
                    break;
                }
                Err(error) if fs::is_exists(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        let destination = placed_name
            .as_ref()
            .context("no available destination name")?;
        let placed = fs::identity_at(&self.0.folder.file, destination)?;
        ensure!(before == placed, "placed file was replaced");
        self.0.folder.file.sync_all()?;
        Ok(())
    }
}
impl Drop for StageInner {
    fn drop(&mut self) {
        let directory = self.directory.clone();
        let name = self.name.clone();
        // If FD duplication fails, startup can sweep the disposable stage;
        // never delete a pathname without the original ownership evidence.
        let Ok(file) = self.file.try_clone() else {
            return;
        };
        let clean = move || {
            remove_file(&directory.file, &name, &file)?;
            directory.file.sync_all()?;
            Ok(())
        };
        if let Some(cleanup) = self.cleanup.get_mut().ok().and_then(Option::take) {
            if tokio::runtime::Handle::try_current().is_ok() {
                // Register cleanup before releasing the stage's work token:
                // even unobserved blocking-task results cannot escape drain().
                let token = cleanup._work;
                drop(cleanup.store.start_cleanup(move |_| {
                    let _lifecycle = token;
                    clean()
                }));
                return;
            }
            let _ = clean();
            return;
        }
        let _ = clean();
    }
}
/// Recorded ownership evidence without an open file that could pin unlinked
/// cache blocks. Keep only the directory anchor; reopen without following links
/// and verify identity and unchanged metadata when retirement is requested.
pub(crate) struct OwnedFile {
    folder: Folder,
    name: String,
    identity: fs::Version,
}
impl OwnedFile {
    /// A journal path is reusable only while it names the original directory
    /// and the unchanged regular file. Never follow a replaced ancestor/link.
    pub fn matches_path(&self, path: &Path) -> Result<bool> {
        if path.file_name() != Some(std::ffi::OsStr::new(&self.name)) {
            return Ok(false);
        }
        let folder = Folder::open(path.parent().context("cache parent")?, false)?;
        if folder.identity()? != self.folder.identity()? {
            return Ok(false);
        }
        Ok(self.open_matching()?.is_some())
    }
    fn open_matching(&self) -> Result<Option<File>> {
        let file = match fs::open_read_at(&self.folder.file, &self.name) {
            Ok(file) => file,
            Err(error) if fs::is_missing(&error) || fs::is_link(&error) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok((fs::Version::read_regular(&file)? == self.identity).then_some(file))
    }
    pub fn remove(self) -> Result<()> {
        let Some(file) = self.open_matching()? else {
            return Ok(());
        };
        remove_file(&self.folder.file, &self.name, &file)?;
        self.folder.file.sync_all()?;
        Ok(())
    }
}
/// The private cache namespace identifies disposable entries. Legacy received-*
/// names are ambiguous and must never authorize deletion of a saved file.
/// Retention policy selects old regular cache files, then anchors its checks and
/// deletion to opened descriptors. This is also the restart orphan sweep.
pub(crate) fn prune_cache(folder: &Folder, cutoff: std::time::SystemTime) -> Result<()> {
    for entry in fs::entries(&folder.file)? {
        let name = entry?;
        let name = name.as_os_str();
        if !name.as_encoded_bytes().starts_with(CACHE_PREFIX.as_bytes()) {
            continue;
        }
        let file = match fs::open_read_at(&folder.file, name) {
            Ok(file) => file,
            Err(error) if fs::is_missing(&error) || fs::is_link(&error) => continue,
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if meta.is_file() && meta.modified()? < cutoff {
            crate::platform::private::check(&file)?;
            remove_file(&folder.file, name, &file)?;
        }
    }
    folder.file.sync_all()?;
    Ok(())
}
/// Evidence from the receive stream, bound to the exact declared metadata.
/// Commit never reads the file contents again; it checks the on-disk length.
pub(crate) struct VerifiedFile(pneu_proto::FileBegin);
impl VerifiedFile {
    pub fn from_stream(
        begin: pneu_proto::FileBegin,
        received: u64,
        digest: impl AsRef<[u8]>,
    ) -> std::result::Result<Self, pneu_proto::NackCode> {
        if received != begin.size {
            return Err(pneu_proto::NackCode::SizeMismatch);
        }
        if hex::encode(digest) != begin.sha256.as_str() {
            return Err(pneu_proto::NackCode::HashMismatch);
        }
        Ok(Self(begin))
    }
    pub fn metadata(self) -> pneu_proto::FileBegin {
        self.0
    }
    #[cfg(test)]
    pub fn fixture(begin: pneu_proto::FileBegin) -> Self {
        Self(begin)
    }
}

/// A descriptor-bound size check plus a hard read cap also covers growth after
/// stat. Used for peer-derived images and persisted control/identity JSON.
pub(crate) fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = fs::open_read(path)?;
    crate::platform::private::check(&file)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= limit as u64,
        "input exceeds bounded read limit"
    );
    read_limited(file, limit)
}
fn read_limited(reader: impl std::io::Read, limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "input grew beyond bounded read limit");
    Ok(bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    fn named_stage(folder: &Folder, name: &str, bytes: &[u8]) -> Result<Stage> {
        use std::io::Write;
        let stage = Stage::create(
            folder.clone(),
            staging_directory(folder)?,
            name.into(),
            bytes.len() as u64,
        )?;
        (&stage.0.file).write_all(bytes)?;
        Ok(stage)
    }
    #[test]
    fn placement_keeps_names_and_numbers_collisions_without_overwriting() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let folder = Folder::open(dir.path(), false)?;
        for (name, second, third) in [
            ("photo.jpg", "photo (2).jpg", "photo (3).jpg"),
            ("README", "README (2)", "README (3)"),
            ("archive.tar.gz", "archive.tar (2).gz", "archive.tar (3).gz"),
            (".hidden", ".hidden (2)", ".hidden (3)"),
            ("café-写真.png", "café-写真 (2).png", "café-写真 (3).png"),
        ] {
            // Stages predate all placements: naming must be decided at rename.
            let stages = [
                named_stage(&folder, name, b"first")?,
                named_stage(&folder, name, b"second")?,
                named_stage(&folder, name, b"third")?,
            ];
            for ((stage, expected), contents) in stages.iter().zip([name, second, third]).zip([
                b"first".as_slice(),
                b"second",
                b"third",
            ]) {
                stage.place()?;
                assert_eq!(stage.destination_path()?, dir.path().join(expected));
                assert_eq!(std::fs::read(stage.destination_path()?)?, contents);
                assert!(!stage.path()?.exists());
            }
            // Ownership evidence must also follow the actual collision name.
            stages[1].record_placed()?.remove()?;
            assert!(!dir.path().join(second).exists());
            assert_eq!(std::fs::read(dir.path().join(name))?, b"first");
            assert_eq!(std::fs::read(dir.path().join(third))?, b"third");
        }
        Ok(())
    }
    #[cfg(windows)]
    #[test]
    fn placed_versions_survive_writer_close_and_preserve_later_changes() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let folder = Folder::open(dir.path(), false)?;
        let mut records = Vec::new();
        for change in ["none", "edit", "restored_mtime", "replace", "rename"] {
            // Every placement after the first uses the collision-name branch.
            let stage = named_stage(&folder, "preview.bin", b"owned")?;
            let writer = stage.0.file.try_clone()?;
            stage.place()?;
            let path = stage.destination_path()?;
            let ownership = stage.record_placed()?;
            assert!(ownership.matches_path(&path)?, "before close: {change}");
            records.push((stage, writer, path, ownership, change));
        }
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let records: Vec<_> = records
            .into_iter()
            .map(|(stage, writer, path, owner, change)| {
                drop(stage);
                drop(writer);
                (path, owner, change)
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(2100));
        for (path, ownership, change) in records {
            assert!(ownership.matches_path(&path)?, "after close: {change}");
            let modified = std::fs::metadata(&path)?.modified()?;
            match change {
                "edit" | "restored_mtime" => {
                    std::fs::write(&path, b"other")?; // same length
                    if change == "restored_mtime" {
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open(&path)?
                            .set_times(std::fs::FileTimes::new().set_modified(modified))?;
                    }
                }
                "replace" => {
                    std::fs::remove_file(&path)?;
                    std::fs::write(&path, b"other")?;
                }
                "rename" => {
                    let moved = path.with_extension("moved");
                    std::fs::rename(&path, &moved)?;
                    std::fs::rename(moved, &path)?;
                }
                _ => {}
            }
            assert_eq!(ownership.matches_path(&path)?, change == "none", "{change}");
            ownership.remove()?;
            assert_eq!(path.exists(), change != "none", "{change}");
        }
        Ok(())
    }
    #[test]
    fn placement_bounds_long_ascii_and_unicode_collision_names() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let folder = Folder::open(dir.path(), false)?;
        for name in [
            format!("{}.jpg", "a".repeat(251)),
            "b".repeat(255),
            format!("{}xx.png", "界".repeat(83)),
        ] {
            assert_eq!(name.len(), NAME_MAX);
            for number in 1..=3 {
                let stage = named_stage(&folder, &name, &[number])?;
                stage.place()?;
                let path = stage.destination_path()?;
                let actual = path.file_name().unwrap().to_str().unwrap();
                assert!(actual.len() <= NAME_MAX);
                let (stem, extension) = name_parts(actual);
                assert_eq!(extension, name_parts(&name).1);
                if number == 1 {
                    assert_eq!(actual, name);
                } else {
                    assert!(stem.ends_with(&format!(" ({number})")));
                }
                assert_eq!(std::fs::read(path)?, [number]);
            }
        }
        // Larger counters reserve their entire suffix before UTF-8 truncation.
        let numbered = numbered_name(&format!("{}.png", "界".repeat(83)), NAME_ATTEMPTS);
        assert!(numbered.len() <= NAME_MAX);
        assert!(numbered.ends_with(" (10000).png"));
        Ok(())
    }
    #[test]
    fn simultaneous_placements_atomically_select_distinct_names() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let folder = Folder::open(dir.path(), false)?;
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let stages = (0..16)
            .map(|n| named_stage(&folder, "shared.txt", &[n]))
            .collect::<Result<Vec<_>>>()?;
        let paths = std::thread::scope(|scope| {
            let jobs = stages
                .iter()
                .map(|stage| {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        stage.place()?;
                        stage.destination_path()
                    })
                })
                .collect::<Vec<_>>();
            jobs.into_iter()
                .map(|job| job.join().expect("placement thread"))
                .collect::<Result<Vec<_>>>()
        })?;
        assert_eq!(
            paths.iter().collect::<std::collections::HashSet<_>>().len(),
            16
        );
        for (n, path) in paths.iter().enumerate() {
            assert_eq!(std::fs::read(path)?, [n as u8]);
        }
        assert!(dir.path().join("shared.txt").exists());
        for n in 2..=16 {
            assert!(dir.path().join(format!("shared ({n}).txt")).exists());
        }
        Ok(())
    }
    #[test]
    #[cfg(unix)]
    fn placement_retries_late_collisions_against_the_opened_directory() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let save = dir.path().join("save");
        let moved = dir.path().join("moved");
        let folder = Folder::open(&save, true)?;
        let stage = named_stage(&folder, "photo.jpg", b"received")?;
        std::fs::rename(&save, &moved)?;
        std::fs::create_dir(&save)?;
        std::fs::write(save.join("photo.jpg"), b"replacement")?;
        // A directory and symlink count as occupied names too.
        std::fs::create_dir(moved.join("photo.jpg"))?;
        std::os::unix::fs::symlink(save.join("photo.jpg"), moved.join("photo (2).jpg"))?;
        stage.place()?;
        assert_eq!(stage.destination_path()?, moved.join("photo (3).jpg"));
        assert_eq!(std::fs::read(stage.destination_path()?)?, b"received");
        assert_eq!(std::fs::read(save.join("photo.jpg"))?, b"replacement");
        assert!(moved.join("photo.jpg").is_dir());
        assert!(moved.join("photo (2).jpg").is_symlink());
        Ok(())
    }
    #[test]
    fn exhausted_numbered_names_use_a_bounded_random_prefix() -> Result<()> {
        for namespace in ["", CACHE_PREFIX] {
            let dir = tempfile::tempdir()?;
            let folder = Folder::open(dir.path(), false)?;
            let name = format!("{namespace}{}.jpg", "a".repeat(251 - namespace.len()));
            let stage = named_stage(&folder, &name, b"received")?;
            let occupied = dir.path().join("occupied");
            std::fs::write(&occupied, b"keep")?;
            // Hard links occupy all candidates without allocating 10,000 files.
            for number in 1..=NAME_ATTEMPTS {
                let candidate = if number == 1 {
                    name.clone()
                } else {
                    numbered_name(&name, number)
                };
                #[cfg(unix)]
                std::fs::hard_link(&occupied, dir.path().join(candidate))?;
                #[cfg(windows)]
                std::fs::write(dir.path().join(candidate), b"keep")?;
            }
            stage.place()?;
            let path = stage.destination_path()?;
            let actual = path.file_name().unwrap().to_str().unwrap();
            assert_eq!(actual.len(), NAME_MAX);
            let actual = actual
                .strip_prefix(namespace)
                .expect("cache namespace retained");
            assert!(actual[..32].bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_eq!(&actual[32..33], "-");
            assert!(actual.ends_with(".jpg"));
            assert_eq!(std::fs::read(path)?, b"received");
            assert_eq!(std::fs::read(dir.path().join(name))?, b"keep");
            assert_eq!(
                std::fs::read(
                    dir.path()
                        .join(numbered_name(&stage.0.destination, NAME_ATTEMPTS))
                )?,
                b"keep"
            );
        }
        Ok(())
    }
    #[test]
    #[cfg(unix)]
    fn cache_pruning_only_removes_old_private_cache_entries_and_is_anchored() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = dir.path().join("cache");
        std::fs::create_dir(&cache)?;
        let folder = Folder::open(&cache, false)?;
        let old = cache_basename("old.jpg");
        let new = cache_basename("new.jpg");
        let link = cache_basename("link.jpg");
        let directory = cache_basename("directory");
        std::fs::write(cache.join(&old), b"old")?;
        File::open(cache.join(&old))?
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))?;
        std::fs::write(cache.join(&new), b"new")?;
        std::fs::write(cache.join("received-photo.jpg"), b"saved")?;
        File::open(cache.join("received-photo.jpg"))?
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))?;
        std::fs::write(cache.join("unrelated"), b"keep")?;
        std::os::unix::fs::symlink(cache.join("unrelated"), cache.join(&link))?;
        std::fs::create_dir(cache.join(&directory))?;
        let moved = dir.path().join("moved");
        std::fs::rename(&cache, &moved)?;
        std::fs::create_dir(&cache)?;
        std::fs::write(cache.join(&old), b"foreign")?;
        prune_cache(
            &folder,
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1),
        )?;
        assert!(!moved.join(&old).exists());
        assert_eq!(std::fs::read(moved.join(&new))?, b"new");
        assert_eq!(std::fs::read(moved.join("received-photo.jpg"))?, b"saved");
        assert_eq!(std::fs::read(moved.join("unrelated"))?, b"keep");
        assert!(
            std::fs::symlink_metadata(moved.join(&link))?
                .file_type()
                .is_symlink()
        );
        assert!(moved.join(&directory).is_dir());
        assert_eq!(std::fs::read(cache.join(&old))?, b"foreign");
        Ok(())
    }
    #[test]
    #[cfg(unix)]
    fn cleanup_and_reuse_never_chmod_existing_staging_roots() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let folder = Folder::open(dir.path(), false)?;
        let staging = staging_directory(&folder)?;
        std::fs::set_permissions(
            dir.path().join(STAGING_DIR),
            std::fs::Permissions::from_mode(0o500),
        )?;
        clean_staging(&folder)?;
        staging_directory(&folder)?;
        assert_eq!(staging.file.metadata()?.permissions().mode() & 0o777, 0o500);
        // A publicly writable root is refused without silently changing it.
        std::fs::set_permissions(
            dir.path().join(STAGING_DIR),
            std::fs::Permissions::from_mode(0o777),
        )?;
        assert!(clean_staging(&folder).is_err());
        assert!(staging_directory(&folder).is_err());
        assert_eq!(staging.file.metadata()?.permissions().mode() & 0o777, 0o777);
        std::fs::set_permissions(
            dir.path().join(STAGING_DIR),
            std::fs::Permissions::from_mode(0o700),
        )?;
        Ok(())
    }
    #[test]
    fn streamed_verification_checks_both_count_and_digest() {
        use sha2::{Digest, Sha256};
        let begin = pneu_proto::FileBegin {
            hlc: None,
            source_device: None,
            transfer_id: pneu_proto::Id::new(crate::random_hex(16)).unwrap(),
            file_name: "file".into(),
            mime: "application/octet-stream".into(),
            size: 3,
            sha256: pneu_proto::Hash::new(hex::encode(Sha256::digest(b"abc"))).unwrap(),
            kind: pneu_proto::FileKind::File,
            created_at: 0,
            source_media_id: None,
        };
        assert!(VerifiedFile::from_stream(begin.clone(), 3, Sha256::digest(b"abc")).is_ok());
        assert!(matches!(
            VerifiedFile::from_stream(begin.clone(), 2, Sha256::digest(b"abc")),
            Err(pneu_proto::NackCode::SizeMismatch)
        ));
        assert!(matches!(
            VerifiedFile::from_stream(begin, 3, Sha256::digest(b"def")),
            Err(pneu_proto::NackCode::HashMismatch)
        ));
    }
    #[test]
    fn bounded_reader_rejects_growth_and_bounds_allocation() {
        assert!(read_limited(std::io::repeat(0).take(1000), 8).is_err());
        assert_eq!(read_limited(&b"tiny"[..], 8).unwrap(), b"tiny");
    }
    #[test]
    #[cfg(unix)]
    fn missing_ancestors_are_created_without_changing_existing_permissions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared)?;
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o775))?;
        let selected = shared.join("new/nested/saved");
        let folder = Folder::open(&selected, true)?;
        assert_eq!(
            folder.identity()?,
            Folder::open(&selected, false)?.identity()?
        );
        assert_eq!(
            std::fs::metadata(&shared)?.permissions().mode() & 0o777,
            0o775
        );
        let owned = shared.join("daemon/a/b");
        Folder::open_private(&owned)?;
        for path in [shared.join("daemon"), shared.join("daemon/a"), owned] {
            assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o700);
        }
        assert_eq!(
            std::fs::metadata(shared)?.permissions().mode() & 0o777,
            0o775
        );
        Ok(())
    }
}
