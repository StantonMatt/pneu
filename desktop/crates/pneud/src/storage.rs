//! Descriptor-relative receive storage. Pathnames are display/recovery hints;
//! live staging, placement and deletion stay anchored to opened directories.
use anyhow::{Context, Result, ensure};
use rustix::fs::{AtFlags, Mode, OFlags};
use std::{
    fs::File,
    os::fd::AsRawFd,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
pub(crate) struct Folder {
    pub file: Arc<File>,
}
impl Folder {
    pub fn open(path: &Path, create: bool) -> Result<Self> {
        Self::open_with_mode(path, create, 0o777)
    }
    pub fn open_private(path: &Path) -> Result<Self> {
        Self::open_with_mode(path, true, 0o700)
    }
    fn open_with_mode(path: &Path, create: bool, mode: u32) -> Result<Self> {
        ensure!(path.is_absolute(), "folder must be absolute");
        let mut current = File::from(rustix::fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        for component in path.components() {
            match component {
                Component::RootDir => continue,
                Component::Normal(name) => {
                    let flags =
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
                    let next = match rustix::fs::openat(&current, name, flags, Mode::empty()) {
                        Ok(fd) => fd,
                        Err(rustix::io::Errno::NOENT) if create => {
                            match rustix::fs::mkdirat(&current, name, Mode::from_raw_mode(mode)) {
                                Ok(()) => {}
                                Err(rustix::io::Errno::EXIST) => {}
                                Err(e) => return Err(e.into()),
                            }
                            // Durably link each new ancestor before a later file ACK.
                            current.sync_all()?;
                            let fd = rustix::fs::openat(&current, name, flags, Mode::empty())?;
                            rustix::fs::fsync(&fd)?;
                            fd
                        }
                        Err(e) => return Err(e.into()),
                    };
                    current = File::from(next);
                }
                _ => anyhow::bail!("unsafe folder component"),
            }
        }
        Ok(Self {
            file: Arc::new(current),
        })
    }
    pub fn path(&self) -> Result<PathBuf> {
        let path = std::fs::read_link(format!("/proc/self/fd/{}", self.file.as_raw_fd()))?;
        ensure!(
            !path.as_os_str().as_encoded_bytes().ends_with(b" (deleted)"),
            "folder was deleted"
        );
        Ok(path)
    }
    pub fn space(&self) -> Result<u64> {
        let s = rustix::fs::fstatvfs(&*self.file)?;
        Ok(s.f_bavail.saturating_mul(s.f_frsize))
    }
    pub fn identity(&self) -> Result<(u64, u64)> {
        let s = rustix::fs::fstat(&*self.file)?;
        Ok((s.st_dev, s.st_ino))
    }
}
pub(crate) const STAGING_DIR: &str = ".pneu-staging";
/// Startup-only sweep. Never called while a transfer owns a live stage.
pub(crate) fn clean_staging(folder: &Folder) -> Result<()> {
    clean_legacy_staging(folder)?;
    match rustix::fs::openat(
        &*folder.file,
        STAGING_DIR,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let directory = owned_staging(File::from(fd))?;
            wipe(&directory.file)?;
            directory.file.sync_all()?;
        }
        Err(rustix::io::Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
/// Retire the old per-transfer private directories too, including ones that
/// crashed before journal registration. Reserved names alone are insufficient:
/// require an owned private directory, and never follow symlinks.
pub(crate) fn clean_legacy_staging(folder: &Folder) -> Result<()> {
    for entry in rustix::fs::Dir::read_from(&*folder.file)? {
        let entry = entry?;
        let name = entry.file_name();
        let bytes = name.to_bytes();
        let Some(suffix) = bytes.strip_prefix(b".pneu-stage-") else {
            continue;
        };
        if suffix.len() != 32 || !suffix.iter().all(u8::is_ascii_hexdigit) {
            continue;
        }
        let fd = match rustix::fs::openat(
            &*folder.file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR | rustix::io::Errno::NOENT) => {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let directory = File::from(fd);
        let stat = rustix::fs::fstat(&directory)?;
        if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o777 != 0o700 {
            continue;
        }
        wipe(&directory)?;
        remove_directory(&folder.file, name, &directory)?;
    }
    folder.file.sync_all()?;
    Ok(())
}
pub(crate) fn staging_directory(folder: &Folder) -> Result<Folder> {
    match rustix::fs::mkdirat(&*folder.file, STAGING_DIR, Mode::from_raw_mode(0o700)) {
        Ok(()) => folder.file.sync_all()?,
        Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    owned_staging(File::from(rustix::fs::openat(
        &*folder.file,
        STAGING_DIR,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
fn owned_staging(file: File) -> Result<Folder> {
    let stat = rustix::fs::fstat(&file)?;
    ensure!(
        stat.st_uid == rustix::process::geteuid().as_raw() && stat.st_mode & 0o077 == 0,
        "staging directory is not owned and private"
    );
    // Existing roots are opened and checked, never chmodded during cleanup or
    // transfer admission. Only mkdir sets permissions on newly created roots.
    Ok(Folder {
        file: Arc::new(file),
    })
}

fn wipe(directory: &File) -> Result<()> {
    for entry in rustix::fs::Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let stat = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Directory {
            let child = File::from(rustix::fs::openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            wipe(&child)?;
            // The tree is daemon-owned, but still never follow swapped entries.
            let own = rustix::fs::fstat(&child)?;
            let named = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
            ensure!(
                (own.st_dev, own.st_ino) == (named.st_dev, named.st_ino),
                "staging directory was replaced during cleanup"
            );
            unlink(directory, name, AtFlags::REMOVEDIR)?;
        } else {
            let file = File::from(rustix::fs::openat(
                directory,
                name,
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
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
    pub cleanup: Mutex<Option<Cleanup>>,
}
impl Stage {
    pub fn create(
        folder: Folder,
        directory: Folder,
        destination: String,
        size: u64,
    ) -> Result<Self> {
        ensure!(
            folder.space()? >= size.saturating_add(16 * 1024 * 1024),
            std::io::Error::from_raw_os_error(rustix::io::Errno::NOSPC.raw_os_error())
        );
        let name = crate::random_hex(16);
        let file = File::from(rustix::fs::openat(
            &*directory.file,
            &name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        Ok(Self(Arc::new(StageInner {
            folder,
            directory,
            file,
            name,
            destination,
            cleanup: Mutex::new(None),
        })))
    }
    pub fn record_placed(&self) -> Result<OwnedFile> {
        Ok(OwnedFile {
            folder: self.0.folder.clone(),
            name: self.0.destination.clone(),
            identity: FileIdentity::read(&self.0.file)?,
        })
    }
    pub fn destination_path(&self) -> Result<PathBuf> {
        Ok(self.0.folder.path()?.join(&self.0.destination))
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
        let before = rustix::fs::fstat(&self.0.file)?;
        let named = rustix::fs::statat(
            &*self.0.directory.file,
            &self.0.name,
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        ensure!(
            (before.st_dev, before.st_ino) == (named.st_dev, named.st_ino),
            "staging file was replaced"
        );
        rustix::fs::renameat_with(
            &*self.0.directory.file,
            &self.0.name,
            &*self.0.folder.file,
            &self.0.destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        let placed = rustix::fs::statat(
            &*self.0.folder.file,
            &self.0.destination,
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        ensure!(
            (before.st_dev, before.st_ino) == (placed.st_dev, placed.st_ino),
            "placed file was replaced"
        );
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
                drop(cleanup.store.start_blocking(move |_| {
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
/// Unlink only the entry still naming our retained inode, relative to its
/// original directory. Retaining the file FD prevents inode-number reuse.
pub(crate) fn remove_file(
    parent: &File,
    name: impl rustix::path::Arg + Clone,
    file: &File,
) -> Result<()> {
    remove_owned(parent, name, file, AtFlags::empty())
}
pub(crate) fn remove_directory(
    parent: &File,
    name: impl rustix::path::Arg + Clone,
    directory: &File,
) -> Result<()> {
    remove_owned(parent, name, directory, AtFlags::REMOVEDIR)
}
fn remove_owned(
    parent: &File,
    name: impl rustix::path::Arg + Clone,
    file: &File,
    flags: AtFlags,
) -> Result<()> {
    let own = rustix::fs::fstat(file)?;
    match rustix::fs::statat(parent, name.clone(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) if (own.st_dev, own.st_ino) == (named.st_dev, named.st_ino) => {
            unlink(parent, name, flags)
        }
        Ok(_) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
/// Recorded ownership evidence without an open file that could pin unlinked
/// cache blocks. Keep only the directory anchor; reopen without following links
/// and verify identity and unchanged metadata when retirement is requested.
pub(crate) struct OwnedFile {
    folder: Folder,
    name: String,
    identity: FileIdentity,
}
#[derive(PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl FileIdentity {
    fn read(file: &File) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        ensure!(meta.is_file(), "placed file must be regular");
        Ok(Self {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }
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
        let file = match rustix::fs::openat(
            &*self.folder.file,
            &self.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok((FileIdentity::read(&file)? == self.identity).then_some(file))
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
/// Retention policy selects old regular cache files, then anchors its checks and
/// deletion to opened descriptors. This is also the restart orphan sweep.
pub(crate) fn prune_cache(folder: &Folder, cutoff: std::time::SystemTime) -> Result<()> {
    for entry in rustix::fs::Dir::read_from(&*folder.file)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_bytes().starts_with(b"received-") {
            continue;
        }
        let file = match rustix::fs::openat(
            &*folder.file,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP) => continue,
            Err(e) => return Err(e.into()),
        };
        let meta = file.metadata()?;
        if meta.is_file() && meta.modified()? < cutoff {
            remove_file(&folder.file, name, &file)?;
        }
    }
    folder.file.sync_all()?;
    Ok(())
}
pub(crate) fn unlink(dir: &File, name: impl rustix::path::Arg, flags: AtFlags) -> Result<()> {
    match rustix::fs::unlinkat(dir, name, flags) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(e) => Err(e).context("relative cleanup"),
    }
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
    let file = File::from(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
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
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn cache_pruning_is_anchored_and_selects_only_old_regular_received_files() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = dir.path().join("cache");
        std::fs::create_dir(&cache)?;
        let folder = Folder::open(&cache, false)?;
        std::fs::write(cache.join("received-old"), b"old")?;
        File::open(cache.join("received-old"))?
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))?;
        std::fs::write(cache.join("received-new"), b"new")?;
        std::fs::write(cache.join("unrelated"), b"keep")?;
        std::os::unix::fs::symlink(cache.join("unrelated"), cache.join("received-link"))?;
        std::fs::create_dir(cache.join("received-directory"))?;
        let moved = dir.path().join("moved");
        std::fs::rename(&cache, &moved)?;
        std::fs::create_dir(&cache)?;
        std::fs::write(cache.join("received-old"), b"foreign")?;
        prune_cache(
            &folder,
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1),
        )?;
        assert!(!moved.join("received-old").exists());
        assert_eq!(std::fs::read(moved.join("received-new"))?, b"new");
        assert_eq!(std::fs::read(moved.join("unrelated"))?, b"keep");
        assert!(
            std::fs::symlink_metadata(moved.join("received-link"))?
                .file_type()
                .is_symlink()
        );
        assert!(moved.join("received-directory").is_dir());
        assert_eq!(std::fs::read(cache.join("received-old"))?, b"foreign");
        Ok(())
    }
    #[test]
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
