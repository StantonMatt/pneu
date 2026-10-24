//! One private viewer-materialization lease per supervised child. The daemon
//! retains directory identities so a killed child is cleaned without following
//! display paths or deleting a replacement cache directory.
use anyhow::Result;
use rustix::fs::{FlockOperation, Mode, OFlags};
use std::{
    fs::File,
    path::{Path, PathBuf},
};
pub(crate) struct Lease {
    root: crate::storage::Folder,
    directory: crate::storage::Folder,
    name: String,
    pub lock: File,
}
impl Lease {
    pub fn new(cache: &Path) -> Result<Self> {
        let root = crate::storage::Folder::open_private(&cache.join("ui-viewers"))?;
        // Serialize recovery with lease creation, including in other processes.
        rustix::fs::flock(&*root.file, FlockOperation::LockExclusive)?;
        recover(&root)?;
        let name = crate::random_hex(16);
        let directory = crate::storage::Folder::open_private(&root.path()?.join(&name))?;
        let lock = File::from(rustix::fs::openat(
            &*directory.file,
            ".lease",
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        crate::platform::private::created(&lock)?;
        rustix::fs::flock(&lock, FlockOperation::LockExclusive)?;
        rustix::fs::flock(&*root.file, FlockOperation::Unlock)?;
        Ok(Self {
            root,
            directory,
            name,
            lock,
        })
    }
    pub fn path(&self) -> Result<PathBuf> {
        self.directory.path()
    }
}
fn recover(root: &crate::storage::Folder) -> Result<()> {
    for entry in rustix::fs::Dir::read_from(&*root.file)? {
        let entry = entry?;
        let name = entry.file_name();
        let bytes = name.to_bytes();
        if bytes.len() != 32 || !bytes.iter().all(u8::is_ascii_hexdigit) {
            continue;
        }
        let directory = match rustix::fs::openat(
            &*root.file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(_) => continue,
        };
        if crate::platform::private::check(&directory).is_err() {
            continue;
        }
        // Older leases without a liveness marker cannot be proven stale.
        let lock = match rustix::fs::openat(
            &directory,
            ".lease",
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(_) => continue,
        };
        if !lock.metadata()?.is_file() || crate::platform::private::check(&lock).is_err() {
            continue;
        }
        if rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
            continue;
        }
        crate::storage::wipe(&directory)?;
        crate::storage::remove_directory(&root.file, name, &directory)?;
    }
    Ok(())
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Err(error) = crate::storage::wipe(&self.directory.file).and_then(|_| {
            crate::storage::remove_directory(&self.root.file, &self.name, &self.directory.file)
        }) {
            eprintln!("pneud: UI viewer cache cleanup: {error:#}");
        }
    }
}
/// A missing cache disables only descriptor-backed viewing. Retry on each open
/// so repairing the volume needs neither a daemon nor a UI restart.
#[cfg(any(target_os = "macos", test))]
pub(crate) struct ViewerCache {
    root: PathBuf,
    inherited: Option<PathBuf>,
    lease: Option<Lease>,
}
#[cfg(any(target_os = "macos", test))]
impl ViewerCache {
    pub fn new(root: PathBuf, inherited: Option<PathBuf>) -> Self {
        Self {
            root,
            inherited,
            lease: None,
        }
    }
    pub fn path(&mut self) -> Result<PathBuf> {
        if let Some(path) = &self.inherited {
            return Ok(path.clone());
        }
        if self.lease.is_none() {
            self.lease = Some(Lease::new(&self.root).map_err(|error| anyhow::anyhow!(
                "Received-file Open is unavailable because Pneu's viewer cache cannot be used; repair the cache and retry: {error:#}"
            ))?);
        }
        self.lease.as_ref().expect("viewer lease").path()
    }
}
/// Retain recent viewer copies, retiring the oldest accepted handoff when a
/// new one succeeds. Capacity is a working set, never a count of past opens.
#[derive(Default)]
pub(crate) struct Materials(std::collections::VecDeque<Materialization>);
impl Materials {
    pub fn find(&mut self, source: &File, name: &str) -> Option<PathBuf> {
        let identity = SourceIdentity::read(source).ok()?;
        let index = self.0.iter().position(|m| {
            m.source == identity && m.name == std::ffi::OsStr::new(name) && m.present()
        })?;
        let material = self.0.remove(index)?;
        let path = material.directory.path().ok()?.join(&material.name);
        self.0.push_back(material);
        Some(path)
    }
    pub fn push(&mut self, material: Materialization) {
        self.0.push_back(material);
        while self.0.len() > 128 {
            self.0.pop_front();
        }
    }
}
pub(crate) struct Materialization {
    pub path: PathBuf,
    root: crate::storage::Folder,
    directory: crate::storage::Folder,
    directory_name: String,
    name: std::ffi::OsString,
    file: File,
    source: SourceIdentity,
}
#[derive(PartialEq, Eq)]
struct SourceIdentity(u64, u64, u64, i64, i64, i64, i64);
impl SourceIdentity {
    fn read(file: &File) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        Ok(Self(
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        ))
    }
}
impl Materialization {
    fn present(&self) -> bool {
        let own = rustix::fs::fstat(&self.file);
        let named = rustix::fs::statat(
            &*self.directory.file,
            self.name.as_os_str(),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        );
        matches!((own, named), (Ok(own), Ok(named)) if (own.st_dev, own.st_ino) == (named.st_dev, named.st_ino))
    }
}
impl Drop for Materialization {
    fn drop(&mut self) {
        let _ =
            crate::storage::remove_file(&self.directory.file, self.name.as_os_str(), &self.file);
        let _ = crate::storage::remove_directory(
            &self.root.file,
            &self.directory_name,
            &self.directory.file,
        );
    }
}
pub(crate) fn materialize(
    cache: &Path,
    source: File,
    name: &str,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Materialization> {
    use anyhow::ensure;
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{FileExt, PermissionsExt},
    };
    let metadata = source.metadata()?;
    let source_identity = SourceIdentity::read(&source)?;
    ensure!(
        metadata.is_file() && metadata.len() <= pneu_proto::MAX_FILE_SIZE,
        "invalid received descriptor"
    );
    let root = crate::storage::Folder::open_private(cache)?;
    let directory_name = crate::random_hex(16);
    rustix::fs::mkdirat(&*root.file, &directory_name, Mode::from_raw_mode(0o700))?;
    let directory = crate::storage::Folder {
        file: std::sync::Arc::new(File::from(rustix::fs::openat(
            &*root.file,
            &directory_name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?)),
    };
    crate::platform::private::created(&directory.file)?;
    let name = Path::new(name)
        .file_name()
        .filter(|s| !s.as_bytes().is_empty())
        .unwrap_or(std::ffi::OsStr::new("received-file"))
        .to_owned();
    let destination = File::from(rustix::fs::openat(
        &*directory.file,
        name.as_os_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let mut material = Materialization {
        path: directory.path()?.join(&name),
        root,
        directory,
        directory_name,
        name,
        file: destination,
        source: source_identity,
    };
    crate::platform::private::created(&material.file)?;
    use std::io::Write;
    let mut offset = 0;
    let mut buffer = [0u8; 65536];
    while offset < metadata.len() {
        ensure!(
            !cancelled.load(std::sync::atomic::Ordering::Acquire),
            "viewer request cancelled"
        );
        let cap = buffer.len().min((metadata.len() - offset) as usize);
        let read = source.read_at(&mut buffer[..cap], offset)?;
        ensure!(read > 0, "received file truncated");
        material.file.write_all(&buffer[..read])?;
        offset += read as u64;
    }
    ensure!(
        SourceIdentity::read(&source)? == material.source,
        "received file changed during open"
    );
    material.file.sync_all()?;
    material
        .file
        .set_permissions(std::fs::Permissions::from_mode(0o400))?;
    Ok(material)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_cache_does_not_prevent_ui_state_and_retries_on_demand() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?.join("cache");
        std::fs::write(&root, b"unavailable volume")?;
        let mut cache = ViewerCache::new(root.clone(), None);
        assert!(
            cache
                .path()
                .unwrap_err()
                .to_string()
                .contains("repair the cache and retry")
        );
        std::fs::remove_file(&root)?;
        let path = cache.path()?;
        assert!(path.is_dir());
        assert_eq!(cache.path()?, path);
        drop(cache);
        assert!(!path.exists());
        Ok(())
    }
    #[test]
    fn material_cleanup_preserves_file_replacement_and_ancestor_symlinks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let source = root.join("source");
        std::fs::write(&source, b"received")?;
        let cache = root.join("cache");
        let material = materialize(
            &cache,
            File::open(&source)?,
            "received.txt",
            &std::sync::atomic::AtomicBool::new(false),
        )?;
        let path = material.path.clone();
        let moved = root.join("moved");
        std::fs::rename(path.parent().unwrap(), &moved)?;
        let foreign = root.join("foreign");
        std::fs::create_dir(&foreign)?;
        std::fs::write(foreign.join("received.txt"), b"foreign")?;
        std::os::unix::fs::symlink(&foreign, path.parent().unwrap())?;
        std::fs::rename(moved.join("received.txt"), moved.join("original"))?;
        std::fs::write(moved.join("received.txt"), b"replacement")?;
        drop(material);
        assert_eq!(std::fs::read(foreign.join("received.txt"))?, b"foreign");
        assert_eq!(std::fs::read(moved.join("received.txt"))?, b"replacement");
        Ok(())
    }
    #[test]
    fn viewer_capacity_is_a_reclaimable_working_set() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let source = root.join("source");
        std::fs::write(&source, b"received")?;
        let mut materials = Materials::default();
        for n in 0..300 {
            let material = materialize(
                &root.join("cache"),
                File::open(&source)?,
                &format!("{n}.txt"),
                &std::sync::atomic::AtomicBool::new(false),
            )?;
            assert_eq!(std::fs::read(&material.path)?, b"received");
            materials.push(material);
            assert!(materials.0.len() <= 128);
        }
        assert_eq!(std::fs::read_dir(root.join("cache"))?.count(), 128);
        assert!(materials.find(&File::open(&source)?, "299.txt").is_some());
        std::fs::write(&source, b"changed")?;
        assert!(materials.find(&File::open(&source)?, "299.txt").is_none());
        drop(materials);
        assert_eq!(std::fs::read_dir(root.join("cache"))?.count(), 0);
        Ok(())
    }
    #[test]
    fn abrupt_lease_owner() -> Result<()> {
        let Some(cache) = std::env::var_os("PNEU_TEST_LEASE_CACHE") else {
            return Ok(());
        };
        let lease = Lease::new(Path::new(&cache))?;
        crate::config::atomic_private(&lease.path()?.join("orphan"), b"private copy")?;
        // No destructors, as with a crash or power loss. The OS releases flock.
        std::process::exit(0);
    }
    #[test]
    fn startup_recovers_abrupt_exit_but_preserves_live_leases() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let live = Lease::new(&root)?;
        assert!(live.lock.metadata()?.is_file());
        crate::config::atomic_private(&live.path()?.join("live"), b"keep")?;
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "ui::viewer_cache::tests::abrupt_lease_owner"])
            .env("PNEU_TEST_LEASE_CACHE", &root)
            .status()?;
        assert!(status.success());
        assert_eq!(std::fs::read_dir(root.join("ui-viewers"))?.count(), 2);
        let next = Lease::new(&root)?;
        assert_eq!(std::fs::read_dir(root.join("ui-viewers"))?.count(), 2);
        assert_eq!(std::fs::read(live.path()?.join("live"))?, b"keep");
        drop(next);
        drop(live);
        assert_eq!(std::fs::read_dir(root.join("ui-viewers"))?.count(), 0);
        Ok(())
    }
    #[test]
    fn child_cache_cleanup_is_descriptor_relative_and_preserves_replacements() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let lease = Lease::new(&root)?;
        let path = lease.path()?;
        let artifact = path.join("view");
        crate::config::private_dir(&artifact)?;
        crate::config::atomic_private(&artifact.join("received.txt"), b"original")?;
        let moved = path.with_extension("moved");
        std::fs::rename(&path, &moved)?;
        crate::config::private_dir(&path)?;
        crate::config::atomic_private(&path.join("keep"), b"replacement")?;
        drop(lease);
        assert!(std::fs::read_dir(&moved)?.next().is_none());
        assert_eq!(std::fs::read(path.join("keep"))?, b"replacement");
        Ok(())
    }
}
