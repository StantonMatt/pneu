//! ACL enforcement is descriptor based. Never repair an existing ACL: it may
//! already have exposed a secret, and accepting it would hide that fact.
use anyhow::Result;
use std::fs::File;
#[cfg(target_os = "macos")]
use std::path::Path;

#[cfg(any(target_os = "macos", test))]
pub(crate) fn rename_safe(owner: u32, mode: u32, uid: u32) -> bool {
    (owner == 0 || owner == uid) && (mode & 0o022 == 0 || mode & 0o1000 != 0)
}
pub(crate) fn created(file: &File) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let acl = darwin::Acl::empty()?;
        darwin::set(file, &acl)?;
    }
    check(file)
}
pub(crate) fn check(file: &File) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            file.metadata()?.uid() == rustix::process::geteuid().as_raw(),
            "private object has foreign owner"
        );
        darwin::check(file, false)?;
    }
    #[cfg(target_os = "linux")]
    let _ = file;
    Ok(())
}
#[cfg(target_os = "macos")]
pub(crate) fn check_path(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    check(&crate::storage::open_regular_file(path)?)?;
    #[cfg(target_os = "linux")]
    let _ = path;
    Ok(())
}
#[cfg(target_os = "macos")]
pub(crate) fn open(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = match options.create_new(true).open(path) {
        Ok(file) => {
            created(&file)?;
            file
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = options.create_new(false).open(path)?;
            anyhow::ensure!(
                file.metadata()?.is_file(),
                "private object is not a regular file"
            );
            check(&file)?;
            file
        }
        Err(e) => return Err(e.into()),
    };
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(target_os = "macos")]
pub(crate) mod darwin {
    use super::*;
    use anyhow::ensure;
    use std::{ffi::c_void, os::fd::AsRawFd};
    type Raw = *mut c_void;
    unsafe extern "C" {
        fn acl_init(count: i32) -> Raw;
        fn acl_free(acl: Raw) -> i32;
        fn acl_get_fd_np(fd: i32, kind: i32) -> Raw;
        fn acl_set_fd(fd: i32, acl: Raw) -> i32;
        fn acl_get_entry(acl: Raw, index: i32, entry: *mut Raw) -> i32;
        fn acl_get_tag_type(entry: Raw, tag: *mut i32) -> i32;
    }
    pub(super) struct Acl(Raw);
    impl Drop for Acl {
        fn drop(&mut self) {
            unsafe {
                acl_free(self.0);
            }
        }
    }
    impl Acl {
        pub(super) fn empty() -> Result<Self> {
            let acl = unsafe { acl_init(0) };
            ensure!(!acl.is_null(), std::io::Error::last_os_error());
            Ok(Self(acl))
        }
    }
    pub(super) fn set(file: &File, acl: &Acl) -> Result<()> {
        ensure!(
            unsafe { acl_set_fd(file.as_raw_fd(), acl.0) } == 0,
            std::io::Error::last_os_error()
        );
        Ok(())
    }
    pub(crate) fn check(file: &File, ancestor: bool) -> Result<()> {
        let raw = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
        if raw.is_null() {
            // Darwin reports an absent extended ACL as NULL/ENOENT.
            let error = std::io::Error::last_os_error();
            ensure!(error.raw_os_error() == Some(libc::ENOENT), error);
            return Ok(());
        }
        let acl = Acl(raw);
        let mut index = 0; // Darwin ACL_FIRST_ENTRY; ACL_NEXT_ENTRY is -1.
        loop {
            let mut entry = std::ptr::null_mut();
            let result = unsafe { acl_get_entry(acl.0, index, &mut entry) };
            if result != 0 {
                // Darwin returns -1/EINVAL at the end (unlike POSIX's 0).
                let error = std::io::Error::last_os_error();
                ensure!(error.raw_os_error() == Some(libc::EINVAL), error);
                break;
            }
            let mut tag = 0;
            ensure!(
                unsafe { acl_get_tag_type(entry, &mut tag) } == 0,
                std::io::Error::last_os_error()
            );
            ensure!(
                ancestor && tag == 2,
                "private path has an extended ACL grant"
            );
            index = -1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod mode_tests {
    #[test]
    fn private_leaf_cannot_compensate_for_foreign_rename_rights() {
        use super::rename_safe;
        assert!(rename_safe(501, 0o700, 501));
        assert!(rename_safe(0, 0o755, 501));
        assert!(rename_safe(0, 0o1777, 501));
        assert!(!rename_safe(0, 0o777, 501));
        assert!(!rename_safe(501, 0o770, 501));
        assert!(!rename_safe(502, 0o755, 501));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::storage::{Folder, Stage, staging_directory};
    fn grant(path: &Path, inherit: bool) -> Result<()> {
        let acl = if inherit {
            "group:everyone allow read,readattr,readextattr,readsecurity,file_inherit,directory_inherit"
        } else {
            "group:everyone allow read,readattr,readextattr,readsecurity"
        };
        let result = std::process::Command::new("/bin/chmod")
            .args(["+a", acl])
            .arg(path)
            .output()?;
        anyhow::ensure!(
            result.status.success(),
            "ACL fixture: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    }
    #[test]
    fn private_creation_removes_inherited_foreign_grants_before_bytes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        grant(&root, true)?;
        let private = Folder::open_private(&root.join("private"))?;
        check(&private.file)?;
        // Publication itself must sanitize even in an ACL-bearing parent.
        let key = root.join("secret");
        crate::config::atomic_private_new(&key, &[7; 32])?;
        check_path(&key)?;
        let staging = staging_directory(&Folder::open(&root, false)?)?;
        check(&staging.file)?;
        let stage = Stage::create(private.clone(), staging, "fixture".into(), 0)?;
        check(&stage.0.file)?;
        stage.cleanup()?;
        let identity = root.join("identity");
        crate::identity::Identity::load_or_create(&identity)?;
        check_path(&identity.join("identity.json"))?;
        let db = private.path()?.join("state.sqlite3");
        let store = crate::store::Store::open(&db)?;
        check_path(&db)?;
        check_path(&db.with_extension("lock"))?;
        drop(store);
        Ok(())
    }
    #[test]
    fn existing_private_objects_reject_explicit_foreign_grants() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let private = root.join("private");
        crate::config::private_dir(&private)?;
        grant(&private, false)?;
        assert!(crate::config::private_dir(&private).is_err());
        let identity = root.join("identity");
        crate::identity::Identity::load_or_create(&identity)?;
        grant(&identity.join("identity.json"), false)?;
        assert!(crate::identity::Identity::load_or_create(&identity).is_err());
        let fingerprint = root.join("fingerprint");
        crate::clipboard_fingerprint::FingerprintKey::load_or_create(&fingerprint)?;
        grant(&fingerprint, false)?;
        assert!(
            crate::clipboard_fingerprint::FingerprintKey::load_or_create(&fingerprint).is_err()
        );
        let staging = staging_directory(&Folder::open(&root, false)?)?;
        grant(&staging.path()?, false)?;
        assert!(staging_directory(&Folder::open(&root, false)?).is_err());
        let db = root.join("state.sqlite3");
        drop(crate::store::Store::open(&db)?);
        grant(&db, false)?;
        assert!(crate::store::Store::open(&db).is_err());
        let db_sidecar = root.join("sidecar.sqlite3");
        drop(crate::store::Store::open(&db_sidecar)?);
        let wal = root.join("sidecar.sqlite3-wal");
        std::fs::write(&wal, b"fixture")?;
        grant(&wal, false)?;
        assert!(crate::store::Store::open(&db_sidecar).is_err());
        let config = root.join("config.toml");
        crate::Config::default().save(&config)?;
        grant(&config, false)?;
        assert!(crate::Config::load(&config).is_err());
        Ok(())
    }
}
