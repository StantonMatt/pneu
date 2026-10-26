//! Unix implementation. Keep flags, permissions and OS errors at this boundary.
use anyhow::{Context, Result, ensure};
use rustix::fs::{AtFlags, Mode, OFlags};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[path = "../private.rs"]
pub(crate) mod private;

/// Reject links in every path component and open nonblocking before checking
/// type, so FIFOs and device nodes cannot hang a local IPC worker.
/// Callers apply their own size policy after opening the descriptor.
pub(crate) fn open_regular_file(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "file paths must be absolute");
    let mut directory = File::from(rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let mut components = path.components().peekable();
    let file = loop {
        let component = components.next().context("file name")?;
        use std::path::Component;
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => std::ffi::OsStr::new(".."),
            Component::Normal(name) => name,
            Component::Prefix(_) => anyhow::bail!("unsupported file prefix"),
        };
        if components.peek().is_none() {
            ensure!(matches!(component, Component::Normal(_)), "file name");
            break File::from(rustix::fs::openat(
                &directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
        }
        // Resolve .. from the already opened directory. Do not normalize it
        // lexically: link/../file must still reject the link.
        directory = File::from(rustix::fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
    };
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "only regular files can be opened");
    Ok(file)
}
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
                            let created = match rustix::fs::mkdirat(
                                &current,
                                name,
                                Mode::from_raw_mode(mode as _),
                            ) {
                                Ok(()) => true,
                                Err(rustix::io::Errno::EXIST) => false,
                                Err(e) => return Err(e.into()),
                            };
                            // Durably link each new ancestor before a later file ACK.
                            current.sync_all()?;
                            let fd = rustix::fs::openat(&current, name, flags, Mode::empty())?;
                            if mode == 0o700 {
                                let file = File::from(fd);
                                if created {
                                    crate::platform::private::created(&file)?;
                                } else {
                                    crate::platform::private::check(&file)?;
                                }
                                rustix::fs::fsync(&file)?;
                                file.into()
                            } else {
                                fd
                            }
                        }
                        Err(e) => return Err(e.into()),
                    };
                    current = File::from(next);
                }
                _ => anyhow::bail!("unsafe folder component"),
            }
        }
        if mode == 0o700 {
            crate::platform::private::check(&current)?;
        }
        Ok(Self {
            file: Arc::new(current),
        })
    }
    pub fn path(&self) -> Result<PathBuf> {
        directory_path(&self.file)
    }
    pub fn space(&self) -> Result<u64> {
        let s = rustix::fs::fstatvfs(&*self.file)?;
        Ok((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
    }
    pub fn identity(&self) -> Result<Identity> {
        identity(&self.file)
    }
}
/// Unlink only the entry still naming our retained inode, relative to its
/// original directory. Retaining the file FD prevents inode-number reuse.
pub(crate) fn remove_file(parent: &File, name: impl AsRef<OsStr>, file: &File) -> Result<()> {
    remove_owned(parent, name, file, AtFlags::empty())
}
pub(crate) fn remove_directory(
    parent: &File,
    name: impl AsRef<OsStr>,
    directory: &File,
) -> Result<()> {
    remove_owned(parent, name, directory, AtFlags::REMOVEDIR)
}
fn remove_owned(parent: &File, name: impl AsRef<OsStr>, file: &File, flags: AtFlags) -> Result<()> {
    let own = rustix::fs::fstat(file)?;
    match rustix::fs::statat(parent, name.as_ref(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) if (own.st_dev, own.st_ino) == (named.st_dev, named.st_ino) => {
            unlink(parent, name.as_ref(), flags)
        }
        Ok(_) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn unlink(dir: &File, name: impl rustix::path::Arg, flags: AtFlags) -> Result<()> {
    match rustix::fs::unlinkat(dir, name, flags) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(e) => Err(e).context("relative cleanup"),
    }
}
#[cfg(target_os = "linux")]
pub(crate) fn directory_path(file: &std::fs::File) -> Result<PathBuf> {
    use std::os::fd::AsRawFd;
    let path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    ensure!(
        !path.as_os_str().as_encoded_bytes().ends_with(b" (deleted)"),
        "folder was deleted"
    );
    Ok(path)
}
#[cfg(target_os = "linux")]
pub(crate) fn metadata_file(
    directory: &std::fs::File,
    name: impl AsRef<OsStr>,
) -> Result<std::fs::File> {
    Ok(std::fs::File::from(rustix::fs::openat(
        directory,
        name.as_ref(),
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?))
}

#[cfg(target_os = "macos")]
pub(crate) fn directory_path(file: &File) -> Result<PathBuf> {
    let mut bytes = [0u8; libc::PATH_MAX as usize];
    // F_GETPATH is display/recovery only. Mutations continue to use openat fds.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) };
    ensure!(result != -1, std::io::Error::last_os_error());
    let path = PathBuf::from(OsStr::from_bytes(
        unsafe { CStr::from_ptr(bytes.as_ptr().cast()) }.to_bytes(),
    ));
    let reopened = crate::storage::Folder::open(&path, false)?;
    use std::os::unix::fs::MetadataExt;
    let own = file.metadata()?;
    let named = reopened.file.metadata()?;
    ensure!(
        (own.dev(), own.ino()) == (named.dev(), named.ino()),
        "folder was deleted or replaced"
    );
    Ok(path)
}
#[cfg(target_os = "macos")]
pub(crate) fn metadata_file(directory: &File, name: impl AsRef<OsStr>) -> Result<File> {
    // O_SYMLINK opens the final symlink itself; never its target. O_EVTONLY
    // avoids data access/side effects. Darwin cannot open socket vnodes this
    // way; unexpected sockets in a private staging tree fail cleanup closed.
    let flags = rustix::fs::OFlags::from_bits_retain(
        (libc::O_EVTONLY | libc::O_SYMLINK | libc::O_CLOEXEC) as _,
    );
    Ok(File::from(rustix::fs::openat(
        directory,
        name.as_ref(),
        flags,
        rustix::fs::Mode::empty(),
    )?))
}

#[cfg(target_os = "macos")]
use std::{ffi::CStr, os::fd::AsRawFd};

/// Stable identity only; each backend chooses its representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Identity(u64, u64);
pub(crate) fn identity(file: &File) -> Result<Identity> {
    let stat = rustix::fs::fstat(file)?;
    Ok(Identity(stat.st_dev as u64, stat.st_ino))
}
pub(crate) fn identity_at(parent: &File, name: impl AsRef<OsStr>) -> Result<Identity> {
    let stat = rustix::fs::statat(parent, name.as_ref(), AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(Identity(stat.st_dev as u64, stat.st_ino))
}
/// Metadata proof without pinning a file's blocks after unlink.
#[derive(PartialEq, Eq)]
pub(crate) struct Version(Identity, u64, i64, i64, i64, i64);
impl Version {
    #[cfg(any(target_os = "macos", test))]
    pub fn read(file: &File) -> Result<Self> {
        Ok(Self::from_metadata(file.metadata()?))
    }
    pub fn read_regular(file: &File) -> Result<Self> {
        let meta = file.metadata()?;
        ensure!(meta.is_file(), "placed file must be regular");
        Ok(Self::from_metadata(meta))
    }
    fn from_metadata(m: std::fs::Metadata) -> Self {
        Self(
            Identity(m.dev(), m.ino()),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    }
}
pub(crate) fn entries(parent: &File) -> Result<impl Iterator<Item = Result<OsString>>> {
    Ok(rustix::fs::Dir::read_from(parent)?
        .map(|entry| Ok(OsStr::from_bytes(entry?.file_name().to_bytes()).to_owned())))
}
pub(crate) fn open_directory_at(parent: &File, name: impl AsRef<OsStr>) -> Result<File> {
    Ok(File::from(rustix::fs::openat(
        parent,
        name.as_ref(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
pub(crate) fn create_private_directory_at(parent: &File, name: impl AsRef<OsStr>) -> Result<()> {
    Ok(rustix::fs::mkdirat(
        parent,
        name.as_ref(),
        Mode::from_raw_mode(0o700),
    )?)
}
/// These opens reject the final link and avoid blocking on FIFOs; callers check type.
pub(crate) fn open_read_at(parent: &File, name: impl AsRef<OsStr>) -> Result<File> {
    Ok(File::from(rustix::fs::openat(
        parent,
        name.as_ref(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
pub(crate) fn open_read(path: &Path) -> Result<File> {
    Ok(File::from(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
pub(crate) fn open_journal_at(parent: &File, name: impl AsRef<OsStr>) -> Result<File> {
    Ok(File::from(rustix::fs::openat(
        parent,
        name.as_ref(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
pub(crate) fn open_secret(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}
#[cfg(any(target_os = "macos", test))]
pub(crate) fn open_read_write_at(parent: &File, name: impl AsRef<OsStr>) -> Result<File> {
    Ok(File::from(rustix::fs::openat(
        parent,
        name.as_ref(),
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
#[derive(Clone, Copy)]
pub(crate) enum Access {
    Write,
    ReadWrite,
}
pub(crate) fn create_private_at(
    parent: &File,
    name: impl AsRef<OsStr>,
    access: Access,
) -> Result<File> {
    let access = match access {
        Access::Write => OFlags::WRONLY,
        Access::ReadWrite => OFlags::RDWR,
    };
    Ok(File::from(rustix::fs::openat(
        parent,
        name.as_ref(),
        access | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?))
}
#[cfg(target_os = "linux")]
pub(crate) fn open_lock_at(parent: &File, name: impl AsRef<OsStr>) -> Result<File> {
    Ok(File::from(rustix::fs::openat(
        parent,
        name.as_ref(),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?))
}
pub(crate) fn open_lock(path: &Path) -> Result<File> {
    #[cfg(target_os = "macos")]
    {
        private::open(path)
    }
    #[cfg(target_os = "linux")]
    {
        Ok(File::from(rustix::fs::open(
            path,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?))
    }
}
pub(crate) fn open_store_lock(path: &Path) -> Result<File> {
    #[cfg(target_os = "macos")]
    {
        private::open(path)
    }
    #[cfg(target_os = "linux")]
    {
        Ok(std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(OFlags::NOFOLLOW.bits() as i32)
            .open(path)?)
    }
}
pub(crate) fn rename(
    parent: &File,
    name: impl AsRef<OsStr>,
    target: &File,
    destination: impl AsRef<OsStr>,
    replace: bool,
) -> Result<()> {
    if replace {
        rustix::fs::renameat(parent, name.as_ref(), target, destination.as_ref())?;
    } else {
        rustix::fs::renameat_with(
            parent,
            name.as_ref(),
            target,
            destination.as_ref(),
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
    }
    Ok(())
}
pub(crate) fn is_directory_at(parent: &File, name: impl AsRef<OsStr>) -> Result<bool> {
    let stat = rustix::fs::statat(parent, name.as_ref(), AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(stat.st_mode & libc::S_IFMT == libc::S_IFDIR)
}
pub(crate) fn is_private_file(file: &File) -> Result<bool> {
    let meta = file.metadata()?;
    Ok(meta.is_file()
        && meta.uid() == rustix::process::geteuid().as_raw()
        && meta.mode() & 0o777 == 0o600)
}
pub(crate) fn is_private_directory(file: &File) -> Result<bool> {
    let stat = rustix::fs::fstat(file)?;
    Ok(stat.st_uid == rustix::process::geteuid().as_raw() && stat.st_mode & 0o777 == 0o700)
}
pub(crate) fn is_owned_private(file: &File) -> Result<bool> {
    let stat = rustix::fs::fstat(file)?;
    Ok(stat.st_uid == rustix::process::geteuid().as_raw() && stat.st_mode & 0o077 == 0)
}
pub(crate) fn set_private_file(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}
pub(crate) fn set_private_directory(file: &File) -> std::io::Result<()> {
    file.set_permissions(std::fs::Permissions::from_mode(0o700))
}
pub(crate) fn set_read_only(file: &File) -> std::io::Result<()> {
    file.set_permissions(std::fs::Permissions::from_mode(0o400))
}
pub(crate) fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<usize> {
    file.read_at(bytes, offset)
}
pub(crate) fn copy_prefix(source: &File, destination: &File, prefix: u64) -> Result<()> {
    let mut offset = 0;
    let mut bytes = [0; 32768];
    while offset < prefix {
        let n = (prefix - offset).min(bytes.len() as u64) as usize;
        source.read_exact_at(&mut bytes[..n], offset)?;
        destination.write_all_at(&bytes[..n], offset)?;
        offset += n as u64;
    }
    rustix::fs::seek(destination, rustix::fs::SeekFrom::Start(prefix))?;
    Ok(())
}
// Keep the original OS causes (and their Display text) inside anyhow contexts.
fn errno(cause: &(dyn std::error::Error + 'static)) -> Option<rustix::io::Errno> {
    cause
        .downcast_ref::<rustix::io::Errno>()
        .copied()
        .or_else(|| {
            cause
                .downcast_ref::<std::io::Error>()
                .and_then(|e| e.raw_os_error())
                .map(rustix::io::Errno::from_raw_os_error)
        })
}
pub(crate) fn is_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|e| errno(e) == Some(rustix::io::Errno::NOENT))
}
pub(crate) fn is_exists(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|e| errno(e) == Some(rustix::io::Errno::EXIST))
}
pub(crate) fn is_link(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|e| errno(e) == Some(rustix::io::Errno::LOOP))
}
pub(crate) fn is_not_directory(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|e| errno(e) == Some(rustix::io::Errno::NOTDIR))
}
pub(crate) fn permission_denied() -> anyhow::Error {
    rustix::io::Errno::ACCESS.into()
}
pub(crate) fn no_space() -> std::io::Error {
    std::io::Error::from_raw_os_error(rustix::io::Errno::NOSPC.raw_os_error())
}
pub(crate) fn storage_failure(
    cause: &(dyn std::error::Error + 'static),
) -> Option<pneu_proto::NackCode> {
    use pneu_proto::NackCode;
    match errno(cause)? {
        rustix::io::Errno::NOSPC | rustix::io::Errno::DQUOT => Some(NackCode::DiskFull),
        rustix::io::Errno::ACCESS
        | rustix::io::Errno::ROFS
        | rustix::io::Errno::PERM
        | rustix::io::Errno::NOENT
        | rustix::io::Errno::NOTDIR
        | rustix::io::Errno::EXIST => Some(NackCode::NotWritable),
        _ => None,
    }
}

/// Private, anonymous backing for normalized clipboard bytes. Keep a fallback
/// for cache filesystems that do not implement O_TMPFILE.
pub(crate) fn clipboard_file(cache: &Path) -> Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    let directory = rustix::fs::open(
        cache,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    #[cfg(target_os = "macos")]
    {
        let directory = std::fs::File::from(directory);
        crate::platform::private::check(&directory)?;
        unlinked_clipboard_file(&directory)
    }
    #[cfg(target_os = "linux")]
    match rustix::fs::openat(
        &directory,
        ".",
        OFlags::TMPFILE | OFlags::RDWR | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(file) => Ok(file.into()),
        Err(rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::ISDIR | rustix::io::Errno::NOENT) => {
            unlinked_clipboard_file(&directory)
        }
        Err(error) => Err(error.into()),
    }
}
pub(crate) fn unlinked_clipboard_file(directory: &impl std::os::fd::AsFd) -> Result<std::fs::File> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    for _ in 0..128 {
        let name = format!(".pneu-clipboard-{}", crate::random_hex(16));
        match rustix::fs::openat(
            directory,
            &name,
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(file) => {
                rustix::fs::unlinkat(directory, &name, AtFlags::empty())?;
                let file = std::fs::File::from(file);
                crate::platform::private::created(&file)?;
                return Ok(file);
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("could not allocate anonymous clipboard file")
}

#[cfg(target_os = "linux")]
pub(crate) fn viewer_descriptor(file: &File) -> Result<File> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    // dup/try_clone shares the streaming cursor (usually at EOF). Reopen
    // through our still-owned descriptor, never through the received path,
    // to give each portal launch a read-only cursor starting at byte zero.
    let descriptor = std::fs::File::from(rustix::fs::open(
        format!("/proc/self/fd/{}", file.as_raw_fd()),
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    let original = file.metadata()?;
    let opened = descriptor.metadata()?;
    anyhow::ensure!(
        opened.is_file() && (opened.dev(), opened.ino()) == (original.dev(), original.ino()),
        "viewer descriptor changed identity"
    );
    Ok(descriptor)
}

pub(crate) fn unlink_directory(parent: &File, name: impl AsRef<OsStr>) -> Result<()> {
    unlink(parent, name.as_ref(), AtFlags::REMOVEDIR)
}
#[cfg(target_os = "macos")]
pub(crate) fn unlink_file(parent: &File, name: impl AsRef<OsStr>) -> Result<()> {
    unlink(parent, name.as_ref(), AtFlags::empty())
}

pub(crate) fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("no parent")?)?.sync_all()?;
    Ok(())
}

pub(crate) fn lock_error(error: std::fs::TryLockError) -> anyhow::Error {
    match error {
        std::fs::TryLockError::WouldBlock => rustix::io::Errno::WOULDBLOCK.into(),
        std::fs::TryLockError::Error(error) => match error.raw_os_error() {
            Some(code) => rustix::io::Errno::from_raw_os_error(code).into(),
            None => error.into(),
        },
    }
}
