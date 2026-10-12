//! Owner-only local IPC. A bounded one-line JSON request/reply; pairing secrets
//! appear only in direct command replies, never daemon logs or event streams.
use crate::{Config, Core};
use anyhow::{Context, Result, ensure};
use pneu_proto::Id;
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
    ClosePairing,
    Unpair { device: Id },
    Config { config: Config },
    Clipboard { device: Id, text: String },
    DiscardClipboard { device: Id },
    Status,
}
#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub value: serde_json::Value,
}
pub fn bind(path: &Path) -> Result<(UnixListener, std::fs::File)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path.with_extension("lock"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .context("another daemon owns the runtime socket")?;
    if path.exists() {
        let meta = std::fs::symlink_metadata(path)?;
        ensure!(
            meta.file_type().is_socket() && meta.uid() == rustix::process::getuid().as_raw(),
            "unexpected runtime socket owner/type"
        );
        match std::os::unix::net::UnixStream::connect(path) {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(path)?
            }
            _ => bail_existing(path)?,
        }
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, lock))
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
                if tasks.len() < 16 { let core = core.clone(); tasks.spawn(async move { let _ = handle(core,stream).await; }); }
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
    let mut stream = BufReader::new(stream);
    let request: Command = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(10), line(&mut stream)).await??,
    )?;
    let result: Result<serde_json::Value> = async { Ok(match request {
        Command::Pair => { let uri = core.start_pairing().await?; serde_json::json!({"uri":uri.build()?,"expires_at":uri.expires_at}) },
        Command::ClosePairing => { core.close_pairing().await; serde_json::Value::Null },
        Command::Unpair {device} => { core.unpair_device(device).await?; serde_json::Value::Null },
        Command::Config {config} => { core.update_config(config).await?; serde_json::Value::Null },
        Command::Clipboard {device,text} => serde_json::to_value(core.send_clipboard(device,text).await?)?,
        Command::DiscardClipboard {device} => { core.discard_clipboard(device).await?; serde_json::Value::Null },
        Command::Status => serde_json::json!({"device_id":core.identity.device_id,"fingerprint":core.identity.fingerprint,"port":core.bound_port(),"paired_devices":core.paired_devices(),"connected_devices":core.connected_devices(),"paused_devices":core.connected_devices().into_iter().filter(|id| core.phone_paused(id) == Some(true)).collect::<Vec<_>>(),"config":core.config()}),
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
