//! Owner-only local IPC. A bounded one-line JSON request/reply; pairing secrets
//! appear only in direct command replies, never daemon logs or event streams.
use crate::{Config, Core};
use anyhow::{Context, Result, ensure};
use pneu_proto::{Hash, Id};
use serde::{Deserialize, Serialize};
#[cfg(all(unix, any(target_os = "linux", test)))]
use std::os::unix::fs::PermissionsExt;
use std::{path::Path, sync::Arc, time::Duration};
// 128 PATH_MAX (4096-byte) paths, up to six JSON bytes per path byte,
// plus recipient authorities and framing, fit below 4 MiB. This also covers
// clipboard text (16 KiB), invitations (8 KiB), and config/path commands.
pub const REQUEST_LIMIT: usize = 4 * 1024 * 1024;
pub const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;
const RESPONSE_PAGE_BYTES: usize = 1024 * 1024;

#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    task::JoinSet,
};
#[cfg(unix)]
type Listener = UnixListener;
#[cfg(unix)]
type ServerStream = UnixStream;
#[cfg(windows)]
#[path = "platform/windows_ipc.rs"]
mod windows;
#[cfg(windows)]
use windows::Connection as ServerStream;
#[cfg(windows)]
pub(crate) use windows::pipe_name as windows_pipe_name;
#[cfg(windows)]
pub use windows::{Listener, SocketOwner, bind};
#[cfg(unix)]
async fn accept(listener: &mut Listener) -> Result<ServerStream> {
    Ok(listener.accept().await?.0)
}
#[cfg(windows)]
use windows::{accept, connect};
#[cfg(unix)]
async fn connect(path: &Path) -> Result<UnixStream> {
    Ok(UnixStream::connect(path).await?)
}
#[cfg(unix)]
fn check_client(stream: &ServerStream) -> Result<()> {
    ensure!(
        stream.peer_cred()?.uid() == rustix::process::getuid().as_raw(),
        "foreign IPC owner"
    );
    Ok(())
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    ShowClipboardHistory,
    CopyClipboardHistory {
        id: String,
    },
    DeleteClipboardHistory {
        id: String,
    },
    ClearClipboardHistory,
    DisableEmptyClipboardHistory,
    SendFiles {
        paths: Vec<std::path::PathBuf>,
        targets: crate::outbox::Targets,
        #[serde(default)]
        request_id: Option<String>,
    },
    /// UI entry point: choose recipients before admission, then show Recent.
    PresentFiles {
        paths: Vec<std::path::PathBuf>,
        targets: crate::outbox::Targets,
        request_id: Option<String>,
    },
    #[cfg(target_os = "macos")]
    UiTest {
        action: String,
    },
    #[cfg(any(target_os = "macos", windows))]
    PresentPicker {
        #[serde(default)]
        paths: Option<Vec<std::path::PathBuf>>,
        targets: Vec<crate::outbox::Authority>,
        clipboard: bool,
        targeted: bool,
    },
    PresentSendError {
        message: String,
    },
    AcknowledgeSend {
        request: String,
    },
    OperationStatus {
        operation: Id,
    },
    DismissTransfer {
        operation: Id,
        #[serde(default)]
        artifact: Option<Id>,
    },
    CancelTransfer {
        operation: Id,
        #[serde(default)]
        artifact: Option<Id>,
        device: Option<Id>,
    },
    RetryTransfer {
        operation: Id,
        #[serde(default)]
        artifact: Option<Id>,
        device: Id,
    },
    SetSetting {
        key: String,
        value: serde_json::Value,
    },
    SendClipboard {
        targets: crate::outbox::Targets,
        #[serde(default)]
        notify_too_large: bool,
    },
    Rename {
        name: String,
    },
    Pair,
    Join {
        uri: String,
    },
    JoinConfirm {
        attempt: String,
    },
    JoinConfirmScoped {
        attempt: String,
        enabled: bool,
    },
    PairingScope {
        enabled: bool,
    },
    SetupStatus,
    LinkExistingDevice {
        device: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding_generation: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_scope: Option<pneu_proto::SetupScope>,
    },
    SetupDecision {
        device: Id,
        setup_generation: u64,
        binding_generation: u64,
        allow: bool,
    },
    SetupDecisions {
        requests: Vec<crate::ui::window_setup::Decision>,
        allow: bool,
    },
    ReadmissionDecision {
        key: crate::store::members::ReadmissionKey,
        allow: bool,
    },
    RemoveFromSetup {
        device: Id,
        setup_generation: u64,
        binding_generation: Option<u64>,
        fingerprint: Hash,
    },
    DismissSetupNotice {
        device: Id,
        binding_generation: u64,
    },
    JoinStatus {
        attempt: String,
    },
    JoinCancel {
        attempt: String,
    },
    EnsurePair,
    WaitPairingChange {
        revision: u64,
    },
    ClosePairing,
    ClosePairingIf {
        token: Hash,
    },
    Unpair {
        device: Id,
    },
    Config {
        config: Config,
    },
    #[cfg(any(target_os = "linux", windows))]
    OpenFolder,
    SaveFolder {
        folder: std::path::PathBuf,
    },
    Clipboard {
        device: Id,
        text: String,
    },
    DiscardClipboard {
        device: Id,
    },
    PeerSettings {
        device: Id,
    },
    SetPeerSetting {
        device: Id,
        key: String,
        enabled: bool,
    },
    Status,
    UiStatus,
    #[cfg(windows)]
    WindowClosed {
        request: u64,
    },
    WindowFocus {
        request: u64,
        focused: bool,
    },
    AddDevice,
    ShowWindow {
        choose_folder: bool,
    },
}
impl Command {
    fn check_desktop(
        &self,
        choose_devices: bool,
        require_ui: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        if self.requires_desktop(choose_devices) {
            require_ui().map_err(|error| match self {
                // A headless runtime cannot present the error in a window.
                // Return it over IPC so the CLI can still show the cause.
                Self::PresentSendError { message } => {
                    if message.len() > 4096 {
                        anyhow::anyhow!("send error is too long")
                    } else {
                        anyhow::anyhow!("{message}")
                    }
                }
                _ => error,
            })?;
        }
        Ok(())
    }

    fn requires_desktop(&self, choose_devices: bool) -> bool {
        #[cfg(any(target_os = "linux", windows))]
        if matches!(self, Self::OpenFolder) {
            return true;
        }
        #[cfg(target_os = "macos")]
        if matches!(self, Self::UiTest { .. }) {
            return true;
        }
        #[cfg(any(target_os = "macos", windows))]
        if matches!(self, Self::PresentPicker { .. }) {
            return true;
        }
        matches!(
            self,
            Self::ShowClipboardHistory
                | Self::AddDevice
                | Self::ShowWindow { .. }
                | Self::PresentFiles { .. }
                | Self::PresentSendError { .. }
        ) || (choose_devices
            && matches!(
                self,
                Self::SendFiles {
                    targets: crate::outbox::Targets::All(_),
                    ..
                } | Self::SendClipboard {
                    targets: crate::outbox::Targets::All(_),
                    ..
                }
            ))
    }
}
#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub value: serde_json::Value,
    /// More array projection pages follow on this same connection. A missing
    /// flag is the original one-line protocol. Only the last page completes it.
    #[serde(default)]
    pub more: bool,
}
/// Holds the instance lock and the exact socket entry created by this daemon.
/// Shutdown must not unlink a replacement socket/file or a replaced parent.
#[cfg(unix)]
pub struct SocketOwner {
    _lock: std::fs::File,
    folder: crate::storage::Folder,
    name: std::ffi::OsString,
    #[cfg(target_os = "linux")]
    socket: std::fs::File,
    #[cfg(target_os = "macos")]
    socket: rustix::fs::Stat,
    #[cfg(target_os = "macos")]
    _socket_pin: std::os::unix::net::UnixListener,
}
#[cfg(unix)]
impl Drop for SocketOwner {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        let _ = crate::storage::remove_file(&self.folder.file, &self.name, &self.socket);
        #[cfg(target_os = "macos")]
        let _ = macos::remove_socket(&self.folder, &self.name, &self.socket);
    }
}
#[cfg(target_os = "macos")]
#[path = "platform/macos_ipc.rs"]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::bind;
#[cfg(target_os = "linux")]
pub fn bind(path: &Path) -> Result<(UnixListener, SocketOwner)> {
    use rustix::fs::{Mode, OFlags};
    use std::os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt},
    };
    let folder = crate::storage::Folder::open(path.parent().context("socket parent")?, false)?;
    let name = path.file_name().context("socket name")?;
    let lock_path = path.with_extension("lock");
    let lock = crate::platform::fs::open_lock_at(
        &folder.file,
        lock_path.file_name().context("lock name")?,
    )?;
    lock.try_lock()
        .map_err(crate::platform::fs::lock_error)
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
#[cfg(unix)]
fn bail_existing(path: &Path) -> Result<()> {
    anyhow::bail!(
        "{} already exists; another daemon may own it (remove only after proving it stale)",
        path.display()
    )
}
pub async fn serve(
    core: Arc<Core>,
    mut listener: Listener,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    let result = loop {
        if *stop.borrow() {
            break Ok(());
        }
        tokio::select! {
            _ = stop.changed() => break Ok(()),
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = accept(&mut listener) => {
                let stream = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        // Losing the next pipe instance must stop the daemon:
                        // continuing could surrender ownership of its name.
                        #[cfg(windows)]
                        break Err(error);
                        #[cfg(unix)]
                        {
                        eprintln!("pneud: IPC accept failed: {error}");
                        tokio::select! {
                            _ = stop.changed() => break Ok(()),
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                        }
                        continue;
                        }
                    }
                };
                if tasks.len() < 16 { let core = core.clone(); tasks.spawn(Box::pin(async move { let _ = handle(core,stream).await; })); }
            }
        }
    };
    #[cfg(windows)]
    tasks.abort_all(); // Dropping each Connection disconnects pending I/O.
    while tasks.join_next().await.is_some() {}
    result
}
async fn line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    limit: usize,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        ensure!(!buf.is_empty(), "IPC EOF");
        let n = buf
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| n + 1)
            .unwrap_or(buf.len());
        ensure!(bytes.len() + n <= limit, "IPC frame too large");
        let ended = buf[n - 1] == b'\n';
        bytes.extend_from_slice(&buf[..n]);
        reader.consume(n);
        if ended {
            return Ok(bytes);
        }
    }
}
async fn handle(core: Arc<Core>, stream: ServerStream) -> Result<()> {
    #[cfg(unix)]
    check_client(&stream)?;
    #[cfg(windows)]
    let mut connection = stream;
    #[cfg(windows)]
    let stream = &mut connection.0;
    Box::pin(async move {

    let mut stream = BufReader::new(stream);
    let request: Command = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(10), line(&mut stream,REQUEST_LIMIT)).await??,
    )?;
    let ask = core.config().choose_devices_each_time;
    let desktop_send = match &request {
        #[cfg(any(target_os = "macos", windows))]
        Command::PresentPicker {..} => true,
        Command::PresentFiles {..} | Command::PresentSendError {..} => true,
        Command::SendFiles {targets,..} | Command::SendClipboard {targets,..} => !matches!(targets, crate::outbox::Targets::Captured(_)),
        _ => false,
    };
    let result: Result<serde_json::Value> = async {
    request.check_desktop(ask, || crate::ui::runtime::require_ui(&core))?;
    Ok(match request {
        Command::SendFiles{paths,targets,request_id} if ask && matches!(&targets, crate::outbox::Targets::All(_))=>core.present_files(paths,targets,request_id)?,
        Command::SendFiles{paths,targets,request_id}=>{
            if desktop_send { core.request_window(false); }
            core.send_files_request(paths,targets,request_id).await?
        },
        Command::PresentFiles{paths,targets,request_id}=>core.present_files(paths,targets,request_id)?,
        #[cfg(target_os = "macos")]
        Command::UiTest {action} => {
            anyhow::ensure!(std::env::var_os("PNEU_UI_TEST").is_some(),"UI acceptance controls are disabled");
            if action=="notify" || action=="notify-transient" {serde_json::json!({"notification":core.ui_test_notification(action=="notify-transient").await?})}
            else if action=="open-test" {core.ui_test_open().await?;serde_json::Value::Null}
            else {crate::ui::child_bridge::begin(crate::ui::child_bridge::Request::Test(action),None)?.await?;serde_json::Value::Null}
        },
        #[cfg(any(target_os = "macos", windows))]
        Command::PresentPicker {paths,targets,clipboard,targeted} => {
            anyhow::ensure!(paths.as_ref().is_none_or(|paths|!clipboard && !paths.is_empty() && paths.len()<=128),"choose 1 to 128 files");
            core.present_send(crate::ui::window::SendRequest {id:crate::random_hex(16),paths,targets,clipboard,targeted,error:None})?
        },
        Command::PresentSendError{message}=>core.present_send_error(message)?,
        Command::AcknowledgeSend{request}=>{core.acknowledge_send(&request);serde_json::Value::Null},
        Command::OperationStatus{operation}=>core.operation_status(operation).await?,
        Command::DismissTransfer{operation,artifact}=>{core.dismiss_transfer_artifact(operation,artifact).await?;serde_json::Value::Null},
        Command::CancelTransfer{operation,artifact,device}=>{core.cancel_transfer_artifact(operation,artifact,device).await?;serde_json::Value::Null},
        Command::RetryTransfer{operation,artifact,device}=>{core.retry_transfer_artifact(operation,artifact,device).await?;serde_json::Value::Null},
        Command::ShowClipboardHistory=>{core.request_history_window();serde_json::Value::Null},
        Command::CopyClipboardHistory{id}=>{core.copy_clipboard_history(id).await?;serde_json::Value::Null},
        Command::DeleteClipboardHistory{id}=>{core.delete_clipboard_history(Some(id)).await?;serde_json::Value::Null},
        Command::DisableEmptyClipboardHistory=>serde_json::json!(core.disable_empty_clipboard_history().await?),
        Command::ClearClipboardHistory=>{core.delete_clipboard_history(None).await?;serde_json::Value::Null},
        Command::SetSetting{key,value}=>{core.set_setting(key,value).await?;serde_json::Value::Null},
        Command::Rename{name}=>{core.rename(name).await?;serde_json::Value::Null},
        Command::SendClipboard{targets:crate::outbox::Targets::All(_),..} if ask=>core.present_send(crate::ui::window::SendRequest {
            id:crate::random_hex(16),paths:None,targets:crate::ui::captured_authorities(&core.ui_snapshot()),clipboard:true,targeted:false,error:None,
        })?,
        Command::SendClipboard{targets,notify_too_large}=>{
            let result = core.share_clipboard(targets).await;
            if notify_too_large && result.as_ref().is_err_and(|error| error.is::<crate::actions::ImageTooLarge>()) {
                let _ = core.notify_clipboard_too_large().await;
            }
            serde_json::json!({"transfer_id":result?})
        },
        Command::Join { uri } => core.join(&uri).await?,
        Command::JoinConfirm { attempt } => core.confirm_join(&attempt).await?,
        Command::JoinConfirmScoped { attempt, enabled } => core.confirm_join_scoped(&attempt,enabled).await?,
        Command::PairingScope { enabled } => {core.set_pairing_scope(enabled).await?;serde_json::Value::Null},
        Command::SetupStatus => core.setup_status().await?,
        Command::LinkExistingDevice {device,binding_generation,expected_scope} => {
            ensure!(binding_generation.is_some()==expected_scope.is_some(),"incomplete setup link expectation");
            let linked=core.link_existing_device(device,binding_generation.zip(expected_scope)).await?;
            if binding_generation.is_some() {serde_json::json!(linked)} else {serde_json::Value::Null}
        },
        Command::SetupDecisions { requests, allow } => {
            ensure!(requests.len() <= 16, "too many setup decisions");
            crate::ui::window_setup::decide_all(&core, &requests, allow).await?;
            serde_json::Value::Null
        },
        Command::ReadmissionDecision { key, allow } => {core.decide_readmission(key,allow).await?;serde_json::Value::Null},
        Command::SetupDecision {device,setup_generation,binding_generation,allow} => {core.decide_setup(device,setup_generation,binding_generation,allow).await?;serde_json::Value::Null},
        Command::RemoveFromSetup {device,setup_generation,binding_generation,fingerprint} => {core.remove_setup_device_confirmed(device,setup_generation,binding_generation,fingerprint).await?;serde_json::Value::Null},
        Command::DismissSetupNotice {device,binding_generation} => {core.dismiss_setup_notice(device,binding_generation).await?;serde_json::Value::Null},
        Command::JoinStatus { attempt } => core.join_status(&attempt)?,
        Command::JoinCancel { attempt } => { core.cancel_join(&attempt).await?; serde_json::Value::Null },
        Command::Pair => { let uri = core.start_pairing().await?; serde_json::json!({"uri":uri.build()?,"expires_at":uri.expires_at,"qr_data":uri.build()?}) },
        Command::EnsurePair => { let uri = core.ensure_pairing().await?; serde_json::to_value(uri)? },
        Command::WaitPairingChange {revision} => serde_json::json!({"revision":core.wait_pairing_change(revision).await}),
        Command::ClosePairingIf { token } => { core.close_pairing_if(token).await; serde_json::Value::Null },
        Command::ClosePairing => { core.close_pairing().await; serde_json::Value::Null },
        Command::Unpair {device} => { core.unpair_device(device).await?; serde_json::Value::Null },
        #[cfg(target_os = "linux")]
        Command::OpenFolder => { crate::ui::runtime::open_folder(&core.config().save_path).await?; serde_json::Value::Null },
        #[cfg(windows)]
        Command::OpenFolder => { crate::actions::open_folder(&core.config().save_path).await?; serde_json::Value::Null },
        Command::SaveFolder {folder} => { core.change_save_folder(folder).await?; serde_json::Value::Null },
        Command::Config {config} => { core.update_config(config).await?; serde_json::Value::Null },
        Command::Clipboard {device,text} => serde_json::to_value(core.send_clipboard(device,text).await?)?,
        Command::PeerSettings {device} => core.peer_settings(device).await?,
        Command::SetPeerSetting {device,key,enabled} => core.set_peer_setting(device,key,enabled).await?,
        Command::DiscardClipboard {device} => { core.discard_clipboard(device).await?; serde_json::Value::Null },
        #[cfg(windows)]
        Command::WindowClosed { request } => { core.window_closed(request); serde_json::Value::Null },
        Command::WindowFocus { request, focused } => { core.set_window_focus(request, focused); serde_json::Value::Null },
        Command::UiStatus => { let (revision, invitation) = core.pairing_status(); let snapshot = core.ui_snapshot(); let (window_request, activation_token) = core.window_request_with_activation(); serde_json::json!({"device":snapshot.device, "snapshot":snapshot, "config": core.config(), "window_request":window_request,"history_page":core.window_history(window_request),"add_device":core.window_add_device(window_request),"remove_device":core.window_remove_device(window_request),"self_id":core.identity.device_id,"activation_token":activation_token,"invitation_revision":revision,"invitation":invitation,"pending_send":core.pending_send(),"setup":core.setup_status().await?}) },
        Command::AddDevice => { core.request_add_device(); serde_json::Value::Null },
        Command::ShowWindow { choose_folder } => { core.request_window(choose_folder); serde_json::Value::Null },
        Command::Status => {
            let snapshot = core.ui_snapshot();
            let paired = snapshot.paired_devices;
            let connected = snapshot.connected_devices;
            let paused = snapshot.paused_devices;
            serde_json::json!({"device_id":core.identity.device_id,"fingerprint":core.identity.fingerprint,"port":core.bound_port(),"paired_devices":paired,"connected_devices":connected,"paused_devices":paused,"devices":snapshot.devices,"transfers":snapshot.transfers,"send_targets":snapshot.send_targets,"config":core.config()})
        },
    }) }.await;
    if desktop_send && let Err(error) = &result {
        core.notify_send_admission_failure(&error.to_string()).await;
    }
    let reply = match result {
        Ok(value) => Reply { ok: true, value, more: false },
        Err(e) => Reply {
            ok: false,
            value: serde_json::Value::String(e.to_string()),
            more: false,
        },
    };
    let pages = response_pages(reply).unwrap_or_else(|error| vec![Reply {
        ok: false, value: serde_json::Value::String(error.to_string()), more: false,
    }]);
    tokio::time::timeout(Duration::from_secs(10), async {
        for page in pages {
            stream.get_mut().write_all(&serde_json::to_vec(&page)?).await?;
            stream.get_mut().write_all(b"\n").await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    #[cfg(windows)]
    {
        // Mio's write_all can finish with the final write still pending. Keep
        // the handler slot and disconnect guard until the client consumes the
        // reply and closes; bound non-reading clients even for tiny replies.
        let mut byte = [0; 1];
        let _ = tokio::time::timeout(Duration::from_secs(10),
            tokio::io::AsyncReadExt::read(&mut stream, &mut byte)).await;
    }
    Ok(())

        }).await
}
/// Split array projections into <= 1 MiB JSON pages, leaving 1 MiB of
/// headroom below the reader's hard limit. This covers both status forms and
/// operation_status accepted/rejected history, without discarding siblings.
/// Pages belong to one captured reply on one connection; no mutable offsets or
/// retained server-side cursors are needed. Scalar metadata appears only once.
fn response_pages(mut reply: Reply) -> Result<Vec<Reply>> {
    if serde_json::to_vec(&reply)?.len() < RESPONSE_PAGE_BYTES {
        return Ok(vec![reply]);
    }
    fn arrays(
        value: &mut serde_json::Value,
        path: &mut Vec<String>,
        out: &mut Vec<(Vec<String>, Vec<serde_json::Value>)>,
    ) {
        match value {
            serde_json::Value::Array(items) => out.push((path.clone(), std::mem::take(items))),
            serde_json::Value::Object(fields) => {
                for (key, value) in fields {
                    path.push(key.clone());
                    arrays(value, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut projections = Vec::new();
    arrays(&mut reply.value, &mut Vec::new(), &mut projections);
    reply.more = true;
    let mut bytes = serde_json::to_vec(&reply)?.len() + 8;
    ensure!(
        bytes <= RESPONSE_PAGE_BYTES,
        "IPC response metadata is too large"
    );
    let mut pages = Vec::new();
    for (path, items) in projections {
        for item in items {
            // Overestimate path/object/array framing for every row, even where
            // it is already present. Never truncate a pathname or recipient.
            let cost = serde_json::to_vec(&item)?.len()
                + path.iter().map(|p| p.len() * 6 + 8).sum::<usize>()
                + 8;
            ensure!(
                cost + 64 <= RESPONSE_PAGE_BYTES,
                "IPC response item is too large"
            );
            if bytes + cost > RESPONSE_PAGE_BYTES {
                pages.push(reply);
                reply = Reply {
                    ok: true,
                    value: serde_json::Value::Null,
                    more: true,
                };
                bytes = serde_json::to_vec(&reply)?.len() + 8;
            }
            let mut destination = &mut reply.value;
            for key in &path {
                if destination.is_null() {
                    *destination = serde_json::json!({});
                }
                destination = destination
                    .as_object_mut()
                    .context("IPC projection object")?
                    .entry(key.clone())
                    .or_insert(serde_json::Value::Null);
            }
            if destination.is_null() {
                *destination = serde_json::json!([]);
            }
            destination
                .as_array_mut()
                .context("IPC projection array")?
                .push(item);
            bytes += cost;
        }
    }
    reply.more = false;
    pages.push(reply);
    Ok(pages)
}
fn append_page(value: &mut serde_json::Value, page: serde_json::Value) -> Result<()> {
    match (value, page) {
        (serde_json::Value::Array(items), serde_json::Value::Array(mut next)) => {
            items.append(&mut next)
        }
        (serde_json::Value::Object(fields), serde_json::Value::Object(next)) => {
            for (key, value) in next {
                append_page(
                    fields
                        .get_mut(&key)
                        .context("unexpected IPC continuation field")?,
                    value,
                )?;
            }
        }
        _ => anyhow::bail!("invalid IPC continuation"),
    }
    Ok(())
}
pub async fn request(path: &Path, command: Command) -> Result<serde_json::Value> {
    request_connected(command, connect(path)).await
}
#[cfg(unix)]
type ClientStream = UnixStream;
#[cfg(windows)]
type ClientStream = tokio::net::windows::named_pipe::NamedPipeClient;
async fn request_connected(
    command: Command,
    connection: impl std::future::Future<Output = Result<ClientStream>>,
) -> Result<serde_json::Value> {
    let encoded = serde_json::to_vec(&command)?;
    ensure!(
        encoded.len() < REQUEST_LIMIT,
        "IPC request exceeds the 4 MiB limit; choose fewer files"
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        let stream = connection.await.context("connect to running pneud")?;
        #[cfg(windows)]
        if matches!(
            command,
            Command::ShowWindow { .. }
                | Command::OpenFolder
                | Command::PresentFiles { .. }
                | Command::PresentPicker { .. }
                | Command::PresentSendError { .. }
        ) {
            use std::os::windows::io::AsRawHandle;
            // Only after the connection-bound owner/DACL/session checks: pass
            // the Shell launcher's foreground rights to the existing daemon.
            let mut pid = 0;
            unsafe {
                if windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId(
                    stream.as_raw_handle(),
                    &mut pid,
                ) != 0
                {
                    windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
                }
            }
        }
        #[cfg(windows)]
        let mut connection = windows::ClientConnection(stream);
        #[cfg(windows)]
        let stream = &mut connection.0;
        #[cfg(unix)]
        let mut stream = stream;
        stream.write_all(&encoded).await?;
        stream.write_all(b"\n").await?;
        let mut reader = BufReader::new(stream);
        let first: Reply = serde_json::from_slice(&line(&mut reader, RESPONSE_LIMIT).await?)?;
        ensure!(first.ok, "daemon command failed: {}", first.value);
        let mut value = first.value;
        let mut more = first.more;
        while more {
            let page: Reply = serde_json::from_slice(&line(&mut reader, RESPONSE_LIMIT).await?)?;
            ensure!(page.ok, "daemon command failed: {}", page.value);
            more = page.more;
            append_page(&mut value, page.value)?;
        }
        Ok(value)
    })
    .await?
}
/// Shared CLI parser; keep original path semantics and reject links before IPC.
pub fn send_command(args: &[String]) -> Result<Command> {
    parse_send_command(args, true)
}
/// Desktop entry points leave file errors to the window so Dolphin users see them.
pub fn desktop_send_command(args: &[String]) -> Command {
    match parse_send_command(args, false) {
        Ok(Command::SendFiles {
            paths,
            targets: targets @ crate::outbox::Targets::All(_),
            request_id,
        }) => Command::PresentFiles {
            paths,
            targets,
            request_id,
        },
        Ok(command @ Command::SendFiles { .. }) => command,
        Ok(_) => unreachable!("send parser"),
        Err(error) => Command::PresentSendError {
            message: error.to_string(),
        },
    }
}
fn parse_send_command(args: &[String], validate_files: bool) -> Result<Command> {
    let mut paths = Vec::new();
    let mut devices = Vec::new();
    let mut args = args.iter();
    let mut options = true;
    let mut request_id = None;
    while let Some(arg) = args.next() {
        if options && arg == "--" {
            options = false;
            continue;
        }
        if options && arg == "--request-id" {
            request_id = Some(args.next().context("--request-id needs an ID")?.clone());
            continue;
        }
        if options && arg == "--to" {
            devices.push(Id::new(
                args.next().context("--to needs a device ID")?.clone(),
            )?);
            continue;
        }
        ensure!(
            !options || !arg.starts_with('-'),
            "unknown send option; use -- before a filename starting with -"
        );
        let path = std::path::absolute(arg)?;
        if validate_files {
            let file = crate::storage::open_regular_file(&path)?;
            ensure!(
                file.metadata()?.len() <= crate::outbox::PEER_BYTES.min(pneu_proto::MAX_FILE_SIZE),
                "file exceeds send size limit"
            );
        }
        paths.push(path);
    }
    ensure!(
        !paths.is_empty() && paths.len() <= 128,
        "send needs 1 to 128 regular files"
    );
    Ok(Command::SendFiles {
        paths,
        request_id: Some(request_id.unwrap_or_else(|| crate::random_hex(16))),
        targets: if devices.is_empty() {
            crate::outbox::Targets::All("all".into())
        } else {
            crate::outbox::Targets::Devices(devices)
        },
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_existing_device_accepts_legacy_and_fenced_requests() -> Result<()> {
        let device = Id::from_bytes([1; 16]);
        for extra in [
            serde_json::json!({}),
            serde_json::json!({"binding_generation":3,"expected_scope":"ask"}),
        ] {
            let mut request = serde_json::json!({"command":"link_existing_device","device":device});
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let command: Command = serde_json::from_value(request.clone())?;
            assert_eq!(serde_json::to_value(command)?, request);
        }
        Ok(())
    }

    #[cfg(unix)]
    struct NoActions;
    #[cfg(unix)]
    #[async_trait::async_trait]
    impl crate::actions::PcActions for NoActions {
        async fn clipboard_text(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn clipboard_image(&self, _: &Path, _: &str) -> Result<()> {
            Ok(())
        }
        async fn notify(&self, _: &Path, _: &str) -> Result<()> {
            Ok(())
        }
        async fn open(&self, _: &Path) -> Result<()> {
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn open_folder_ipc_creates_missing_folder_and_launches_private_xdg_open() -> Result<()> {
        let Some(root) = std::env::var_os("PNEU_OPEN_FOLDER_TEST_ROOT") else {
            let dir = crate::test_support::scratch()?;
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin)?;
            let opener = bin.join("xdg-open");
            std::fs::write(
                &opener,
                "#!/bin/sh\ntest -d \"$1\" || exit 3\nprintf '%s\\n' \"$1\" >> \"$PNEU_OPEN_FOLDER_TEST_ROOT/calls\"\nif [ -f \"$PNEU_OPEN_FOLDER_TEST_ROOT/fail\" ]; then exit 1; fi\n",
            )?;
            std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o700))?;
            let status = tokio::time::timeout(Duration::from_secs(30),
                tokio::process::Command::new(std::env::current_exe()?)
                    .args(["--exact", "ipc::tests::open_folder_ipc_creates_missing_folder_and_launches_private_xdg_open", "--nocapture"])
                    .env("PNEU_OPEN_FOLDER_TEST_ROOT", dir.path())
                    .env("PATH", bin)
                    .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}/unavailable-bus", dir.path().display()))
                    .env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY")
                    .kill_on_drop(true).status()
            ).await??;
            anyhow::ensure!(status.success(), "private open-folder IPC test failed");
            return Ok(());
        };
        let root = std::path::PathBuf::from(root);
        let folder = root.join("Received files 日本語/missing");
        let paths = crate::Paths {
            data: root.join("data"),
            cache: root.join("cache"),
            config: root.join("config/config.toml"),
            socket: root.join("ipc.sock"),
        };
        let core = Core::new(
            paths.clone(),
            Config {
                save_path: folder.clone(),
                ..Config::default()
            },
            Arc::new(NoActions),
        )
        .await?;
        assert!(!folder.exists());
        let (listener, _owner) = bind(&paths.socket)?;
        let (stop, rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve(core.clone(), listener, rx));
        request(&paths.socket, Command::OpenFolder).await?;
        assert!(folder.is_dir());
        let expected = format!("{}\n", folder.display());
        assert_eq!(std::fs::read_to_string(root.join("calls"))?, expected);
        std::fs::write(root.join("fail"), b"")?;
        let error = request(&paths.socket, Command::OpenFolder)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("folder opener exited unsuccessfully")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("calls"))?,
            expected.repeat(2)
        );
        std::fs::remove_dir(&folder)?;
        std::fs::write(&folder, b"not a directory")?;
        assert!(request(&paths.socket, Command::OpenFolder).await.is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("calls"))?,
            expected.repeat(2)
        );
        let _ = stop.send(true);
        tokio::time::timeout(Duration::from_secs(2), server).await???;
        core.shutdown_actions().await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn disconnected_client_queued_before_accept_does_not_stop_server() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let paths = crate::Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: dir.path().join("ipc.sock"),
        };
        let core = Core::new(
            paths.clone(),
            Config {
                save_path: dir.path().join("saved"),
                ..Config::default()
            },
            Arc::new(NoActions),
        )
        .await?;
        let (listener, _owner) = bind(&paths.socket)?;
        // The server has not been polled yet. Darwin's peer_cred() fails with
        // ENOTCONN when it later accepts this already disconnected client.
        drop(UnixStream::connect(&paths.socket).await?);
        let (stop, rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve(core.clone(), listener, rx));
        let status = tokio::time::timeout(
            Duration::from_secs(2),
            request(&paths.socket, Command::Status),
        )
        .await;
        let running = !server.is_finished();
        let _ = stop.send(true);
        let stopped = tokio::time::timeout(Duration::from_secs(2), server).await??;
        core.shutdown_actions().await?;
        assert!(
            running,
            "IPC server exited: {stopped:?}; Status: {status:?}"
        );
        let status = status??;
        assert!(status["devices"].is_array(), "Status reply: {status}");
        stopped
    }

    #[test]
    fn unavailable_desktop_preserves_send_parser_errors() {
        for args in [vec![], vec!["--to".into()], vec!["--unknown".into()]] {
            let expected = send_command(&args).err().unwrap().to_string();
            let command = desktop_send_command(&args);
            let error = command
                .check_desktop(false, || anyhow::bail!("desktop unavailable"))
                .unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert!(command.check_desktop(false, || Ok(())).is_ok());
        }
        let error = Command::PresentSendError {
            message: "x".repeat(4097),
        }
        .check_desktop(false, || anyhow::bail!("desktop unavailable"))
        .unwrap_err();
        assert_eq!(error.to_string(), "send error is too long");
    }

    #[test]
    fn unavailable_desktop_gates_only_window_and_picker_requests() {
        use crate::outbox::Targets;
        for choose_devices in [false, true] {
            let all = || Targets::All("all".into());
            let devices = || Targets::Devices(vec![Id::from_bytes([1; 16])]);
            let captured = || Targets::Captured(Vec::new());
            for (command, needs_ui) in [
                (Command::AddDevice, true),
                #[cfg(any(target_os = "linux", windows))]
                (Command::OpenFolder, true),
                (
                    Command::ShowWindow {
                        choose_folder: false,
                    },
                    true,
                ),
                (
                    Command::ShowWindow {
                        choose_folder: true,
                    },
                    true,
                ),
                (
                    Command::PresentFiles {
                        paths: vec![],
                        targets: all(),
                        request_id: None,
                    },
                    true,
                ),
                (
                    Command::SendFiles {
                        paths: vec![],
                        targets: all(),
                        request_id: None,
                    },
                    choose_devices,
                ),
                (
                    Command::SendClipboard {
                        targets: all(),
                        notify_too_large: false,
                    },
                    choose_devices,
                ),
                (
                    Command::SendFiles {
                        paths: vec![],
                        targets: devices(),
                        request_id: None,
                    },
                    false,
                ),
                (
                    Command::SendClipboard {
                        targets: devices(),
                        notify_too_large: false,
                    },
                    false,
                ),
                (
                    Command::SendFiles {
                        paths: vec![],
                        targets: captured(),
                        request_id: None,
                    },
                    false,
                ),
                (
                    Command::SendClipboard {
                        notify_too_large: false,
                        targets: captured(),
                    },
                    false,
                ),
                (Command::Status, false),
            ] {
                let result =
                    command.check_desktop(choose_devices, || anyhow::bail!("desktop unavailable"));
                assert_eq!(result.is_err(), needs_ui);
                if let Err(error) = result {
                    assert_eq!(error.to_string(), "desktop unavailable");
                }
            }
        }
    }

    #[test]
    fn readmission_decisions_roundtrip_exact_removal_authority() -> Result<()> {
        let key = crate::store::members::ReadmissionKey {
            device: Id::from_bytes([3; 16]),
            fingerprint: Hash::new("c".repeat(64))?,
            removal_hlc: u64::MAX,
            removal_writer: Id::from_bytes([2; 16]),
            setup_generation: 7,
        };
        for allow in [false, true] {
            let value =
                serde_json::json!({"command":"readmission_decision","key":key,"allow":allow});
            let command: Command = serde_json::from_value(value.clone())?;
            assert_eq!(serde_json::to_value(command)?, value);
        }
        Ok(())
    }

    #[test]
    fn transfer_commands_accept_optional_artifact_and_roundtrip_it() -> Result<()> {
        let operation = Id::from_bytes([1; 16]);
        let artifact = Id::from_bytes([2; 16]);
        let device = Id::from_bytes([3; 16]);
        for command in ["dismiss_transfer", "cancel_transfer", "retry_transfer"] {
            for scoped in [false, true] {
                let mut value =
                    serde_json::json!({"command":command,"operation":operation,"device":device});
                if scoped {
                    value["artifact"] = serde_json::json!(artifact);
                }
                let parsed: Command = serde_json::from_value(value)?;
                let actual = match &parsed {
                    Command::DismissTransfer { artifact, .. }
                    | Command::CancelTransfer { artifact, .. }
                    | Command::RetryTransfer { artifact, .. } => artifact,
                    _ => unreachable!(),
                };
                assert_eq!(actual.as_ref(), scoped.then_some(&artifact));
                let encoded = serde_json::to_value(parsed)?;
                assert_eq!(
                    encoded["artifact"],
                    if scoped {
                        serde_json::json!(artifact)
                    } else {
                        serde_json::Value::Null
                    }
                );
            }
        }
        Ok(())
    }

    #[test]
    fn desktop_cli_defers_unreadable_sources_and_parser_errors_to_the_window() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let missing = dir
            .path()
            .join("missing.txt")
            .to_string_lossy()
            .into_owned();
        assert!(send_command(std::slice::from_ref(&missing)).is_err());
        assert!(
            matches!(desktop_send_command(&[missing]), Command::PresentFiles {paths, ..} if paths.len() == 1)
        );
        assert!(
            matches!(desktop_send_command(&[]), Command::PresentSendError {message} if message.contains("1 to 128"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn maximum_supported_requests_fit_including_json_escaping() -> Result<()> {
        // Linux PATH_MAX includes the NUL. Control bytes are legal in names
        // and JSON expands each one to six bytes. No real paths are touched.
        let path = std::path::PathBuf::from(format!("/{}", "\u{1}".repeat(4094)));
        let targets = crate::outbox::Targets::Captured(
            (1..=16)
                .map(|n| crate::outbox::Authority {
                    device: Id::from_bytes([n; 16]),
                    generation: u64::MAX,
                    fingerprint: Hash::new("f".repeat(64)).unwrap(),
                })
                .collect(),
        );
        let commands = [
            Command::SendFiles {
                paths: vec![path.clone(); 128],
                targets,
                request_id: Some("r".repeat(128)),
            },
            Command::Clipboard {
                device: Id::from_bytes([1; 16]),
                text: "\u{1}".repeat(pneu_proto::MAX_CLIPBOARD_BYTES),
            },
            Command::Join {
                uri: "\u{1}".repeat(pneu_proto::MAX_PAIRING_URI_BYTES),
            },
            Command::SaveFolder { folder: path },
            Command::Rename {
                name: "\u{1}".repeat(128),
            },
        ];
        for command in commands {
            let mut encoded = serde_json::to_vec(&command)?;
            encoded.push(b'\n');
            assert!(encoded.len() <= REQUEST_LIMIT);
            assert_eq!(
                line(&mut BufReader::new(encoded.as_slice()), REQUEST_LIMIT).await?,
                encoded
            );
            let _: Command = serde_json::from_slice(&encoded)?;
        }
        let encoded = vec![b'x'; REQUEST_LIMIT + 1];
        assert!(
            line(&mut BufReader::new(encoded.as_slice()), REQUEST_LIMIT)
                .await
                .is_err()
        );
        let command = Command::Rename {
            name: "x".repeat(REQUEST_LIMIT),
        };
        let error = request(Path::new("/does-not-exist"), command)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("4 MiB"),
            "preflight before connect: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn maximum_transfer_status_and_admission_history_use_bounded_lossless_pages() -> Result<()>
    {
        // 128 artifacts x 16 recipients, each with PATH_MAX-length source paths,
        // maximum filename/error escaping, and all terminal rows in one batch.
        let rows: Vec<_> = (0..128u8)
            .flat_map(|artifact| {
                (1..=16).map(move |device| serde_json::json!({
            "operation": Id::from_bytes([255;16]), "transfer_id": Id::from_bytes([artifact;16]),
            "device": Id::from_bytes([device;16]), "file_name": "\u{1}".repeat(255),
            "size": u64::MAX, "sent": u64::MAX, "state": "sent", "direction":"outgoing",
            "path": format!("/{}", "\u{1}".repeat(4094)), "error": "e".repeat(1024),
            "finished_at": u64::MAX
        }))
            })
            .collect();
        let values = [
            serde_json::json!({"snapshot":{"transfers":rows,"devices":[]},"config": Config::default(),"window_request":1}),
            serde_json::json!({"transfers":rows,"devices":[],"config":Config::default()}),
            serde_json::json!({"operation":Id::from_bytes([255;16]),"accepted":rows.iter().take(128).collect::<Vec<_>>(),"rejected":rows}),
        ];
        for value in values {
            let expected = value.clone();
            let pages = response_pages(Reply {
                ok: true,
                value,
                more: false,
            })?;
            assert!(pages.len() > 1);
            let count = pages.len();
            let mut assembled = None;
            for (index, page) in pages.into_iter().enumerate() {
                assert_eq!(page.more, index + 1 < count);
                let mut encoded = serde_json::to_vec(&page)?;
                encoded.push(b'\n');
                assert!(
                    encoded.len() <= RESPONSE_PAGE_BYTES,
                    "{} bytes",
                    encoded.len()
                );
                assert!(encoded.len() <= RESPONSE_LIMIT / 2);
                let decoded: Reply = serde_json::from_slice(
                    &line(&mut BufReader::new(encoded.as_slice()), RESPONSE_LIMIT).await?,
                )?;
                match &mut assembled {
                    None => assembled = Some(decoded.value),
                    Some(value) => append_page(value, decoded.value)?,
                }
            }
            assert_eq!(assembled, Some(expected));
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_reassembles_pages_on_one_socket_and_rejects_incomplete_replies() -> Result<()>
    {
        for complete in [true, false] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("ipc.sock");
            let listener = UnixListener::bind(&path)?;
            let expected = serde_json::json!({"snapshot":{"transfers":["a","b","c"]},"config":{}});
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut stream = BufReader::new(stream);
                let _ = line(&mut stream, REQUEST_LIMIT).await?;
                stream.get_mut().write_all(b"{\"ok\":true,\"more\":true,\"value\":{\"snapshot\":{\"transfers\":[\"a\"]},\"config\":{}}}\n").await?;
                if complete {
                    stream.get_mut().write_all(b"{\"ok\":true,\"more\":false,\"value\":{\"snapshot\":{\"transfers\":[\"b\",\"c\"]}}}\n").await?;
                }
                Ok::<_, anyhow::Error>(())
            });
            let result = request(&path, Command::UiStatus).await;
            if complete {
                assert_eq!(result?, expected);
            } else {
                assert!(result.is_err());
            }
            server.await??;
        }
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn socket_owner_deletes_only_its_original_entry_and_parent() -> Result<()> {
        for replacement in ["none", "file", "symlink", "socket", "parent"] {
            let dir = tempfile::tempdir()?;
            let parent = dir.path().join("runtime");
            std::fs::create_dir(&parent)?;
            #[cfg(target_os = "macos")]
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
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
    #[cfg(unix)]
    #[tokio::test]
    async fn binding_reclaims_stale_owned_socket_but_preserves_foreign_entries() -> Result<()> {
        let dir = tempfile::tempdir()?;
        #[cfg(target_os = "macos")]
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
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
