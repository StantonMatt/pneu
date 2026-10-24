//! Installer deletion by entry ownership, never by receive-path comparisons.
//! Every traversal and unlink is relative to an open no-follow directory.
use anyhow::{Context, Result, ensure};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use std::{ffi::OsStr, fs::File, os::unix::ffi::OsStrExt, path::Path};

#[derive(Clone, Copy)]
enum Layout {
    State,
    Cache,
    Runtime,
    Outbox,
    Viewers,
    Lease,
    Material,
    Bundle,
    Contents,
    MacOS,
    Resources,
    Menubar,
    Signature,
}
enum Entry {
    File,
    Directory(Layout),
}
fn hex32(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit())
}
fn owned(layout: Layout, name: &str) -> Option<Entry> {
    use Entry::{Directory as D, File as F};
    use Layout::*;
    match (layout, name) {
        (
            State,
            "state.sqlite3"
            | "state.sqlite3-wal"
            | "state.sqlite3-shm"
            | "state.sqlite3-journal"
            | "state.lock"
            | "identity.json"
            | "clipboard-fingerprint.key"
            | "config.toml",
        ) => Some(F),
        (State, "run") => Some(D(Runtime)),
        (State, "outbox") => Some(D(Outbox)),
        (Cache, "ui-viewers") => Some(D(Viewers)),
        (Cache, n)
            if n.strip_prefix(crate::storage::CACHE_PREFIX)
                .is_some_and(|n| n.split_once('-').is_some_and(|(id, _)| hex32(id))) =>
        {
            Some(F)
        }
        (Runtime, "pneud.sock" | "pneud.lock" | "pneud.log" | "daemon-session.plist") => Some(F),
        (Outbox, n) if hex32(n) => Some(F),
        (Viewers, n) if hex32(n) => Some(D(Lease)),
        (Lease, ".lease") => Some(F),
        (Lease, n) if hex32(n) => Some(D(Material)),
        // Material names are the sanitized received basename, inside two random
        // 128-bit directory names and a marked viewer lease, never receive dirs.
        (Material, _) => Some(F),
        (Bundle, "Contents") => Some(D(Contents)),
        (Contents, "Info.plist") => Some(F),
        (Contents, "MacOS") => Some(D(MacOS)),
        (Contents, "Resources") => Some(D(Resources)),
        (Contents, "_CodeSignature") => Some(D(Signature)),
        (MacOS, "pneud") => Some(F),
        (Resources, "AppIcon.icns" | "daemon.plist.in") => Some(F),
        (Resources, "menubar") => Some(D(Menubar)),
        (Signature, "CodeResources") => Some(F),
        (Menubar, n)
            if ["connected", "disconnected", "error", "paused", "receiving"]
                .iter()
                .any(|state| {
                    n == format!("pneu-tray-{state}.png")
                        || n == format!("pneu-tray-{state}@2x.png")
                }) =>
        {
            Some(F)
        }
        _ => None,
    }
}
fn directory(parent: &File, name: &OsStr) -> Result<File> {
    Ok(File::from(rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
fn walk(parent: &File, name: &OsStr, layout: Layout, check: bool) -> Result<bool> {
    let stat = match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        if !check {
            rustix::fs::unlinkat(parent, name, AtFlags::empty())?;
        }
        return Ok(true); // Unlink a symlink itself; never descend into its target.
    }
    let dir = directory(parent, name)?;
    // Do not treat an arbitrary hex-named directory as a viewer lease.
    if matches!(layout, Layout::Lease) {
        let marker = rustix::fs::statat(&dir, ".lease", AtFlags::SYMLINK_NOFOLLOW);
        if !matches!(marker, Ok(s) if FileType::from_raw_mode(s.st_mode) == FileType::RegularFile) {
            return Ok(false);
        }
    }
    let mut empty = true;
    for entry in rustix::fs::Dir::read_from(&dir)? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if matches!(bytes, b"." | b"..") {
            continue;
        }
        let name = OsStr::from_bytes(bytes);
        match name.to_str().and_then(|name| owned(layout, name)) {
            Some(Entry::Directory(layout)) => {
                empty &= walk(&dir, name, layout, check)?;
            }
            Some(Entry::File) => {
                let stat = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
                if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
                    empty = false;
                } else if !check {
                    rustix::fs::unlinkat(&dir, name, AtFlags::empty())?;
                }
            }
            None => {
                empty = false;
            }
        }
    }
    if !check {
        // rmdir preserves concurrent additions and unknown content.
        match rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR) {
            Ok(()) => {}
            Err(rustix::io::Errno::NOTEMPTY | rustix::io::Errno::EXIST) => empty = false,
            Err(error) => return Err(error.into()),
        }
        if !empty {
            eprintln!("Kept {}: contains other files", name.to_string_lossy());
        }
    }
    Ok(empty)
}
fn remove(path: &Path, layout: Layout, check: bool) -> Result<bool> {
    ensure!(path.is_absolute(), "removal path must be absolute");
    let parent = path.parent().context("removal parent")?;
    // Folder::open rejects symlinks in every ancestor; it never creates here.
    let parent = match crate::storage::Folder::open(parent, false) {
        Ok(parent) => parent,
        Err(_) if !parent.exists() => return Ok(true),
        Err(error) => return Err(error).context("cannot safely open removal parent"),
    };
    walk(
        &parent.file,
        path.file_name().context("removal filename")?,
        layout,
        check,
    )
}
pub fn cli(args: &[String]) -> Result<()> {
    ensure!(
        args.len() == 2,
        "expected uninstall-owned check-bundle|bundle|purge PATH"
    );
    let path = Path::new(&args[1]);
    match args[0].as_str() {
        "check-bundle" => ensure!(
            remove(path, Layout::Bundle, true)?,
            "bundle contains other files; move them before upgrading; bundle retained"
        ),
        "bundle" => {
            remove(path, Layout::Bundle, false)?;
        }
        "purge" => {
            remove(
                &path.join("Library/Application Support/Pneu"),
                Layout::State,
                false,
            )?;
            remove(&path.join("Library/Caches/Pneu"), Layout::Cache, false)?;
        }
        _ => anyhow::bail!("unknown removal operation"),
    }
    Ok(())
}
