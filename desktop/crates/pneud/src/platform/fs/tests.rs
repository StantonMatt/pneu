use super::*;
use anyhow::Result;
use std::{
    fs::File,
    os::unix::fs::{PermissionsExt, symlink},
};

#[test]
fn every_component_rejects_links_including_before_parent_traversal() -> Result<()> {
    let temp = crate::test_support::scratch()?;
    let root = temp.path().canonicalize()?;
    std::fs::create_dir_all(root.join("real/a/b"))?;
    std::fs::write(root.join("real/a/b/file"), b"original")?;
    symlink(root.join("real"), root.join("link"))?;
    symlink(root.join("real/a"), root.join("real/link"))?;
    symlink(root.join("real/a/b"), root.join("real/a/link"))?;
    symlink(root.join("real/a/b/file"), root.join("real/a/b/link"))?;
    for path in [
        "link/a/b/file",
        "real/link/b/file",
        "real/a/link/file",
        "real/a/b/link",
        "link/../real/a/b/file",
    ] {
        assert!(open_regular_file(&root.join(path)).is_err(), "{path}");
    }
    for path in ["link/a/b", "real/link/b", "real/a/link"] {
        assert!(Folder::open(&root.join(path), false).is_err(), "{path}");
        assert!(Folder::open_private(&root.join(path)).is_err(), "{path}");
    }
    let folder = Folder::open(&root.join("real/a/b"), false)?;
    assert!(is_link(&open_read_at(&folder.file, "link").unwrap_err()));
    assert!(open_regular_file(&root.join("real/a/b/../b/file")).is_ok());
    Ok(())
}

#[test]
fn exclusive_private_create_preserves_existing_file_and_symlink() -> Result<()> {
    let temp = crate::test_support::scratch()?;
    let folder = Folder::open(&temp.path().canonicalize()?, false)?;
    let original = create_private_at(&folder.file, "file", Access::ReadWrite)?;
    assert!(is_private_file(&original.metadata()?));
    assert!(is_exists(
        &create_private_at(&folder.file, "file", Access::Write).unwrap_err()
    ));
    symlink("file", folder.path()?.join("link"))?;
    assert!(is_exists(
        &create_private_at(&folder.file, "link", Access::Write).unwrap_err()
    ));
    assert_eq!(identity(&original)?, identity_at(&folder.file, "file")?);
    assert_eq!(original.metadata()?.len(), 0);
    Ok(())
}

#[test]
fn relative_rename_preserves_conflict_and_supports_replace() -> Result<()> {
    let temp = crate::test_support::scratch()?;
    let root = temp.path().canonicalize()?;
    let source = Folder::open_private(&root.join("source"))?;
    let target = Folder::open_private(&root.join("target"))?;
    std::fs::write(source.path()?.join("file"), b"new")?;
    std::fs::write(target.path()?.join("file"), b"old")?;
    assert!(is_exists(
        &rename(&source.file, "file", &target.file, "file", false).unwrap_err()
    ));
    assert_eq!(std::fs::read(source.path()?.join("file"))?, b"new");
    assert_eq!(std::fs::read(target.path()?.join("file"))?, b"old");
    std::fs::rename(root.join("target"), root.join("moved"))?;
    std::fs::create_dir(root.join("target"))?;
    rename(&source.file, "file", &target.file, "fresh", false)?;
    assert_eq!(std::fs::read(root.join("moved/fresh"))?, b"new");
    assert!(!root.join("target/fresh").exists());
    rename(&target.file, "fresh", &target.file, "file", true)?;
    assert_eq!(std::fs::read(root.join("moved/file"))?, b"new");
    Ok(())
}

#[test]
fn identity_checked_unlink_preserves_swapped_files_and_directories() -> Result<()> {
    let temp = crate::test_support::scratch()?;
    let root = temp.path().canonicalize()?;
    let folder = Folder::open(&root, false)?;
    let retained = create_private_at(&folder.file, "file", Access::ReadWrite)?;
    std::fs::rename(root.join("file"), root.join("original"))?;
    std::fs::write(root.join("file"), b"replacement")?;
    remove_file(&folder.file, "file", &retained)?;
    assert_eq!(std::fs::read(root.join("file"))?, b"replacement");
    remove_file(&folder.file, "original", &retained)?;
    remove_file(&folder.file, "original", &retained)?;
    assert!(!root.join("original").exists());
    let directory = Folder::open_private(&root.join("directory"))?;
    std::fs::rename(root.join("directory"), root.join("old-directory"))?;
    std::fs::create_dir(root.join("directory"))?;
    remove_directory(&folder.file, "directory", &directory.file)?;
    assert!(root.join("directory").is_dir());
    remove_directory(&folder.file, "old-directory", &directory.file)?;
    assert!(!root.join("old-directory").exists());
    Ok(())
}

#[test]
fn private_permissions_are_checked_without_repairing_existing_objects() -> Result<()> {
    let temp = crate::test_support::scratch()?;
    let folder = Folder::open(&temp.path().canonicalize()?, false)?;
    let file = create_private_at(&folder.file, "file", Access::ReadWrite)?;
    private::created(&file)?;
    assert!(is_private_file(&file.metadata()?));
    assert!(is_owned_private(&file)?);
    file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
    assert!(!is_private_file(&file.metadata()?));
    assert!(!is_owned_private(&file)?);
    assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o644);
    Ok(())
}

#[test]
fn standard_locks_interoperate_with_flock_and_release_on_close() -> Result<()> {
    let temp = crate::test_support::scratch()?;
    let path = temp.path().join("lock");
    let first = open_store_lock(&path)?;
    let second = open_store_lock(&path)?;
    first.try_lock()?;
    assert!(matches!(
        second.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert_eq!(
        rustix::fs::flock(
            &second,
            rustix::fs::FlockOperation::NonBlockingLockExclusive
        ),
        Err(rustix::io::Errno::WOULDBLOCK)
    );
    let error = second.try_lock().map_err(lock_error).unwrap_err();
    assert_eq!(
        error.downcast_ref::<rustix::io::Errno>(),
        Some(&rustix::io::Errno::WOULDBLOCK)
    );
    let duplicate = first.try_clone()?;
    drop(first);
    assert!(second.try_lock().is_err());
    drop(duplicate);
    second.try_lock()?;
    second.unlock()?;
    let reopened = File::open(path)?;
    rustix::fs::flock(
        &reopened,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )?;
    assert!(matches!(
        second.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(reopened);
    second.lock()?;
    Ok(())
}
