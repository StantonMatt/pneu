//! Owner-only local IPC. A bounded one-line JSON request/reply; pairing secrets
//! appear only in direct command replies, never daemon logs or event streams.
use crate::{Config, Core};
use anyhow::{Context, Result, ensure};
use pneu_proto::{Hash, Id};
use serde::{Deserialize, Serialize};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    task::JoinSet,
};
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Pair,
    EnsurePair,
    WaitPairingChange { revision: u64 },
    ClosePairing,
    ClosePairingIf { token: Hash },
    Unpair { device: Id },
    Config { config: Config },
    SaveFolder { folder: std::path::PathBuf },
    Clipboard { device: Id, text: String },
    DiscardClipboard { device: Id },
    Status,
    UiStatus,
    ShowWindow { choose_folder: bool },
}
#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub value: serde_json::Value,
}
/// Holds the instance lock and the exact socket entry created by this daemon.
/// Shutdown must not unlink a replacement socket/file or a replaced parent.
pub struct SocketOwner {
    _lock: std::fs::File,
    folder: crate::storage::Folder,
    name: std::ffi::OsString,
    socket: std::fs::File,
}
impl Drop for SocketOwner {
    fn drop(&mut self) {
        let _ = crate::storage::remove_file(&self.folder.file, &self.name, &self.socket);
    }
}
pub fn bind(path: &Path) -> Result<(UnixListener, SocketOwner)> {
    use rustix::fs::{Mode, OFlags};
    use std::os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt},
    };
    let folder = crate::storage::Folder::open(path.parent().context("socket parent")?, false)?;
    let name = path.file_name().context("socket name")?;
    let lock_path = path.with_extension("lock");
    let lock = std::fs::File::from(rustix::fs::openat(
        &*folder.file,
        lock_path.file_name().context("lock name")?,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .context("another daemon owns the runtime socket")?;
    // Linux has no bindat; /proc anchors both bind and stale-socket probing to
    // the retained parent instead of resolving a mutable directory pathname.
    let anchored =
        std::path::PathBuf::from(format!("/proc/self/fd/{}", folder.file.as_raw_fd())).join(name);
    match rustix::fs::openat(
        &*folder.file,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let socket = std::fs::File::from(fd);
            let meta = socket.metadata()?;
            ensure!(
                meta.file_type().is_socket() && meta.uid() == rustix::process::getuid().as_raw(),
                "unexpected runtime socket owner/type"
            );
            match std::os::unix::net::UnixStream::connect(&anchored) {
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    crate::storage::remove_file(&folder.file, name, &socket)?;
                }
                _ => bail_existing(path)?,
            }
        }
        Err(rustix::io::Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&anchored)?;
    let socket = std::fs::File::from(rustix::fs::openat(
        &*folder.file,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    std::fs::set_permissions(&anchored, std::fs::Permissions::from_mode(0o600))?;
    Ok((
        listener,
        SocketOwner {
            _lock: lock,
            folder,
            name: name.to_owned(),
            socket,
        },
    ))
}
fn bail_existing(path: &Path) -> Result<()> {
    anyhow::bail!(
        "{} already exists; another daemon may own it (remove only after proving it stale)",
        path.display()
    )
}
pub async fn serve(
    core: Arc<Core>,
    listener: UnixListener,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    loop {
        if *stop.borrow() {
            break;
        }
        tokio::select! {
            _ = stop.changed() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept() => {
                let (stream,_) = accepted?;
                ensure!(stream.peer_cred()?.uid() == rustix::process::getuid().as_raw(),"foreign IPC owner");
                if tasks.len() < 16 { let core = core.clone(); tasks.spawn(Box::pin(async move { let _ = handle(core,stream).await; })); }
            }
        }
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}
async fn line(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        ensure!(!buf.is_empty(), "IPC EOF");
        let n = buf
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| n + 1)
            .unwrap_or(buf.len());
        ensure!(bytes.len() + n <= 65536, "IPC request too large");
        let ended = buf[n - 1] == b'\n';
        bytes.extend_from_slice(&buf[..n]);
        reader.consume(n);
        if ended {
            return Ok(bytes);
        }
    }
}
async fn handle(core: Arc<Core>, stream: UnixStream) -> Result<()> {
    Box::pin(async move {

    let mut stream = BufReader::new(stream);
    let request: Command = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(10), line(&mut stream)).await??,
    )?;
    let result: Result<serde_json::Value> = async { Ok(match request {
        Command::Pair => { let uri = core.start_pairing().await?; serde_json::json!({"uri":uri.build()?,"expires_at":uri.expires_at}) },
        Command::EnsurePair => { let uri = core.ensure_pairing().await?; serde_json::to_value(uri)? },
        Command::WaitPairingChange {revision} => serde_json::json!({"revision":core.wait_pairing_change(revision).await}),
        Command::ClosePairingIf { token } => { core.close_pairing_if(token).await; serde_json::Value::Null },
        Command::ClosePairing => { core.close_pairing().await; serde_json::Value::Null },
        Command::Unpair {device} => { core.unpair_device(device).await?; serde_json::Value::Null },
        Command::SaveFolder {folder} => { core.change_save_folder(folder).await?; serde_json::Value::Null },
        Command::Config {config} => { core.update_config(config).await?; serde_json::Value::Null },
        Command::Clipboard {device,text} => serde_json::to_value(core.send_clipboard(device,text).await?)?,
        Command::DiscardClipboard {device} => { core.discard_clipboard(device).await?; serde_json::Value::Null },
        Command::UiStatus => { let (revision, invitation) = core.pairing_status(); let snapshot = core.ui_snapshot(); let (window_request, activation_token) = core.window_request_with_activation(); serde_json::json!({"device":snapshot.device, "snapshot":snapshot, "config": core.config(), "window_request":window_request,"activation_token":activation_token,"invitation_revision":revision,"invitation":invitation}) },
        Command::ShowWindow { choose_folder } => { core.request_window(choose_folder); serde_json::Value::Null },
        Command::Status => {
            let snapshot = core.ui_snapshot();
            let paired: Vec<_> = snapshot.device.iter().cloned().collect();
            let connected: Vec<_> = snapshot.device.iter().filter(|_| snapshot.connected).cloned().collect();
            let paused: Vec<_> = snapshot.device.iter().filter(|_| snapshot.connected && snapshot.paused).cloned().collect();
            serde_json::json!({"device_id":core.identity.device_id,"fingerprint":core.identity.fingerprint,"port":core.bound_port(),"paired_devices":paired,"connected_devices":connected,"paused_devices":paused,"config":core.config()})
        },
    }) }.await;
    let reply = match result {
        Ok(value) => Reply { ok: true, value },
        Err(e) => Reply {
            ok: false,
            value: serde_json::Value::String(e.to_string()),
        },
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        stream
            .get_mut()
            .write_all(&serde_json::to_vec(&reply)?)
            .await?;
        stream.get_mut().write_all(b"\n").await?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())

        }).await
}
pub async fn request(path: &Path, command: Command) -> Result<serde_json::Value> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut stream = UnixStream::connect(path)
            .await
            .context("connect to running pneud")?;
        stream.write_all(&serde_json::to_vec(&command)?).await?;
        stream.write_all(b"\n").await?;
        let reply: Reply = serde_json::from_slice(&line(&mut BufReader::new(stream)).await?)?;
        ensure!(reply.ok, "daemon command failed: {}", reply.value);
        Ok(reply.value)
    })
    .await?
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn socket_owner_deletes_only_its_original_entry_and_parent() -> Result<()> {
        for replacement in ["none", "file", "symlink", "socket", "parent"] {
            let dir = tempfile::tempdir()?;
            let parent = dir.path().join("runtime");
            std::fs::create_dir(&parent)?;
            let path = parent.join("pneu.sock");
            let (listener, owner) = bind(&path)?;
            // Binding through /proc still accepts clients via the canonical path.
            let client = UnixStream::connect(&path).await?;
            let _accepted = listener.accept().await?;
            drop(client);
            assert!(bind(&path).is_err());
            let foreign = dir.path().join("foreign");
            std::fs::write(&foreign, b"foreign")?;
            let mut replacement_socket = None;
            match replacement {
                "file" | "symlink" | "socket" => {
                    std::fs::remove_file(&path)?;
                    match replacement {
                        "file" => std::fs::write(&path, b"foreign")?,
                        "symlink" => std::os::unix::fs::symlink(&foreign, &path)?,
                        "socket" => replacement_socket = Some(UnixListener::bind(&path)?),
                        _ => unreachable!(),
                    }
                }
                "parent" => {
                    std::fs::rename(&parent, dir.path().join("moved"))?;
                    std::fs::create_dir(&parent)?;
                    std::fs::write(&path, b"foreign")?;
                }
                _ => {}
            }
            drop(owner);
            drop(listener);
            match replacement {
                "none" => assert!(!path.exists()),
                "socket" => assert!(std::os::unix::fs::FileTypeExt::is_socket(
                    &std::fs::symlink_metadata(&path)?.file_type()
                )),
                "symlink" => assert!(std::fs::symlink_metadata(&path)?.file_type().is_symlink()),
                "parent" => {
                    assert_eq!(std::fs::read(&path)?, b"foreign");
                    assert!(!dir.path().join("moved/pneu.sock").exists());
                }
                "file" => assert_eq!(std::fs::read(&path)?, b"foreign"),
                _ => unreachable!(),
            }
            assert_eq!(std::fs::read(foreign)?, b"foreign");
            drop(replacement_socket);
        }
        Ok(())
    }
    #[tokio::test]
    async fn binding_reclaims_stale_owned_socket_but_preserves_foreign_entries() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("pneu.sock");
        drop(std::os::unix::net::UnixListener::bind(&path)?);
        let (listener, owner) = bind(&path)?;
        drop(listener);
        drop(owner);
        assert!(!path.exists());
        std::fs::write(&path, b"foreign")?;
        assert!(bind(&path).is_err());
        assert_eq!(std::fs::read(&path)?, b"foreign");
        std::fs::remove_file(&path)?;
        let foreign = dir.path().join("foreign");
        std::fs::write(&foreign, b"foreign")?;
        std::os::unix::fs::symlink(&foreign, &path)?;
        assert!(bind(&path).is_err());
        assert!(std::fs::symlink_metadata(path)?.file_type().is_symlink());
        assert_eq!(std::fs::read(foreign)?, b"foreign");
        Ok(())
    }
}
