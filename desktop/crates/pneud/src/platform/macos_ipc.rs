//! Darwin has neither bindat/connectat nor Linux's /proc fd paths.
use super::*;
use rustix::fs::{AtFlags, Mode, OFlags};
use std::{ffi::OsStr, os::unix::fs::MetadataExt};

fn private_runtime(folder: &crate::storage::Folder) -> Result<()> {
    crate::platform::private::check(&folder.file)?;
    let meta = folder.file.metadata()?;
    ensure!(
        meta.uid() == rustix::process::getuid().as_raw() && meta.mode() & 0o777 == 0o700,
        "runtime directory must be owned by this uid and mode 0700"
    );
    Ok(())
}
fn verify_parent(folder: &crate::storage::Folder, parent: &Path) -> Result<()> {
    safe_ancestors(parent)?;
    let named = crate::storage::Folder::open(parent, false)?;
    private_runtime(&named)?;
    ensure!(
        folder.identity()? == named.identity()?,
        "runtime directory was replaced"
    );
    Ok(())
}
// Sticky shared parents cannot rename this uid's children. Validate every
// ancestor through retained no-follow descriptors before absolute bind/probe.
fn safe_ancestors(path: &Path) -> Result<()> {
    let mut file = std::fs::File::open("/")?;
    for component in path.components() {
        check_ancestor(&file)?;
        match component {
            std::path::Component::RootDir => continue,
            std::path::Component::Normal(name) => {
                file = std::fs::File::from(rustix::fs::openat(
                    &file,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
            }
            _ => anyhow::bail!("unsafe runtime ancestor"),
        }
    }
    check_ancestor(&file)
}
fn check_ancestor(file: &std::fs::File) -> Result<()> {
    let meta = file.metadata()?;
    ensure!(
        crate::platform::private::rename_safe(
            meta.uid(),
            meta.mode(),
            rustix::process::geteuid().as_raw()
        ),
        "runtime ancestor permits foreign rename"
    );
    crate::platform::private::darwin::check(file, true)
}
pub(super) fn remove_socket(
    folder: &crate::storage::Folder,
    name: &OsStr,
    own: &rustix::fs::Stat,
) -> Result<()> {
    match rustix::fs::statat(&*folder.file, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) if (own.st_dev, own.st_ino) == (named.st_dev, named.st_ino) => {
            crate::platform::fs::unlink_file(&folder.file, name)
        }
        Ok(_) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
pub fn bind(path: &Path) -> Result<(UnixListener, SocketOwner)> {
    ensure!(
        path.is_absolute() && path.as_os_str().as_encoded_bytes().len() < 104,
        "invalid/overlong macOS socket path"
    );
    let parent = path.parent().context("socket parent")?;
    safe_ancestors(parent)?;
    let folder = crate::storage::Folder::open(parent, false)?;
    private_runtime(&folder)?;
    let name = path.file_name().context("socket name")?;
    let lock_path = path.with_extension("lock");
    let lock_name = lock_path.file_name().context("lock name")?;
    let lock = match rustix::fs::openat(
        &*folder.file,
        lock_name,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    ) {
        Ok(fd) => {
            let file = std::fs::File::from(fd);
            crate::platform::private::created(&file)?;
            file
        }
        Err(rustix::io::Errno::EXIST) => {
            let file = std::fs::File::from(rustix::fs::openat(
                &*folder.file,
                lock_name,
                OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            crate::platform::private::check(&file)?;
            file
        }
        Err(e) => return Err(e.into()),
    };
    let meta = lock.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == rustix::process::getuid().as_raw()
            && meta.mode() & 0o077 == 0,
        "unsafe runtime lock"
    );
    lock.try_lock()
        .map_err(crate::platform::fs::lock_error)
        .context("another daemon owns the runtime socket")?;
    verify_parent(&folder, parent)?;
    match rustix::fs::statat(&*folder.file, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(socket) => {
            ensure!(
                socket.st_mode & libc::S_IFMT == libc::S_IFSOCK
                    && socket.st_uid == rustix::process::getuid().as_raw(),
                "unexpected runtime socket owner/type"
            );
            match std::os::unix::net::UnixStream::connect(path) {
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    verify_parent(&folder, parent)?;
                    remove_socket(&folder, name, &socket)?;
                }
                _ => bail_existing(path)?,
            }
        }
        Err(rustix::io::Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    verify_parent(&folder, parent)?;
    // Residual race: a same-uid process (or root) able to rename a parent can
    // redirect bind/probe between these checks. Validated ancestor ownership,
    // modes and ACLs exclude foreign-uid renames. Darwin has no descriptor-relative
    // bind. Reopen without following links and compare dev/ino after binding;
    // cleanup always uses the retained directory and exact socket identity.
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    verify_parent(&folder, parent)?;
    let socket = rustix::fs::statat(&*folder.file, name, AtFlags::SYMLINK_NOFOLLOW)?;
    ensure!(
        socket.st_mode & libc::S_IFMT == libc::S_IFSOCK
            && socket.st_uid == rustix::process::getuid().as_raw(),
        "socket entry changed during bind"
    );
    // Descriptor-relative, and never follows a substituted symlink.
    rustix::fs::chmodat(
        &*folder.file,
        name,
        Mode::from_raw_mode(0o600),
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    verify_parent(&folder, parent)?;
    let after = rustix::fs::statat(&*folder.file, name, AtFlags::SYMLINK_NOFOLLOW)?;
    ensure!(
        (socket.st_dev, socket.st_ino) == (after.st_dev, after.st_ino)
            && after.st_mode & 0o777 == 0o600,
        "socket entry changed during chmod"
    );
    listener.set_nonblocking(true)?;
    // Darwin unp_bind retains unp_vnode with vnode_ref until unp_detach.
    // Keep the bound listener alive through SocketOwner::drop so the created
    // socket's inode cannot be recycled before the identity-checked unlink.
    let pin = listener.try_clone()?;
    Ok((
        UnixListener::from_std(listener)?,
        SocketOwner {
            _lock: lock,
            folder,
            name: name.to_owned(),
            socket,
            _socket_pin: pin,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_leaf_under_foreign_writable_ancestor_is_rejected() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let shared = root.join("shared");
        let runtime = shared.join("run");
        crate::config::private_dir(&runtime)?;
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777))?;
        assert!(bind(&runtime.join("p.sock")).is_err());
        Ok(())
    }
    #[test]
    fn acl_rename_grant_on_ancestor_is_rejected_even_with_private_mode() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let runtime = root.join("run");
        crate::config::private_dir(&runtime)?;
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "group:everyone allow delete_child,add_subdirectory"])
            .arg(&root)
            .status()?;
        ensure!(status.success(), "cannot create ACL ancestor fixture");
        assert!(bind(&runtime.join("p.sock")).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn ipc_requires_private_directory_and_checks_peer_uid() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let parent = std::fs::canonicalize(temp.path())?;
        let path = parent.join("p.sock");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))?;
        assert!(bind(&path).is_err());
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
        let (listener, owner) = bind(&path)?;
        let client = UnixStream::connect(&path).await?;
        let (accepted, _) = listener.accept().await?;
        assert_eq!(
            accepted.peer_cred()?.uid(),
            rustix::process::getuid().as_raw()
        );
        assert_eq!(
            client.peer_cred()?.uid(),
            rustix::process::getuid().as_raw()
        );
        assert_eq!(std::fs::metadata(&path)?.mode() & 0o777, 0o600);
        drop(client);
        drop(accepted);
        drop(listener);
        drop(owner);
        assert!(!path.exists());
        Ok(())
    }
    #[tokio::test]
    async fn ipc_rejects_symlink_parent_and_symlink_lock() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let runtime = root.join("run");
        crate::config::private_dir(&runtime)?;
        std::os::unix::fs::symlink(&runtime, root.join("link"))?;
        assert!(bind(&root.join("link/p.sock")).is_err());
        let foreign = root.join("foreign");
        std::fs::write(&foreign, b"untouched")?;
        std::os::unix::fs::symlink(&foreign, runtime.join("p.lock"))?;
        assert!(bind(&runtime.join("p.sock")).is_err());
        assert_eq!(std::fs::read(foreign)?, b"untouched");
        Ok(())
    }
}

#[cfg(test)]
mod ui_capability_tests {
    use super::*;
    #[tokio::test]
    async fn ui_requests_report_unavailable_without_queuing_a_picker() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let paths = crate::Paths {
            data: root.join("data"),
            config: root.join("config/config.toml"),
            cache: root.join("cache"),
            socket: root.join("run/p.sock"),
        };
        let config = crate::Config {
            port: 17833,
            save_path: root.join("saved"),
            choose_devices_each_time: true,
            ..crate::Config::default()
        };
        let actions = Arc::new(crate::actions::DesktopActions::new(paths.cache.clone()));
        let core = crate::Core::new(paths.clone(), config, actions).await?;
        let (listener, _owner) = bind(&paths.socket)?;
        let (stop, rx) = tokio::sync::watch::channel(false);
        let serving = tokio::spawn(serve(core.clone(), listener, rx));
        for command in [
            Command::ShowWindow {
                choose_folder: false,
            },
            Command::ShowWindow {
                choose_folder: true,
            },
            Command::AddDevice,
            Command::PresentFiles {
                paths: vec![root.join("file")],
                targets: crate::outbox::Targets::All("all".into()),
                request_id: None,
            },
            Command::SendFiles {
                paths: vec![root.join("file")],
                targets: crate::outbox::Targets::All("all".into()),
                request_id: None,
            },
            Command::SendClipboard {
                notify_too_large: false,
                targets: crate::outbox::Targets::All("all".into()),
            },
        ] {
            let error = request(&paths.socket, command).await.unwrap_err();
            assert!(
                error.to_string().contains("menu bar is unavailable"),
                "{error}"
            );
        }
        let status = request(&paths.socket, Command::UiStatus).await?;
        assert!(status["pending_send"].is_null());
        // A ready child enables the same pending-send handoff on M2. Simulate
        // readiness only for this test-owned Core; no AppKit or pasteboard I/O.
        core.ui_ready
            .lock()
            .unwrap()
            .store(true, std::sync::atomic::Ordering::Release);
        request(
            &paths.socket,
            Command::ShowWindow {
                choose_folder: false,
            },
        )
        .await?;
        let recipient = crate::outbox::Authority {
            device: pneu_proto::Id::from_bytes([42; 16]),
            generation: 7,
            fingerprint: pneu_proto::Hash::new("a".repeat(64))?,
        };
        let reply = request(
            &paths.socket,
            Command::PresentPicker {
                paths: None,
                targets: vec![recipient.clone()],
                clipboard: false,
                targeted: true,
            },
        )
        .await?;
        let status = request(&paths.socket, Command::UiStatus).await?;
        assert_eq!(status["pending_send"]["id"], reply["request"]);
        assert_eq!(
            status["pending_send"]["targets"][0],
            serde_json::to_value(recipient)?
        );
        assert!(status["pending_send"]["paths"].is_null());
        core.ui_ready
            .lock()
            .unwrap()
            .store(false, std::sync::atomic::Ordering::Release);
        assert!(request(&paths.socket, Command::AddDevice).await.is_err());
        assert_eq!(
            request(&paths.socket, Command::UiStatus).await?["window_request"],
            status["window_request"]
        );
        stop.send_replace(true);
        serving.await??;
        core.shutdown_actions().await?;
        Ok(())
    }
}
