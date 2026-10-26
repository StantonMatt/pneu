use super::*;
use std::io::{Read, Seek, Write};
fn fixture() -> Result<(tempfile::TempDir, Folder)> {
    let temp = crate::test_support::scratch()?;
    let folder = Folder::open(temp.path(), false)?;
    Ok((temp, folder))
}
#[test]
fn private_dacl_is_present_at_exclusive_create() -> Result<()> {
    let (_temp, folder) = fixture()?;
    let file = create_private_at(&folder.file, "file", Access::ReadWrite)?;
    private::check(&file)?;
    assert!(is_private_file(&file)?);
    assert!(is_exists(
        &create_private_at(&folder.file, "file", Access::Write).unwrap_err()
    ));
    create_private_directory_at(&folder.file, "child")?;
    assert!(is_exists(
        &create_private_directory_at(&folder.file, "child").unwrap_err()
    ));
    private::check(&open_directory_at(&folder.file, "child")?)?;
    Ok(())
}
#[test]
fn inherited_and_permissive_dacls_are_rejected_without_repair() -> Result<()> {
    let (temp, folder) = fixture()?;
    std::fs::write(temp.path().join("inherited"), b"exposed")?;
    let inherited = open_read_at(&folder.file, "inherited")?;
    assert!(private::check(&inherited).is_err());
    assert!(set_private_file(&temp.path().join("inherited")).is_err());
    let file = create_private_at(&folder.file, "permissive", Access::ReadWrite)?;
    let output = std::process::Command::new("icacls")
        .arg(temp.path().join("permissive"))
        .args(["/grant", "*S-1-1-0:(R)"])
        .output()?;
    ensure!(
        output.status.success(),
        "icacls fixture failed: {:?}",
        output.status
    );
    assert!(private::check(&file).is_err());
    assert!(private::created(&file).is_err());
    Ok(())
}
#[test]
fn relative_rename_conflicts_even_with_open_target_and_preserves_anchor() -> Result<()> {
    let (temp, folder) = fixture()?;
    let mut source = create_private_at(&folder.file, "source", Access::ReadWrite)?;
    source.write_all(b"new")?;
    source.sync_all()?;
    let mut target = create_private_at(&folder.file, "target", Access::ReadWrite)?;
    target.write_all(b"old")?;
    let held = std::fs::File::open(temp.path().join("target"))?;
    assert!(is_exists(
        &rename(&folder.file, "source", &folder.file, "target", false).unwrap_err()
    ));
    assert_eq!(std::fs::read(temp.path().join("target"))?, b"old");
    let error = rename(&folder.file, "source", &folder.file, "TARGET", false).unwrap_err();
    let collision = error.chain().find_map(code).expect("Win32 collision error");
    println!("case-insensitive no-replace rename error: {collision}");
    assert!(matches!(
        collision,
        ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS
    ));
    assert!(is_exists(&error));
    drop(held);
    let destination = Folder::open_private(&temp.path().join("destination"))?;
    std::fs::rename(temp.path().join("destination"), temp.path().join("moved"))?;
    std::fs::create_dir(temp.path().join("destination"))?;
    rename(&folder.file, "source", &destination.file, "placed", false)?;
    assert_eq!(
        identity(&source)?,
        identity_at(&destination.file, "placed")?
    );
    assert_eq!(std::fs::read(temp.path().join("moved/placed"))?, b"new");
    assert!(!temp.path().join("destination/placed").exists());
    rename(&destination.file, "placed", &folder.file, "target", true)?;
    assert_eq!(std::fs::read(temp.path().join("target"))?, b"new");
    let mut old = String::new();
    target.rewind()?;
    target.read_to_string(&mut old)?;
    assert_eq!(old, "old");
    Ok(())
}
#[test]
fn identity_checked_delete_refuses_swapped_files_and_directories() -> Result<()> {
    let (temp, folder) = fixture()?;
    let original = create_private_at(&folder.file, "file", Access::ReadWrite)?;
    std::fs::rename(temp.path().join("file"), temp.path().join("old"))?;
    let replacement = create_private_at(&folder.file, "file", Access::ReadWrite)?;
    assert_ne!(identity(&original)?, identity(&replacement)?);
    remove_file(&folder.file, "file", &original)?;
    assert!(temp.path().join("file").exists());
    remove_file(&folder.file, "old", &original)?;
    remove_file(&folder.file, "old", &original)?;
    assert!(!temp.path().join("old").exists());
    let original = Folder::open_private(&temp.path().join("dir"))?;
    std::fs::rename(temp.path().join("dir"), temp.path().join("old-dir"))?;
    std::fs::create_dir(temp.path().join("dir"))?;
    remove_directory(&folder.file, "dir", &original.file)?;
    assert!(temp.path().join("dir").exists());
    remove_directory(&folder.file, "old-dir", &original.file)?;
    assert!(!temp.path().join("old-dir").exists());
    Ok(())
}
#[test]
fn positional_reads_and_copy_preserve_source_shared_cursor() -> Result<()> {
    let (_temp, folder) = fixture()?;
    let mut source = create_private_at(&folder.file, "source", Access::ReadWrite)?;
    source.write_all(b"0123456789")?;
    source.seek(SeekFrom::Start(7))?;
    let clone = source.try_clone()?;
    let mut bytes = [0; 3];
    assert_eq!(read_at(&clone, &mut bytes, 2)?, 3);
    assert_eq!(&bytes, b"234");
    assert_eq!(source.stream_position()?, 7);
    std::thread::scope(|scope| {
        for offset in 0..10 {
            let f = &source;
            scope.spawn(move || {
                let mut b = [0];
                read_at(f, &mut b, offset).unwrap();
                assert_eq!(b[0], b'0' + offset as u8);
            });
        }
    });
    assert_eq!(source.stream_position()?, 7);
    let mut destination = create_private_at(&folder.file, "copy", Access::ReadWrite)?;
    destination.write_all(b"xxxxxxxxx")?;
    copy_prefix(&source, &destination, 5)?;
    assert_eq!(destination.stream_position()?, 5);
    assert_eq!(source.stream_position()?, 7);
    destination.write_all(b"tail")?;
    destination.rewind()?;
    let mut bytes = Vec::new();
    destination.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"01234tail");
    Ok(())
}
#[test]
fn enumeration_space_and_versions_use_handles() -> Result<()> {
    let (temp, folder) = fixture()?;
    let mut file = create_private_at(&folder.file, "café-写真", Access::ReadWrite)?;
    let before = Version::read(&file)?;
    file.write_all(b"changed")?;
    file.sync_all()?;
    assert!(Version::read_regular(&file)? != before);
    assert!(folder.space()? > 0);
    let names = entries(&folder.file)?.collect::<Result<Vec<_>>>()?;
    assert_eq!(names, vec![OsString::from("café-写真")]);
    assert_eq!(entries(&folder.file)?.count(), 1);
    let old = folder.identity()?;
    assert_eq!(identity(&Folder::open(temp.path(), false)?.file)?, old);
    Ok(())
}
#[test]
fn recorded_writer_versions_finalize_times_without_freezing_other_writers() -> Result<()> {
    let (temp, folder) = fixture()?;
    let mut records = Vec::new();
    for (name, access) in [("write", Access::Write), ("readwrite", Access::ReadWrite)] {
        let mut writer = create_private_at(&folder.file, name, access)?;
        writer.write_all(b"owned")?;
        // A known stale write time makes the finalization assertion independent
        // of whether this NTFS volume happens to defer automatic timestamps.
        writer.set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))?;
        let creation = query::<FILE_BASIC_INFO>(&writer, FileBasicInfo)?.CreationTime;
        let before = std::time::SystemTime::now();
        let version = Version::record_written(&writer)?;
        let basic: FILE_BASIC_INFO = query(&writer, FileBasicInfo)?;
        assert_eq!(basic.CreationTime, creation);
        assert!(writer.metadata()?.modified()? >= before);
        assert_eq!(basic.LastWriteTime, basic.ChangeTime);
        private::check(&writer)?;
        let reader = open_read_at(&folder.file, name)?;
        assert!(Version::read_regular(&reader)? == version);
        let clone = writer.try_clone()?;
        records.push((writer, clone, reader, version, temp.path().join(name)));
    }
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let records: Vec<_> = records
        .into_iter()
        .map(|(writer, clone, reader, version, path)| {
            drop(writer);
            drop(clone);
            (reader, version, path)
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(2100));
    for (reader, version, path) in records {
        assert!(Version::read_regular(&reader)? == version);
        let modified = reader.metadata()?.modified()?;
        std::fs::write(&path, b"other")?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_times(std::fs::FileTimes::new().set_modified(modified))?;
        assert!(Version::read_regular(&reader)? != version);
    }
    Ok(())
}
#[test]
fn locks_contend_across_independent_handles() -> Result<()> {
    let (temp, _) = fixture()?;
    let first = open_lock(&temp.path().join("lock"))?;
    let second = open_lock(&temp.path().join("lock"))?;
    first.try_lock()?;
    assert!(matches!(
        second.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    first.unlock()?;
    second.try_lock()?;
    second.unlock()?;
    drop(first);
    drop(second);
    open_lock(&temp.path().join("lock"))?.try_lock()?;
    Ok(())
}
#[test]
fn anonymous_clipboard_backing_remains_readable_by_handle() -> Result<()> {
    let (temp, _) = fixture()?;
    let mut file = clipboard_file(temp.path())?;
    file.write_all(b"clipboard")?;
    let mut buf = [0; 9];
    assert_eq!(read_at(&file, &mut buf, 0)?, 9);
    assert_eq!(&buf, b"clipboard");
    assert_eq!(std::fs::read_dir(temp.path())?.count(), 0);
    Ok(())
}
#[test]
fn unsafe_relative_names_and_nonlocal_prefixes_are_rejected() -> Result<()> {
    let (_temp, folder) = fixture()?;
    for name in ["../file", "a:b", r"a\b", ".", "..", ""] {
        assert!(
            create_private_at(&folder.file, name, Access::ReadWrite).is_err(),
            "{name}"
        );
    }
    assert!(Folder::open(Path::new(r"\\server\share\folder"), false).is_err());
    assert!(open_regular_file(Path::new("relative")).is_err());
    Ok(())
}
#[test]
fn reparse_junctions_are_rejected_in_every_component() -> Result<()> {
    reparse(false)
}
#[test]
fn reparse_symlinks_are_rejected_in_every_component() -> Result<()> {
    reparse(true)
}
fn reparse(symbolic: bool) -> Result<()> {
    let (temp, _) = fixture()?;
    // Limited batch accounts cannot create symlinks without Developer Mode.
    // They can run every rejection assertion against an admin-created fixture.
    let existing = symbolic
        .then(|| std::env::var_os("PNEU_TEST_SYMLINK_FIXTURE"))
        .flatten()
        .map(PathBuf::from);
    let root = existing.as_deref().unwrap_or(temp.path());
    if existing.is_none() {
        std::fs::create_dir_all(root.join("real/a/b"))?;
        std::fs::write(root.join("real/a/b/file"), b"original")?;
        for (link, target) in [
            ("link", "real"),
            ("real/link", "real/a"),
            ("real/a/link", "real/a/b"),
        ] {
            let link = root.join(link);
            let target = root.join(target);
            if symbolic {
                std::os::windows::fs::symlink_dir(&target, &link)?;
            } else {
                let output = std::process::Command::new("cmd")
                    .args(["/c", "mklink", "/J"])
                    .arg(link.to_string_lossy().replace('/', r"\"))
                    .arg(target.to_string_lossy().replace('/', r"\"))
                    .output()?;
                ensure!(
                    output.status.success(),
                    "junction fixture failed: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
    for path in [
        "link/a/b/file",
        "real/link/b/file",
        "real/a/link/file",
        "link/../real/a/b/file",
    ] {
        assert!(
            is_link(&open_regular_file(&root.join(path)).unwrap_err()),
            "{path}"
        );
    }
    for path in ["link/a/b", "real/link/b", "real/a/link"] {
        assert!(Folder::open(&root.join(path), false).is_err());
        assert!(Folder::open_private(&root.join(path)).is_err());
        for save_to_folder in [false, true] {
            let error = crate::Config {
                save_path: root.join(path),
                save_to_folder,
                ..crate::Config::default()
            }
            .validate_with_cache(&temp.path().join("cache"))
            .unwrap_err();
            assert!(is_link(&error), "{path}: {error:#}");
        }
    }
    if symbolic {
        let link = root.join("real/a/b/link");
        if existing.is_none() {
            std::os::windows::fs::symlink_file(root.join("real/a/b/file"), &link)?;
        }
        assert!(is_link(&open_regular_file(&link).unwrap_err()));
    }
    Ok(())
}
#[test]
fn windows_storage_errors_retain_their_os_cause() {
    let error = anyhow::Error::from(no_space()).context("receipt");
    assert_eq!(
        crate::store::save_failure_code(&error),
        pneu_proto::NackCode::DiskFull
    );
    assert_eq!(
        crate::store::save_failure_code(&permission_denied()),
        pneu_proto::NackCode::NotWritable
    );
}

#[test]
fn ntfs_refuses_live_descendant_directory_rename_without_redirecting_handles() -> Result<()> {
    let (temp, root) = fixture()?;
    let directory = Folder::open_private(&temp.path().join("live"))?;
    let file = create_private_at(&directory.file, "child", Access::ReadWrite)?;
    let error = rename(&root.file, "live", &root.file, "moved", false).unwrap_err();
    assert!(error.chain().any(|e| code(e) == Some(ERROR_ACCESS_DENIED)));
    assert_eq!(identity(&file)?, identity_at(&directory.file, "child")?);
    assert!(temp.path().join("live/child").exists());
    assert!(!temp.path().join("moved").exists());
    drop(file);
    rename(&root.file, "live", &root.file, "moved", false)?;
    // Compare resolved paths: the temporary directory may be spelled with an 8.3 short name.
    assert_eq!(
        std::fs::canonicalize(directory.path()?)?,
        std::fs::canonicalize(temp.path().join("moved"))?
    );
    Ok(())
}

#[test]
fn configured_win32_alias_names_stay_strict_while_receive_names_are_mapped() -> Result<()> {
    let (temp, folder) = fixture()?;
    let source = create_private_at(&folder.file, "source", Access::ReadWrite)?;
    source.sync_all()?;
    let staging = crate::storage::staging_directory(&folder)?;
    for name in [
        "data.",
        "data ",
        "file:stream",
        "CON",
        "nul.txt",
        "COM1.log",
        "lpt².txt",
        "CON .txt",
        "CONIN$",
        "a?b",
        "a*b",
        "a\"b",
        "a|b",
        "a<b",
        "a\u{1}b",
    ] {
        let path = temp.path().join(name);
        assert!(validate_path(&path).is_err(), "{name}");
        let stage = crate::storage::Stage::create(folder.clone(), staging.clone(), name.into(), 0)?;
        assert_ne!(stage.0.destination, name);
        validate_name(OsStr::new(&stage.0.destination))?;
        drop(stage);
        assert_eq!(entries(&staging.file)?.count(), 0);
        assert!(Folder::open_private(&path).is_err(), "{name}");
        assert!(Folder::open(&path.join("child"), true).is_err(), "{name}");
        assert!(open_read(&path).is_err(), "{name}");
        assert!(open_regular_file(&path).is_err(), "{name}");
        assert!(open_lock(&path).is_err(), "{name}");
        assert!(
            create_private_at(&folder.file, name, Access::ReadWrite).is_err(),
            "{name}"
        );
        assert!(
            rename(&folder.file, "source", &folder.file, name, false).is_err(),
            "{name}"
        );
        assert!(
            crate::config::atomic_private(&path, b"secret").is_err(),
            "{name}"
        );
        assert!(crate::Config::load(&path).is_err(), "{name}");
        assert!(
            crate::store::Store::open(&path.join("state.sqlite3")).is_err(),
            "{name}"
        );
        assert!(crate::store::Store::open(&path).is_err(), "{name}");
        assert!(
            crate::Config {
                save_path: path,
                ..crate::Config::default()
            }
            .validate_with_cache(&temp.path().join("cache"))
            .is_err(),
            "{name}"
        );
    }
    assert!(validate_name(&OsString::from_wide(&[0xd800])).is_err());
    assert!(Folder::open_private(&temp.path().join("new/bad./child")).is_err());
    assert!(!temp.path().join("new").exists());
    assert_eq!(entries(&folder.file)?.count(), 2);
    for path in [
        r"\\?\C:\pneu",
        r"\\.\C:\pneu",
        r"\\server\share\pneu",
        r"\\?\UNC\server\share\pneu",
        r"C:relative",
    ] {
        assert!(validate_path(Path::new(path)).is_err(), "{path}");
        assert!(Folder::open_private(Path::new(path)).is_err(), "{path}");
    }
    Ok(())
}

#[test]
fn short_renames_support_fresh_collision_replace_and_read_only() -> Result<()> {
    for name in ["a", "bc", "🦀"] {
        for readonly in [false, true] {
            let (temp, folder) = fixture()?;
            let mut source = create_private_at(&folder.file, "source", Access::ReadWrite)?;
            source.write_all(b"new")?;
            source.sync_all()?;
            if readonly {
                set_read_only(&source)?;
            }
            let id = identity(&source)?;
            rename(&folder.file, "source", &folder.file, name, false)?;
            assert_eq!(identity_at(&folder.file, name)?, id);
            assert_eq!(std::fs::read(temp.path().join(name))?, b"new");
            let mut contender = create_private_at(&folder.file, "contender", Access::ReadWrite)?;
            contender.write_all(b"replacement")?;
            contender.sync_all()?;
            if readonly {
                set_read_only(&contender)?;
            }
            let error = rename(&folder.file, "contender", &folder.file, name, false).unwrap_err();
            assert!(is_exists(&error), "{error:#}");
            assert_eq!(std::fs::read(temp.path().join(name))?, b"new");
            // Read-only destination replacement is a separate OS policy. Keep
            // the source frozen, and use a writable destination for replacement.
            if readonly {
                let mut p = source.metadata()?.permissions();
                p.set_readonly(false);
                source.set_permissions(p)?;
            }
            rename(&folder.file, "contender", &folder.file, name, true)?;
            assert_eq!(identity_at(&folder.file, name)?, identity(&contender)?);
            assert_eq!(std::fs::read(temp.path().join(name))?, b"replacement");
        }
    }
    Ok(())
}

#[test]
fn regular_file_parent_traversal_retains_validated_ancestors() -> Result<()> {
    let (temp, _) = fixture()?;
    std::fs::create_dir_all(temp.path().join("real/a/b"))?;
    let path = temp.path().join("real/a/b/file");
    std::fs::write(&path, b"original")?;
    for relative in ["real/a/b/../b/file", "real/a/b/../../a/b/file"] {
        assert_eq!(
            identity(&open_regular_file(&temp.path().join(relative))?)?,
            identity(&open_regular_file(&path)?)?
        );
        assert_eq!(
            identity(&open_read(&temp.path().join(relative))?)?,
            identity(&open_regular_file(&path)?)?
        );
    }
    // Existing reparse tests cover a junction/symlink immediately before "..".
    Ok(())
}

fn cache_aliases(path: &Path) -> Result<Vec<PathBuf>> {
    let mut aliases = vec![path.to_owned(), path.with_file_name("ACTUALCACHELONGNAME")];
    let input = wide(path.as_os_str());
    let mut buf = vec![0u16; 32768];
    // SAFETY: terminated input and sized output UTF-16 buffers.
    let len =
        unsafe { GetShortPathNameW(input.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    ensure!(len > 0 && len < buf.len(), "cannot query short path");
    let short = PathBuf::from(OsString::from_wide(&buf[..len]));
    if short != path {
        aliases.push(short);
    } else {
        eprintln!("8.3 names unavailable on this volume; long/case aliases covered");
    }
    Ok(aliases)
}

#[test]
fn config_overlap_uses_directory_identities_and_missing_suffixes() -> Result<()> {
    let (temp, _) = fixture()?;
    let cache = temp.path().join("ActualCacheLongName");
    Folder::open_private(&cache)?;
    let aliases = cache_aliases(&cache)?;
    for alias in &aliases {
        for configured_cache in &aliases {
            for save in [
                alias.clone(),
                alias.join("new/nested"),
                alias.join("new/../nested"),
            ] {
                assert!(folder_overlaps_cache(&save, configured_cache)?);
                assert!(
                    crate::Config {
                        save_path: save,
                        ..crate::Config::default()
                    }
                    .validate_with_cache(configured_cache)
                    .is_err()
                );
            }
        }
    }
    for save in [
        temp.path().to_owned(),
        temp.path().join("ActualCacheLongName-sibling/new"),
    ] {
        assert!(!folder_overlaps_cache(&save, &cache)?);
        crate::Config {
            save_path: save,
            ..crate::Config::default()
        }
        .validate_with_cache(&cache)?;
    }
    assert!(folder_overlaps_cache(
        &temp.path().join("FUTURE/new"),
        &temp.path().join("future")
    )?);
    assert!(folder_overlaps_cache(
        &temp.path().join("missing/../FUTURE/new"),
        &temp.path().join("future")
    )?);
    for path in [
        temp.path().join("ActualCacheLongName."),
        temp.path().join("ActualCacheLongName "),
    ] {
        assert!(
            crate::Config {
                save_path: path,
                ..crate::Config::default()
            }
            .validate_with_cache(&cache)
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn inherited_children_require_exact_grants_and_a_protected_parent() -> Result<()> {
    let (temp, folder) = fixture()?;
    std::fs::write(temp.path().join("child"), b"private from birth")?;
    private::check_in(&folder.file, OsStr::new("child"))?;
    assert!(private::check(&open_read_at(&folder.file, "child")?).is_err());
    for args in [["/grant", "*S-1-1-0:(R)"], ["/grant:r", "*S-1-5-18:(R)"]] {
        let output = std::process::Command::new("icacls")
            .arg(temp.path().join("child"))
            .args(args)
            .output()?;
        ensure!(output.status.success(), "icacls fixture failed");
        assert!(private::check_in(&folder.file, OsStr::new("child")).is_err());
    }
    let foreign = tempfile::tempdir()?;
    let parent = Folder::open(foreign.path(), false)?;
    std::fs::write(foreign.path().join("child"), b"exposed")?;
    assert!(private::check_in(&parent.file, OsStr::new("child")).is_err());
    Ok(())
}

fn fixture_dacl(path: &Path, aces: &str) -> Result<()> {
    let sddl = wide(OsStr::new(&format!("D:P{aces}")));
    let mut sd = null_mut();
    // SAFETY: terminated test-only SDDL/path, descriptor retained for the call.
    unsafe {
        win(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut sd,
            null_mut(),
        ))?;
        let sd = Local(sd);
        win(SetFileSecurityW(
            wide(path.as_os_str()).as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            sd.0,
        ))?;
    }
    Ok(())
}

#[test]
fn config_availability_classification_excludes_unsafe_and_unknown_errors() {
    for code in [
        ERROR_FILE_NOT_FOUND,
        ERROR_PATH_NOT_FOUND,
        ERROR_NOT_READY,
        ERROR_DEVICE_NOT_CONNECTED,
        ERROR_INVALID_DRIVE,
        ERROR_ACCESS_DENIED,
        ERROR_BAD_NETPATH,
    ] {
        let error = anyhow::Error::from(std::io::Error::from_raw_os_error(code as i32))
            .context("save folder resolution");
        assert!(save_folder_unavailable(&error), "{code}: {error:#}");
    }
    for code in [
        ERROR_CANT_ACCESS_FILE,
        ERROR_DIRECTORY,
        ERROR_INVALID_NAME,
        ERROR_SHARING_VIOLATION,
        ERROR_GEN_FAILURE,
        ERROR_NOT_SUPPORTED,
    ] {
        let error = std::io::Error::from_raw_os_error(code as i32).into();
        assert!(!save_folder_unavailable(&error), "{code}");
    }
    for message in [
        "remote storage",
        "identity mismatch",
        "ambiguous Windows path",
    ] {
        assert!(!save_folder_unavailable(&anyhow::anyhow!(message)));
    }
}

#[test]
fn config_unavailable_save_paths_remain_recoverable() -> Result<()> {
    const TEST: &str =
        "platform::fs::windows::tests::config_unavailable_save_paths_remain_recoverable";
    const CHILD: &str = "PNEU_TEST_UNAVAILABLE_SAVE_CHILD";
    let (temp, _) = fixture()?;
    if std::env::var_os(CHILD).is_none() {
        // Isolate config's real environment-derived cache without mutating the
        // environment of other concurrently executing tests.
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("PNEU_CACHE_DIR", temp.path().join("cache"))
            .output()?;
        ensure!(
            output.status.success(),
            "unavailable save child failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }
    // Other DOS-alias tests allocate K:..W:; the runner owns X:..Z:.
    let unavailable = (b'D'..=b'J')
        .find_map(|letter| {
            let drive = format!("{}:", letter as char);
            let mut buffer = [0u16; 32768];
            let len = unsafe {
                QueryDosDeviceW(
                    wide(OsStr::new(&drive)).as_ptr(),
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                )
            };
            (len == 0
                && std::io::Error::last_os_error().raw_os_error()
                    == Some(ERROR_FILE_NOT_FOUND as i32))
            .then(|| PathBuf::from(format!("{drive}\\")))
        })
        .context("no unused drive letter for unavailable-save test")?;
    let denied = temp.path().join("denied");
    Folder::open(&denied, true).context("create deny-ACE fixture")?;
    let own = own_sid()?;
    // Deny directory reads, including READ_CONTROL, but preserve WRITE_DAC on
    // the retained handle so even a standard user can restore the fixture.
    struct RestoreDacl(File, Local);
    impl Drop for RestoreDacl {
        fn drop(&mut self) {
            // SAFETY: retained WRITE_DAC handle and valid owned descriptor.
            unsafe {
                win(SetKernelObjectSecurity(
                    self.0.as_raw_handle(),
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    self.1.0,
                ))
                .expect("restore task deny-ACE fixture");
            }
        }
    }
    let _restore = RestoreDacl(
        root_file(&denied, WRITE_DAC | READ).context("retain fixture WRITE_DAC")?,
        security()?,
    );
    fixture_dacl(
        &denied,
        &format!("(D;;0x20089;;;{own})(A;OICI;FA;;;{own})(A;OICI;FA;;;SY)"),
    )
    .context("apply fixture deny-ACE")?;
    // The denied-path probe uses a fresh traversal, just as admission does.
    let error = folder_location(&denied.join("received"))
        .err()
        .context("deny-ACE fixture unexpectedly readable")?;
    assert!(error.chain().any(|e| code(e) == Some(ERROR_ACCESS_DENIED)));
    let config_path = temp.path().join("config.toml");
    for save_path in [unavailable.join("received"), denied.join("received")] {
        assert!(folder_location(&save_path).is_err());
        assert!(crate::config::save_folder_overlaps_cache(
            &save_path,
            &crate::platform::cache_dir_from_env()?
        ));
        for save_to_folder in [false, true] {
            let mut config = crate::Config {
                name: "recoverable-save".into(),
                save_path: save_path.clone(),
                save_to_folder,
                ..crate::Config::default()
            };
            config
                .save(&config_path)
                .with_context(|| format!("save unavailable {save_path:?}"))?;
            let loaded = crate::Config::load(&config_path)
                .with_context(|| format!("load unavailable {save_path:?}"))?;
            assert_eq!(loaded.save_path, save_path);
            assert_eq!(loaded.save_to_folder, save_to_folder);
            config.notification_preview = !loaded.notification_preview;
            config.save(&config_path)?;
            assert_eq!(
                crate::Config::load(&config_path)?.notification_preview,
                config.notification_preview
            );
            // Loading a legacy config also writes its hostname migration.
            config.name = "Pneu PC".into();
            config.save(&config_path)?;
            assert_eq!(
                crate::Config::load(&config_path)?.name,
                crate::platform::machine_hostname()
            );
        }
        let config = crate::Config {
            save_path,
            ..crate::Config::default()
        };
        let error = config
            .validate_with_cache(&unavailable.join("cache"))
            .unwrap_err();
        assert!(format!("{error:#}").contains("cannot validate cache directory"));
        assert!(config.save(&unavailable.join("config.toml")).is_err());
    }
    let error = crate::Config::load(&unavailable.join("config.toml")).unwrap_err();
    assert!(format!("{error:#}").contains("cannot open config directory"));
    for path in [
        unavailable.join("data"),
        unavailable.join("cache"),
        denied.join("data"),
        denied.join("cache"),
    ] {
        assert!(Folder::open_private(&path).is_err());
    }
    let config = crate::Config {
        save_path: unavailable.join("received"),
        ..crate::Config::default()
    };
    let error = config
        .validate_with_cache(&denied.join("cache"))
        .unwrap_err();
    assert!(format!("{error:#}").contains("cannot validate cache directory"));
    assert!(config.save(&denied.join("config.toml")).is_err());
    let error = crate::Config::load(&denied.join("config.toml")).unwrap_err();
    assert!(format!("{error:#}").contains("cannot open config directory"));
    Ok(())
}

#[test]
fn existing_private_directories_require_the_creation_inheritance_policy() -> Result<()> {
    let (temp, root) = fixture()?;
    let own = own_sid()?;
    let store = crate::store::Store::open(&temp.path().join("valid.sqlite3"))?;
    for (case, aces) in [
        ("no-inheritance", format!("(A;;FA;;;{own})(A;;FA;;;SY)")),
        (
            "reduced-system",
            format!("(A;OICI;FA;;;{own})(A;OICI;FR;;;SY)"),
        ),
        ("missing-system", format!("(A;OICI;FA;;;{own})")),
        ("files-only", format!("(A;OI;FA;;;{own})(A;OI;FA;;;SY)")),
        ("dirs-only", format!("(A;CI;FA;;;{own})(A;CI;FA;;;SY)")),
        (
            "no-propagation",
            format!("(A;OICINP;FA;;;{own})(A;OICINP;FA;;;SY)"),
        ),
        (
            "inherit-only-system",
            format!("(A;OICI;FA;;;{own})(A;OICIIO;FA;;;SY)"),
        ),
        (
            "duplicate-user",
            format!("(A;OICI;FA;;;{own})(A;OICI;FA;;;{own})"),
        ),
    ] {
        let path = temp.path().join(case);
        let dir = Folder::open_private(&path)?;
        // All protected directories created by the backend are accepted.
        private::check(&dir.file)?;
        fixture_dacl(&path, &aces)?;
        // Direct checking and every private-directory admission predicate use
        // the same exact policy (data/config/cache/runtime/outbox/viewer/locks).
        assert!(private::check(&dir.file).is_err(), "{case}");
        assert!(private::created(&dir.file).is_err(), "{case}");
        assert!(!is_private_directory(&dir.file)?, "{case}");
        assert!(!is_owned_private(&dir.file)?, "{case}");
        assert!(set_private_directory(&dir.file).is_err(), "{case}");
        assert!(Folder::open_private(&path).is_err(), "{case}");
        assert!(crate::config::private_dir(&path).is_err(), "{case}");
        assert!(
            crate::store::Store::open(&path.join("state.sqlite3")).is_err(),
            "{case}"
        );
        assert!(!path.join("state.sqlite3").exists(), "{case}");
        assert!(
            crate::outbox::Outbox::open(store.clone(), &path).is_err(),
            "{case}"
        );
        // Even an explicitly protected child must not bypass parent admission.
        create_private_at(&dir.file, "child", Access::ReadWrite)?;
        assert!(
            private::check_in(&dir.file, OsStr::new("child")).is_err(),
            "{case}"
        );
        // The existing staging-directory admission is another directory check.
        create_private_directory_at(&root.file, ".pneu-staging")?;
        let staging = temp.path().join(".pneu-staging");
        fixture_dacl(&staging, &aces)?;
        assert!(crate::storage::staging_directory(&root).is_err(), "{case}");
        std::fs::remove_dir(&staging)?;
    }
    Ok(())
}

#[test]
fn accepted_private_parent_propagates_exact_grants_to_recreated_children() -> Result<()> {
    let (temp, folder) = fixture()?;
    Folder::open_private(temp.path())?;
    for name in ["db-wal", "db-shm", "db-journal"] {
        // Recreate using default OS inheritance as SQLite does, then re-open
        // the accepted existing parent, just as Store does after a restart.
        std::fs::write(temp.path().join(name), b"first")?;
        std::fs::remove_file(temp.path().join(name))?;
        std::fs::write(temp.path().join(name), b"recreated")?;
        let reopened = Folder::open_private(temp.path())?;
        private::check_in(&reopened.file, OsStr::new(name))?;
        assert!(private::check(&open_read_at(&folder.file, name)?).is_err());
    }
    Ok(())
}

#[test]
fn mapped_remote_devices_are_rejected_by_all_path_entry_points() -> Result<()> {
    // VM runner supplies a real loopback SMB mapping in each logon session.
    // Hosted CI need not enable a file-sharing service to run portable tests.
    let Some(root) = std::env::var_os("PNEU_TEST_REMOTE_ROOT") else {
        eprintln!(
            "SMB fixture not supplied; native mapped-drive acceptance requires the VM runner"
        );
        return Ok(());
    };
    let remote = PathBuf::from(root);
    // Prove the mapping is readable/writable through ordinary Win32 APIs;
    // unrelated mapping/permission failures must not count as policy rejection.
    assert_eq!(std::fs::read(remote.join("source"))?, b"remote fixture");
    std::fs::write(remote.join("writable-probe"), b"probe")?;
    std::fs::remove_file(remote.join("writable-probe"))?;
    let local = crate::test_support::scratch()?;
    Folder::open(local.path(), false)?;
    Folder::open_private(local.path())?;
    open_regular_file(&{
        let path = local.path().join("source");
        std::fs::write(&path, b"local fixture")?;
        path
    })?;
    if let Some(alias) = std::env::var_os("PNEU_TEST_LOCAL_ALIAS_ROOT") {
        let fixture = tempfile::tempdir_in(PathBuf::from(alias))?;
        let alias = fixture.path();
        let dir = Folder::open_private(&alias.join("private"))?;
        private::check(&dir.file)?;
        let file = create_private_at(&dir.file, "source", Access::ReadWrite)?;
        assert_eq!(
            identity(&file)?,
            identity(&open_regular_file(&alias.join("private/source"))?)?
        );
    }
    let mut roots = vec![remote];
    if let Some(alias) = std::env::var_os("PNEU_TEST_REMOTE_ALIAS_ROOT") {
        roots.push(PathBuf::from(alias));
    }
    for remote in roots {
        assert_eq!(std::fs::read(remote.join("source"))?, b"remote fixture");
        let file = remote.join("source");
        let checks: Vec<Result<()>> = vec![
            Folder::open(&remote, false).map(|_| ()),
            Folder::open(&remote.join("received-files"), true).map(|_| ()),
            Folder::open_private(&remote.join("private")).map(|_| ()),
            open_regular_file(&file).map(|_| ()),
            open_read(&file).map(|_| ()),
            open_secret(&file).map(|_| ()).map_err(Into::into),
            open_lock(&remote.join("lock")).map(|_| ()),
            open_lock(&remote.join("store-lock")).map(|_| ()),
            crate::config::private_dir(&remote.join("cache")),
            crate::config::atomic_private(&remote.join("config.toml"), b"secret"),
            crate::Config::load(&remote.join("config.toml")).map(|_| ()),
            crate::store::Store::open(&remote.join("state.sqlite3")).map(|_| ()),
            folder_overlaps_cache(&remote, local.path()).map(|_| ()),
            folder_overlaps_cache(local.path(), &remote).map(|_| ()),
            crate::Config {
                save_path: remote.clone(),
                ..crate::Config::default()
            }
            .validate_with_cache(local.path()),
            crate::Config {
                save_path: remote.join("missing"),
                save_to_folder: false,
                ..crate::Config::default()
            }
            .validate_with_cache(local.path()),
            sync_parent(&remote.join("new-file")),
        ];
        for (index, check) in checks.into_iter().enumerate() {
            let error = check.expect_err(&format!("remote entry point {index} accepted storage"));
            assert!(
                format!("{error:#}").contains("remote storage"),
                "entry point {index}: {error:#}"
            );
        }
        for name in [
            "received-files",
            "private",
            "cache",
            "lock",
            "store-lock",
            "config.toml",
            "state.sqlite3",
        ] {
            assert!(
                !remote.join(name).exists(),
                "unexpected remote creation: {name}"
            );
        }
    }
    Ok(())
}

// Each mapping is task-owned and removed on unwind. Allocate under a process
// mutex so concurrent tests cannot select the same free DOS drive letter.
pub(crate) struct TestDrive {
    letter: String,
    target: String,
}
impl TestDrive {
    pub(crate) fn new(path: &Path) -> Result<Self> {
        static ALLOCATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ALLOCATION.lock().unwrap();
        let target = format!(r"\??\{}", path.display().to_string().replace('/', r"\"));
        for letter in b'K'..=b'W' {
            let letter = format!("{}:", letter as char);
            let mut existing = [0u16; 32768];
            if unsafe {
                QueryDosDeviceW(
                    wide(OsStr::new(&letter)).as_ptr(),
                    existing.as_mut_ptr(),
                    existing.len() as u32,
                )
            } != 0
            {
                continue;
            }
            unsafe {
                win(DefineDosDeviceW(
                    9,
                    wide(OsStr::new(&letter)).as_ptr(),
                    wide(OsStr::new(&target)).as_ptr(),
                ))?;
            }
            return Ok(Self { letter, target });
        }
        anyhow::bail!("no free test drive letter")
    }
    pub(crate) fn path(&self) -> PathBuf {
        PathBuf::from(format!("{}\\", self.letter))
    }
    pub(crate) fn repoint(&mut self, path: &Path) -> Result<()> {
        self.remove()?;
        self.target = format!(r"\??\{}", path.display().to_string().replace('/', r"\"));
        unsafe {
            win(DefineDosDeviceW(
                9,
                wide(OsStr::new(&self.letter)).as_ptr(),
                wide(OsStr::new(&self.target)).as_ptr(),
            ))?;
        }
        Ok(())
    }
    fn remove(&self) -> Result<()> {
        unsafe {
            win(DefineDosDeviceW(
                15,
                wide(OsStr::new(&self.letter)).as_ptr(),
                wide(OsStr::new(&self.target)).as_ptr(),
            ))?;
        }
        Ok(())
    }
}
impl Drop for TestDrive {
    fn drop(&mut self) {
        self.remove().expect("remove test mapping");
    }
}
fn short_path(path: &Path) -> Result<PathBuf> {
    let mut buf = [0u16; 32768];
    let n = unsafe {
        GetShortPathNameW(
            wide(path.as_os_str()).as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    } as usize;
    ensure!(n > 0 && n < buf.len(), "8.3 query failed");
    let short = PathBuf::from(OsString::from_wide(&buf[..n]));
    ensure!(
        short != path,
        "8.3 fixture unavailable; alias has not been tested"
    );
    Ok(short)
}
#[test]
fn store_alias_locks_canonical_sidecars_and_no_lost_commit() -> Result<()> {
    let (temp, _) = fixture()?;
    let data = temp.path().join("LongDataDirectoryForShortAlias");
    let parent = Folder::open_private(&data)?;
    let path = data.join("CommittedDatabaseLongName.sqlite3");
    let first = crate::store::Store::open(&path)?;
    assert!(!path.with_extension("lock").exists());
    let short = short_path(&path)?;
    let mapping = TestDrive::new(&data)?;
    let aliases = [
        path.clone(),
        short.clone(),
        data.join("COMMITTEDDATABASELONGNAME.SQLITE3"),
        mapping.path().join(path.file_name().unwrap()),
        short_path(&data)?.join(path.file_name().unwrap()),
    ];
    first.test_locked(|c| { c.execute_batch("CREATE TABLE proof(x); PRAGMA wal_autocheckpoint=0; PRAGMA wal_checkpoint(TRUNCATE); INSERT INTO proof VALUES(42);")?; Ok(()) })?;
    for alias in &aliases {
        let error = crate::store::Store::open(alias)
            .err()
            .context("alias bypassed ownership")?;
        assert!(
            format!("{error:#}").contains("already owned"),
            "{alias:?}: {error:#}"
        );
    }
    for suffix in ["-wal", "-shm"] {
        private::check_in(
            &parent.file,
            OsStr::new(&format!("CommittedDatabaseLongName.sqlite3{suffix}")),
        )?;
    }
    let short_wal = format!("{}-wal", short.file_name().unwrap().to_string_lossy());
    assert!(
        !entries(&parent.file)?
            .collect::<Result<Vec<_>>>()?
            .iter()
            .any(|n| n == short_wal.as_str())
    );
    let canonical = first.test_locked(|c| Ok(c.path().unwrap().to_owned()))?;
    assert!(canonical.starts_with(r"\\?\Volume{"));
    assert!(crate::store::Store::open(Path::new(&canonical)).is_err());
    // Internal GUID opens share the same identity lock. Public/config GUID
    // input remains rejected above, rather than widening the input parser.
    let lock = root_file(Path::new(&canonical), READ)?;
    assert_eq!(identity(&lock)?, identity(&open_read(&path)?)?);
    assert!(format!("{:#}", database_lock(&lock).unwrap_err()).contains("already owned"));
    drop(first);
    database_lock(&lock)?;
    drop(lock);
    for alias in &aliases {
        drop(crate::store::Store::open(alias)?);
    }
    let second = crate::store::Store::open(&short)?;
    second.test_locked(|c| {
        c.execute_batch("INSERT INTO proof VALUES(99)")?;
        Ok(())
    })?;
    drop(second);
    let final_store = crate::store::Store::open(&path)?;
    final_store.test_locked(|c| {
        assert_eq!(
            c.query_row("SELECT sum(x) FROM proof", [], |r| r.get::<_, i64>(0))?,
            141
        );
        Ok(())
    })?;
    drop(final_store);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.file_name().unwrap().to_os_string();
        sidecar.push(suffix);
        if open_read_at(&parent.file, &sidecar).is_ok() {
            private::check_in(&parent.file, &sidecar)?;
        }
    }
    Ok(())
}
#[test]
fn database_and_sidecar_hardlinks_are_rejected_before_writes() -> Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        for other_dir in [false, true] {
            let (temp, parent) = fixture()?;
            let path = temp.path().join("state.sqlite3");
            drop(crate::store::Store::open(&path)?);
            let source = temp.path().join(format!("state.sqlite3{suffix}"));
            if !source.exists() {
                drop(create_private_at(
                    &parent.file,
                    source.file_name().unwrap(),
                    Access::ReadWrite,
                )?);
            }
            let other = if other_dir {
                let p = temp.path().join("other");
                Folder::open_private(&p)?;
                p
            } else {
                temp.path().to_owned()
            };
            let alias = other.join(format!("alias.sqlite3{suffix}"));
            std::fs::hard_link(&source, &alias)?;
            let before = std::fs::read(&source)?;
            let error = crate::store::Store::open(&path)
                .err()
                .context("hardlink admitted")?;
            assert!(
                format!("{error:#}").contains("exactly one link"),
                "{error:#}"
            );
            if suffix.is_empty() {
                assert!(crate::store::Store::open(&alias).is_err());
            }
            assert_eq!(std::fs::read(&source)?, before);
            std::fs::remove_file(&alias)?;
            drop(crate::store::Store::open(&path)?);
        }
    }
    Ok(())
}
#[test]
fn database_lock_byte_coexists_with_sqlite_operations_and_reader() -> Result<()> {
    assert!(STORE_LOCK_OFFSET >= (1u64 << 63));
    let (temp, _) = fixture()?;
    let (conn, lock, _parent) = open_database(&temp.path().join("state.sqlite3"))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE proof(x); INSERT INTO proof VALUES(42); PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE; INSERT INTO proof VALUES(99); PRAGMA journal_mode=WAL; VACUUM;")?;
    let reader = rusqlite::Connection::open_with_flags_and_vfs(
        conn.path().unwrap(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        "win32-longpath",
    )?;
    assert_eq!(
        reader.query_row("SELECT sum(x) FROM proof", [], |r| r.get::<_, i64>(0))?,
        141
    );
    reconcile_database(&conn, &lock)?;
    Ok(())
}
#[test]
fn database_retarget_stays_in_retained_parent_and_reconciles_before_writes() -> Result<()> {
    for (bypass, existing) in [(false, false), (true, false), (true, true)] {
        let (temp, _) = fixture()?;
        let a = temp.path().join("A");
        let b = temp.path().join("B");
        Folder::open_private(&a)?;
        let bf = Folder::open_private(&b)?;
        if existing {
            drop(create_private_at(
                &bf.file,
                "state.sqlite3",
                Access::ReadWrite,
            )?);
        }
        let mut mapping = TestDrive::new(&a)?;
        let path = mapping.path().join("state.sqlite3");
        let result = open_database_inner(
            &path,
            || {
                mapping.repoint(&b)?;
                Ok(())
            },
            bypass,
        );
        if bypass {
            let error = result.err().context("bypass admitted")?;
            if existing {
                assert!(
                    format!("{error:#}").contains("identity changed"),
                    "{error:#}"
                );
            }
        } else {
            let (conn, _lock, parent) = result?;
            assert_eq!(parent.identity()?, Folder::open(&a, false)?.identity()?);
            assert!(conn.path().unwrap().starts_with(r"\\?\Volume{"));
            conn.execute_batch("CREATE TABLE stayed_in_A(x)")?;
        }
        assert_eq!(std::fs::read_dir(&b)?.count(), usize::from(existing));
        if existing {
            assert_eq!(std::fs::metadata(b.join("state.sqlite3"))?.len(), 0);
        }
    }
    Ok(())
}
#[test]
fn subst_physical_ancestry_covers_roots_missing_suffixes_and_hidden_junctions() -> Result<()> {
    let (temp, _) = fixture()?;
    let cache = temp.path().join("cache");
    let child = cache.join("child");
    Folder::open_private(&child)?;
    let mapping = TestDrive::new(&child)?;
    for save in [
        mapping.path(),
        mapping.path().join("nested"),
        mapping.path().join("missing/deep"),
    ] {
        assert!(folder_overlaps_cache(&save, &cache)?);
        assert!(
            crate::Config {
                save_path: save,
                ..crate::Config::default()
            }
            .validate_with_cache(&cache)
            .is_err()
        );
    }
    assert!(folder_overlaps_cache(&child, &mapping.path())?);
    assert!(folder_overlaps_cache(
        &child.join("missing"),
        &mapping.path()
    )?);
    assert!(retained_folder_overlaps_cache(
        &Folder::open(&mapping.path(), false)?,
        &cache
    )?);
    assert!(!folder_overlaps_cache(temp.path(), &mapping.path())?);
    assert!(!folder_overlaps_cache(
        &temp.path().join("sibling"),
        &mapping.path()
    )?);
    let junction = temp.path().join("junction");
    let output = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(junction.to_string_lossy().replace('/', r"\"))
        .arg(cache.to_string_lossy().replace('/', r"\"))
        .output()?;
    ensure!(output.status.success(), "junction fixture failed");
    let hidden = TestDrive::new(&junction.join("child"))?;
    match folder_overlaps_cache(&hidden.path(), &cache) {
        Ok(overlap) => assert!(overlap),
        Err(_) => assert!(Folder::open(&hidden.path(), false).is_err()),
    }
    drop(hidden);
    std::fs::remove_dir(junction)?;
    Ok(())
}
#[test]
fn sqlite_longpath_and_vfs_limit_preflight() -> Result<()> {
    let (temp, _) = fixture()?;
    let mut path = temp.path().to_owned();
    while path.as_os_str().encode_wide().count() < 300 {
        path.push("long-component-for-sqlite");
    }
    Folder::open_private(&path)?;
    drop(crate::store::Store::open(&path.join("state.sqlite3"))?);
    let canonical = CanonicalPath::from_handle(&Folder::open(&path, false)?.file)?;
    let huge = PathBuf::from(format!(r"{}\{}", canonical.0.display(), "a".repeat(65530)));
    assert!(database_path_fits(&huge).is_err());
    // Hit SQLite's byte-counted VFS limit before NT's UTF-16 limit. Admission
    // must fail before precreating the DB or any sidecar in this real directory.
    let mut limit_parent = temp.path().join("limit");
    let component = "界".repeat(200);
    while limit_parent.to_str().unwrap().len() < 65530 {
        limit_parent.push(&component);
    }
    let limited = Folder::open_private(&limit_parent)?;
    assert!(crate::store::Store::open(&limit_parent.join("state.sqlite3")).is_err());
    assert_eq!(entries(&limited.file)?.count(), 0);
    Ok(())
}
#[test]
fn nonstore_lock_open_prevents_rename_replace_and_delete() -> Result<()> {
    let (temp, parent) = fixture()?;
    let path = temp.path().join("runtime.lock");
    for _ in 0..2 {
        let lock = open_lock(&path)?;
        lock.try_lock()?;
        assert!(std::fs::rename(&path, temp.path().join("moved")).is_err());
        assert!(rename(&parent.file, "runtime.lock", &parent.file, "moved", false).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        drop(lock);
    }
    std::fs::rename(&path, temp.path().join("moved"))?;
    Ok(())
}
#[test]
fn legacy_readonly_regular_stage_does_not_abort_cleanup() -> Result<()> {
    let (temp, parent) = fixture()?;
    let name = format!(".pneu-stage-{}", "a".repeat(32));
    let file = create_private_at(&parent.file, &name, Access::ReadWrite)?;
    set_read_only(&file)?;
    assert!(is_not_directory(
        &open_directory_at(&parent.file, &name).unwrap_err()
    ));
    crate::storage::clean_staging(&parent)?;
    assert!(temp.path().join(name).exists());
    Ok(())
}
#[test]
fn received_readonly_handoff_unblocks_ordinary_save_and_cache_readers() -> Result<()> {
    for destination in ["saved.png", ".pneu-cache-file.png"] {
        let (temp, parent) = fixture()?;
        let file = create_private_at(&parent.file, "stage", Access::ReadWrite)?;
        (&file).write_all(b"received")?;
        rename(&parent.file, "stage", &parent.file, destination, false)?;
        let received = crate::actions::ReceivedFile::from_file(
            temp.path().join(destination),
            file.try_clone()?,
        )?;
        drop(file);
        let ordinary_reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(received.path())?;
        assert_eq!(received.read_bounded(20)?, b"received");
        assert_eq!(ordinary_reader.metadata()?.len(), 8);
    }
    Ok(())
}

#[test]
fn administrators_owned_inherited_sidecar_is_accepted_only_contextually() -> Result<()> {
    let Some(path) = std::env::var_os("PNEU_TEST_ADMIN_OWNED_PARENT") else {
        eprintln!("Administrators-owner cross-token fixture not supplied");
        return Ok(());
    };
    let parent = Folder::open(Path::new(&path), false)?;
    private::check(&parent.file)?;
    let name = OsStr::new("state.sqlite3-journal");
    let sidecar = open_read_at(&parent.file, name)?;
    let (mut owner, mut sd) = (null_mut(), null_mut());
    let code = unsafe {
        GetSecurityInfo(
            sidecar.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        )
    };
    ensure!(code == 0, "cannot inspect owner fixture");
    let _descriptor = Local(sd);
    assert_eq!(unsafe { sid_string(owner)? }, "S-1-5-32-544");
    assert!(private::check(&sidecar).is_err());
    private::check_in(&parent.file, name)?;
    drop(sidecar);
    drop(crate::store::Store::open(
        &PathBuf::from(path).join("state.sqlite3"),
    )?);
    Ok(())
}

#[test]
fn hardlink_loss_case_refuses_second_writer_and_keeps_both_later_commits() -> Result<()> {
    for elsewhere in [false, true] {
        let (temp, _) = fixture()?;
        let path = temp.path().join("state.sqlite3");
        let first = crate::store::Store::open(&path)?;
        first.test_locked(|c| { c.execute_batch("CREATE TABLE proof(x); PRAGMA wal_autocheckpoint=0; PRAGMA wal_checkpoint(TRUNCATE); INSERT INTO proof VALUES(42);")?; Ok(()) })?;
        let directory = if elsewhere {
            let p = temp.path().join("other");
            Folder::open_private(&p)?;
            p
        } else {
            temp.path().to_owned()
        };
        let alias = directory.join("alias.sqlite3");
        std::fs::hard_link(&path, &alias)?;
        let error = crate::store::Store::open(&alias)
            .err()
            .context("hardlink writer admitted")?;
        assert!(format!("{error:#}").contains("exactly one link"));
        assert!(!directory.join("alias.sqlite3-wal").exists());
        std::fs::remove_file(alias)?;
        drop(first);
        let second = crate::store::Store::open(&path)?;
        second.test_locked(|c| {
            assert_eq!(
                c.query_row("SELECT sum(x) FROM proof", [], |r| r.get::<_, i64>(0))?,
                42
            );
            c.execute_batch("INSERT INTO proof VALUES(99)")?;
            Ok(())
        })?;
        drop(second);
        let final_store = crate::store::Store::open(&path)?;
        final_store.test_locked(|c| {
            assert_eq!(
                c.query_row("SELECT sum(x) FROM proof", [], |r| r.get::<_, i64>(0))?,
                141
            );
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn mounted_volume_reparse_fixture_is_rejected_without_traversal() -> Result<()> {
    let Some(path) = std::env::var_os("PNEU_TEST_MOUNT_FIXTURE") else {
        eprintln!("Native volume mount fixture not supplied");
        return Ok(());
    };
    let path = PathBuf::from(path);
    assert!(is_link(&Folder::open(&path, false).unwrap_err()));
    assert!(is_link(
        &open_regular_file(&path.join("Windows/win.ini")).unwrap_err()
    ));
    Ok(())
}
