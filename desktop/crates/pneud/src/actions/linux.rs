use super::*;
use futures_util::StreamExt;
use std::{collections::HashMap, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
};

/// Desktop integration owns its clipboard helper and one D-Bus signal task.
/// wl-clipboard-rs owns the selection bytes; large images are capped to bound
/// its unavoidable buffers. The helper then serves the
/// data-control selection without a fork from a multithreaded Tokio process.
pub struct DesktopActions {
    cache: PathBuf,
    notifications: Mutex<Option<Notifications>>,
    jobs: Arc<crate::work::Work>,
    commands: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<crate::ui::Command>>>>,
    copy_results: Mutex<HashMap<PathBuf, bool>>,
    burst: Mutex<Option<Burst>>,
    #[cfg(test)]
    bus: Option<zbus::Connection>,
    #[cfg(test)]
    helper: Option<(PathBuf, Vec<String>)>,
    #[cfg(test)]
    helper_timeout: std::time::Duration,
}

pub(super) struct ClipboardCandidate {
    pub(super) child: Option<Child>,
    input: Option<tokio::process::ChildStdin>,
    parent_pipe: Option<tokio::process::ChildStdin>,
    jobs: Arc<crate::work::Work>,
}
impl ClipboardCandidate {
    pub(super) async fn stop(&mut self) {
        self.input.take();
        self.parent_pipe.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}
impl Drop for ClipboardCandidate {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let owned = crate::work::track(vec![self.jobs.clone()]);
            tokio::spawn(async move {
                let _owned = owned;
                let _ = child.wait().await;
            });
        }
    }
}
#[derive(Clone)]
struct Target {
    files: Vec<ReceivedFile>,
    paths: Vec<PathBuf>,
    link: Option<String>,
    retry: Option<(pneu_proto::Id, pneu_proto::Id, pneu_proto::Id)>,
    sms: Option<crate::sms::Target>,
}
struct Burst {
    id: u32,
    paths: Vec<PathBuf>,
    kind: Option<pneu_proto::FileKind>,
    count: usize,
    arrived: std::time::Instant,
    started: std::time::Instant,
    saved: bool,
    source: Option<ArrivalSource>,
}
impl Burst {
    fn accepts(&self, arrived: std::time::Instant) -> bool {
        arrived
            .checked_duration_since(self.arrived)
            .is_some_and(|gap| gap < std::time::Duration::from_secs(4))
            && arrived
                .checked_duration_since(self.started)
                .is_some_and(|span| span < std::time::Duration::from_secs(30))
            && self.count < 128
    }
}
struct Notifications {
    proxy: zbus::Proxy<'static>,
    task: JoinHandle<()>,
    stop: tokio::sync::watch::Sender<bool>,
    paths: Arc<Mutex<HashMap<u32, ReceivedFile>>>,
    targets: Arc<Mutex<HashMap<u32, Target>>>,
}
const NOTIFICATION_ACTION_LIMIT: usize = 256;
fn remember_notification(paths: &mut HashMap<u32, ReceivedFile>, id: u32, path: ReceivedFile) {
    if paths.len() == NOTIFICATION_ACTION_LIMIT
        && !paths.contains_key(&id)
        && let Some(old) = paths.keys().min().copied()
    {
        paths.remove(&old);
    }
    paths.insert(id, path);
}
impl DesktopActions {
    pub fn new(cache: PathBuf) -> Self {
        Self {
            cache,
            notifications: Mutex::new(None),
            jobs: Arc::new(crate::work::Work::default()),
            commands: Arc::new(std::sync::Mutex::new(None)),
            copy_results: Mutex::new(HashMap::new()),
            burst: Mutex::new(None),
            #[cfg(test)]
            bus: None,
            #[cfg(test)]
            helper: None,
            #[cfg(test)]
            helper_timeout: std::time::Duration::from_secs(10),
        }
    }
    pub fn set_commands(&self, commands: tokio::sync::mpsc::Sender<crate::ui::Command>) {
        *self.commands.lock().expect("UI commands") = Some(commands);
    }
    async fn prepare_copy(&self, input: ClipboardInput, ticket: u128) -> Result<PreparedClipboard> {
        let (mime, bytes) = match &input {
            ClipboardInput::SecretText(text) => (
                "application/x-pneu-secret-text".to_owned(),
                text.0.as_bytes().to_vec(),
            ),
            ClipboardInput::Text(text) => (
                "text/plain;charset=utf-8".to_owned(),
                text.as_bytes().to_vec(),
            ),
            ClipboardInput::Image(file, mime) => {
                let file = file.clone();
                let bytes = crate::work::blocking(self.jobs.clone(), move || {
                    file.read_bounded(16 * 1024 * 1024)
                })
                .await??;
                (mime.clone(), bytes)
            }
            ClipboardInput::Bytes { bytes, mime, .. } => (mime.clone(), bytes.clone()),
            ClipboardInput::Formats { text, png, .. } => {
                let mut bytes = (text.len() as u64).to_be_bytes().to_vec();
                bytes.extend_from_slice(text.as_bytes());
                bytes.extend_from_slice(png);
                ("application/x-pneu-formats".into(), bytes)
            }
        };
        let mut command = Command::new(std::env::current_exe()?);
        #[cfg(test)]
        if let Some((executable, args)) = &self.helper {
            command = Command::new(executable);
            command.args(args);
        }
        command
            .arg("clipboard-helper")
            .arg(mime)
            .arg(ticket.to_string())
            .env("TMPDIR", &self.cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let input_pipe = child.stdin.take().context("helper stdin")?;
        let mut output = BufReader::new(child.stdout.take().context("helper stdout")?.take(1024));
        let mut candidate = ClipboardCandidate {
            child: Some(child),
            input: Some(input_pipe),
            parent_pipe: None,
            jobs: self.jobs.clone(),
        };
        #[cfg(not(test))]
        let timeout = std::time::Duration::from_secs(10);
        #[cfg(test)]
        let timeout = self.helper_timeout;
        let result = tokio::time::timeout(timeout, async {
            let pipe = candidate.input.as_mut().context("helper stdin")?;
            pipe.write_all(&(bytes.len() as u64).to_be_bytes()).await?;
            pipe.write_all(&bytes).await?;
            let mut line = String::new();
            let n = output.read_line(&mut line).await?;
            anyhow::ensure!(n > 0 && line.ends_with('\n'), "missing clipboard READY");
            let mut fields = line.split_whitespace();
            anyhow::ensure!(fields.next() == Some("READY"), "invalid clipboard READY");
            let mut hashes = Vec::new();
            for hash in fields {
                anyhow::ensure!(hashes.len() < 8, "too many clipboard hashes");
                let mut digest = [0; 32];
                hex::decode_to_slice(hash, &mut digest).context("invalid clipboard hash")?;
                hashes.push(digest);
            }
            anyhow::ensure!(!hashes.is_empty(), "missing clipboard fingerprints");
            Ok::<_, anyhow::Error>(hashes)
        })
        .await
        .context("clipboard preparation timed out")
        .and_then(|r| r);
        match result {
            Ok(hashes) => Ok(PreparedClipboard {
                hashes,
                marker_confirmation: true,
                input,
                helper: Some(candidate),
                #[cfg(test)]
                test_exited: false,
            }),
            Err(error) => {
                candidate.stop().await;
                Err(error.context("Wayland data-control clipboard unavailable"))
            }
        }
    }
    async fn preview(
        &self,
        file: &ReceivedFile,
    ) -> Result<(i32, i32, i32, bool, i32, i32, Vec<u8>)> {
        let file = file.clone();
        crate::work::blocking(self.jobs.clone(), move || {
            let image = decode_image(&file.read_bounded(16 * 1024 * 1024)?)?
                .thumbnail(256, 256)
                .to_rgba8();
            let width = image.width() as i32;
            let height = image.height() as i32;
            Ok((width, height, width * 4, true, 8, 4, image.into_raw()))
        })
        .await?
    }
    async fn bus_connection(&self) -> Result<zbus::Connection> {
        #[cfg(test)]
        if let Some(bus) = &self.bus {
            return Ok(bus.clone());
        }
        Ok(zbus::Connection::session().await?)
    }
    async fn notifications(&self) -> Result<tokio::sync::MutexGuard<'_, Option<Notifications>>> {
        let mut current = self.notifications.lock().await;
        if current.is_none() {
            let connection = self.bus_connection().await?;
            let action_connection = connection.clone();
            let proxy = zbus::Proxy::new_owned(
                connection,
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
            )
            .await?;
            // One stream preserves ActivationToken -> ActionInvoked -> Closed ordering.
            let mut signals = proxy.receive_all_signals().await?;
            let paths = Arc::new(Mutex::new(HashMap::<u32, ReceivedFile>::new()));
            let action_paths = paths.clone();
            let targets = Arc::new(Mutex::new(HashMap::<u32, Target>::new()));
            let action_targets = targets.clone();
            let commands = self.commands.clone();
            let (stop, mut stopped) = tokio::sync::watch::channel(false);
            let task = tokio::spawn(Box::pin(async move {
                let mut interactions = crate::ui::portal::Interactions::new();
                let mut tokens = crate::ui::activation::NotificationTokens::default();
                loop {
                    tokio::select! { biased;
                        _=stopped.changed()=>break,
                        signal=signals.next()=>{
                            let Some(signal)=signal else {break;};
                            match signal.header().member().map(|m| m.as_str()) {
                                Some("ActivationToken") => {
                                    if let Ok((id, token)) = signal.body().deserialize::<(u32, String)>() {
                                        // Check the live mappings, not an independently evictable ID cache.
                                        let eligible = action_targets.lock().await.contains_key(&id)
                                            || action_paths.lock().await.contains_key(&id);
                                        if eligible { tokens.receive(id, token, std::time::Instant::now()); }
                                    }
                                    continue;
                                }
                                Some("NotificationClosed") => {
                                    if let Ok((id, _)) = signal.body().deserialize::<(u32, u32)>() {
                                        action_paths.lock().await.remove(&id);
                                        action_targets.lock().await.remove(&id);
                                        tokens.remove(id);
                                    }
                                    continue;
                                }
                                Some("ActionInvoked") => {},
                                _ => continue,
                            }
                            if let Ok((id,action))=signal.body().deserialize::<(u32,String)>() {
                                let target=action_targets.lock().await.get(&id).cloned();
                                let path=action_paths.lock().await.get(&id).cloned();
                                let token=tokens.take(id, std::time::Instant::now());
                                if let Some(target)=target {
                                    match action.as_str() {
                                        "default"|"open" => { if interactions.available() && let Some(file)=target.files.first() && let Some(wait)=notification_open(file,&action_connection,token.as_deref()).await { interactions.push(async move { let _=wait.await; }); } },
                                        "show" => { if let Ok(urls)=target.paths.iter().map(|path| file_uri(path)).collect::<Result<Vec<_>>>() { let _=show_files(&urls,&action_connection,token.as_deref()).await; } else { let _=file_unavailable_notice(&action_connection).await; } },
                                        "link" => { if interactions.available() && let Some(link)=target.link { let wait=begin_open_uri(&link,&action_connection,token.as_deref()).await; interactions.push(async move { let _=wait.await; }); } },
                                        "sms-code"|"sms-text" => {
                                            let sender={commands.lock().expect("UI commands").clone()};
                                            if let Some(target)=target.sms && let Some(sender)=sender {
                                                let _=sender.try_send(crate::ui::Command::CopySms {target,code:action=="sms-code"});
                                                let _=action_connection.call_method(Some("org.freedesktop.Notifications"),"/org/freedesktop/Notifications",Some("org.freedesktop.Notifications"),"CloseNotification",&(id,)).await;
                                            }
                                        },
                                        "retry" => { if let Some((operation,artifact,device))=target.retry && let Some(commands)=commands.lock().expect("UI commands").clone() { let _=commands.try_send(crate::ui::Command::RetryTransfer {operation,artifact,device}); } },
                                        "folder"|"pair" => { if let Some(commands)=commands.lock().expect("UI commands").clone() { let _=commands.try_send(if action=="folder" { crate::ui::Command::ChooseFolder } else { crate::ui::Command::AddDevice { activation_token: token } }); } },
                                        _=>{},
                                    }
                                    continue;
                                }
                                if let Some(path)=path {match action.as_str() {"default"|"open"=>{if interactions.available() && let Some(wait)=notification_open(&path, &action_connection,token.as_deref()).await { interactions.push(async move { let _=wait.await; }); }},"show"=>{if let Ok(uri)=file_uri(path.path()) { let _=show_files(&[uri], &action_connection,token.as_deref()).await; } else { let _=file_unavailable_notice(&action_connection).await; }},_=>{}}}
                            }
                        }
                    }
                }
                interactions.shutdown().await;
            }));
            *current = Some(Notifications {
                proxy,
                task,
                stop,
                paths,
                targets,
            });
        }
        Ok(current)
    }
    pub async fn notice(
        &self,
        notice: crate::ui::notifications::Notice,
        files: Vec<ReceivedFile>,
        link: Option<String>,
        replaces: u32,
        preview: Option<&ReceivedFile>,
    ) -> Result<u32> {
        self.notice_with_retry(notice, files, link, replaces, preview, None)
            .await
    }
    async fn notice_with_retry(
        &self,
        notice: crate::ui::notifications::Notice,
        files: Vec<ReceivedFile>,
        link: Option<String>,
        replaces: u32,
        preview: Option<&ReceivedFile>,
        retry: Option<(pneu_proto::Id, pneu_proto::Id, pneu_proto::Id)>,
    ) -> Result<u32> {
        let paths = files.iter().map(|file| file.path().to_owned()).collect();
        self.notice_with_paths(notice, files, link, replaces, preview, retry, paths)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn notice_with_paths(
        &self,
        notice: crate::ui::notifications::Notice,
        files: Vec<ReceivedFile>,
        link: Option<String>,
        replaces: u32,
        preview: Option<&ReceivedFile>,
        retry: Option<(pneu_proto::Id, pneu_proto::Id, pneu_proto::Id)>,
        paths: Vec<PathBuf>,
    ) -> Result<u32> {
        let current = self.notifications().await?;
        let n = current.as_ref().context("notification proxy")?;
        let mut hints = HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        hints.insert("desktop-entry", zbus::zvariant::Value::from("pneu"));
        hints.insert("urgency", zbus::zvariant::Value::from(notice.urgency));
        if notice.sms_target.is_some() || notice.summary.ends_with(" more SMS") {
            hints.insert("category", zbus::zvariant::Value::from("im.received"));
        }
        if notice.transient {
            hints.insert("transient", zbus::zvariant::Value::from(true));
        }
        let urls = if notice.urls.is_empty() {
            files
                .iter()
                .map(|f| file_uri(f.path()))
                .collect::<Result<Vec<_>>>()?
        } else {
            notice.urls.clone()
        };
        if !urls.is_empty() {
            hints.insert("x-kde-urls", zbus::zvariant::Value::from(urls.clone()));
            hints.insert("category", zbus::zvariant::Value::from("transfer.complete"));
        }
        if let Some(file) = preview
            && let Ok(preview) = self.preview(file).await
        {
            hints.insert(
                "image-data",
                zbus::zvariant::Value::from(zbus::zvariant::Structure::from(preview)),
            );
        }
        let mut targets = n.targets.lock().await;
        let id: u32 = n
            .proxy
            .call(
                "Notify",
                &(
                    "Pneu",
                    replaces,
                    "pneu",
                    &notice.summary,
                    &notice.body,
                    notice
                        .actions
                        .iter()
                        .flat_map(|(id, label)| [*id, *label])
                        .collect::<Vec<_>>(),
                    hints,
                    notice.expires,
                ),
            )
            .await?;
        if targets.len() >= NOTIFICATION_ACTION_LIMIT
            && !targets.contains_key(&id)
            && let Some(old) = targets.keys().min().copied()
        {
            targets.remove(&old);
        }
        targets.insert(
            id,
            Target {
                files,
                paths,
                link,
                retry,
                sms: notice.sms_target,
            },
        );
        Ok(id)
    }
}
#[async_trait]
impl PcActions for DesktopActions {
    async fn screen_locked(&self) -> bool {
        session_locked().await.unwrap_or(false)
    }
    async fn notify_sms(
        &self,
        notice: crate::ui::notifications::Notice,
        replaces: u32,
    ) -> Result<u32> {
        self.notice(notice, Vec::new(), None, replaces, None).await
    }
    async fn clipboard_text(&self, _text: &str) -> Result<()> {
        anyhow::bail!("clipboard writes require the clipboard actor")
    }
    async fn clipboard_image(&self, _path: &Path, _mime: &str) -> Result<()> {
        anyhow::bail!("clipboard writes require the clipboard actor")
    }
    async fn drain_clipboard(&self) {
        self.jobs.drain().await;
    }
    async fn prepare_clipboard(
        &self,
        input: ClipboardInput,
        ticket: u128,
    ) -> Result<PreparedClipboard> {
        self.prepare_copy(input, ticket).await
    }
    async fn install_clipboard(&self, prepared: &mut PreparedClipboard) -> Result<()> {
        let secret = matches!(prepared.input(), ClipboardInput::SecretText(_));
        let helper = prepared.helper.as_mut().context("clipboard helper")?;
        let mut pipe = helper
            .input
            .take()
            .context("clipboard authorization already consumed")?;
        pipe.write_all(b"G").await?;
        if secret {
            // Keep the parent-liveness writer open until this serving helper stops.
            helper.parent_pipe = Some(pipe);
        }
        Ok(())
    }
    async fn record_clipboard_copy(&self, file: &ReceivedFile) {
        let mut copies = self.copy_results.lock().await;
        if copies.len() >= 256 {
            copies.clear();
        }
        copies.insert(file.path().to_owned(), true);
    }
    async fn notify(&self, path: &Path, mime: &str) -> Result<()> {
        self.notify_received(&ReceivedFile::open(path)?, mime).await
    }
    async fn notify_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        let path = file.path();
        let current = self.notifications().await?;
        let n = current.as_ref().context("notification proxy")?;
        let mut hints = HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        if mime.starts_with("image/") {
            // Decode a bounded inline preview. The notification server never
            // receives a pathname from which it could read substituted bytes.
            let preview = self.preview(file).await?;
            hints.insert(
                "image-data",
                zbus::zvariant::Value::from(zbus::zvariant::Structure::from(preview)),
            );
        }
        hints.insert("desktop-entry", zbus::zvariant::Value::from("pneu"));
        let summary = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Received file");
        // Hold registration through Notify so an immediate closed signal cannot
        // race insertion of a stale live-notification mapping.
        let mut paths = n.paths.lock().await;
        let id: u32 = n
            .proxy
            .call(
                "Notify",
                &(
                    "Pneu",
                    0u32,
                    "pneu",
                    summary,
                    "",
                    vec!["default", "Open", "open", "Open", "show", "Show in Folder"],
                    hints,
                    -1i32,
                ),
            )
            .await?;
        remember_notification(&mut paths, id, file.clone());
        Ok(())
    }
    async fn notify_clipboard_image(&self, path: &Path, _mime: &str) -> Result<()> {
        self.notify_copied_received(&ReceivedFile::open(path)?, _mime)
            .await
    }
    async fn notify_copied_received(&self, file: &ReceivedFile, _mime: &str) -> Result<()> {
        let preview = self.preview(file).await?;
        let current = self.notifications().await?;
        let n = current.as_ref().context("notification proxy")?;
        let mut hints = HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        hints.insert(
            "image-data",
            zbus::zvariant::Value::from(zbus::zvariant::Structure::from(preview)),
        );
        hints.insert("desktop-entry", zbus::zvariant::Value::from("pneu"));
        hints.insert("transient", zbus::zvariant::Value::from(true));
        hints.insert("urgency", zbus::zvariant::Value::from(0u8));
        let _: u32 = n
            .proxy
            .call(
                "Notify",
                &(
                    "Pneu",
                    0u32,
                    "pneu",
                    &file
                        .notice(
                            crate::ui::notifications::Notice::new("Image copied from phone"),
                            true,
                        )
                        .summary,
                    "",
                    Vec::<&str>::new(),
                    hints,
                    4000i32,
                ),
            )
            .await?;
        Ok(())
    }
    async fn notify_clipboard_failure(&self) -> Result<()> {
        let current = self.notifications().await?;
        let n = current.as_ref().context("notification proxy")?;
        let mut hints = HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        hints.insert("desktop-entry", zbus::zvariant::Value::from("pneu"));
        hints.insert("transient", zbus::zvariant::Value::from(true));
        let _: u32 = n
            .proxy
            .call(
                "Notify",
                &(
                    "Pneu",
                    0u32,
                    "pneu",
                    "Couldn't copy image",
                    "",
                    Vec::<&str>::new(),
                    hints,
                    4000i32,
                ),
            )
            .await?;
        Ok(())
    }
    async fn notify_image_failure(
        &self,
        file: &ReceivedFile,
        metadata: &pneu_proto::FileBegin,
        saved: bool,
    ) -> Result<()> {
        self.notice(
            file.notice(
                crate::ui::notifications::Notice::file(
                    metadata,
                    saved,
                    false,
                    true,
                    None,
                    "",
                    crate::ui::ReceiveDestination::Cache,
                ),
                metadata.kind == pneu_proto::FileKind::ClipboardImage,
            ),
            if metadata.kind == pneu_proto::FileKind::ClipboardImage {
                Vec::new()
            } else {
                vec![file.clone()]
            },
            None,
            0,
            if metadata.kind == pneu_proto::FileKind::ClipboardImage {
                Some(file)
            } else {
                None
            },
        )
        .await
        .map(|_| ())
    }
    async fn notify_text_failure(&self) -> Result<()> {
        let mut notice = crate::ui::notifications::Notice::new("Copied from phone");
        notice.body = "Couldn't copy to clipboard".into();
        self.notice(notice, Vec::new(), None, 0, None)
            .await
            .map(|_| ())
    }
    async fn notify_save_failure(
        &self,
        metadata: &pneu_proto::FileBegin,
        code: pneu_proto::NackCode,
        destination: crate::ui::ReceiveDestination,
        folder: &Path,
    ) -> Result<()> {
        let mut notice = crate::ui::notifications::Notice::file(
            metadata,
            false,
            false,
            false,
            Some(code),
            &crate::ui::display_path(folder),
            destination,
        );
        notice
            .actions
            .retain(|(id, _)| !matches!(*id, "default" | "open"));
        self.notice(notice, Vec::new(), None, 0, None)
            .await
            .map(|_| ())
    }
    async fn notify_text(&self, text: &str) -> Result<()> {
        self.notice(
            crate::ui::notifications::Notice::text(text),
            Vec::new(),
            crate::ui::notifications::single_url(text),
            0,
            None,
        )
        .await
        .map(|_| ())
    }
    async fn notify_text_from(&self, text: &str, source: &ArrivalSource) -> Result<()> {
        self.notice(
            crate::ui::notifications::Notice::text(text).copied_from(&source.name),
            Vec::new(),
            crate::ui::notifications::single_url(text),
            0,
            None,
        )
        .await
        .map(|_| ())
    }
    async fn notify_text_failure_from(&self, source: &ArrivalSource) -> Result<()> {
        let mut notice =
            crate::ui::notifications::Notice::new("Copied from phone").copied_from(&source.name);
        notice.body = "Couldn't copy to clipboard".into();
        self.notice(notice, Vec::new(), None, 0, None)
            .await
            .map(|_| ())
    }
    async fn notify_save_failure_from(
        &self,
        metadata: &pneu_proto::FileBegin,
        code: pneu_proto::NackCode,
        destination: crate::ui::ReceiveDestination,
        folder: &Path,
        source: &ArrivalSource,
    ) -> Result<()> {
        let mut notice = source.file_notice(
            crate::ui::notifications::Notice::file(
                metadata,
                false,
                false,
                false,
                Some(code),
                &crate::ui::display_path(folder),
                destination,
            ),
            metadata.kind == pneu_proto::FileKind::ClipboardImage,
        );
        notice
            .actions
            .retain(|(id, _)| !matches!(*id, "default" | "open"));
        self.notice(notice, Vec::new(), None, 0, None)
            .await
            .map(|_| ())
    }
    async fn notify_send_admission_failure(&self, reason: &str) -> Result<()> {
        let mut notice = crate::ui::notifications::Notice::new("Couldn't send files");
        notice.body = crate::ui::notifications::escape(reason);
        self.notice(notice, Vec::new(), None, 0, None)
            .await
            .map(|_| ())
    }
    async fn notify_delivery(
        &self,
        transfer: &crate::outbox::Transfer,
        device_name: &str,
    ) -> Result<()> {
        let (notice, retry) = if transfer.state == "sent" {
            (
                crate::ui::notifications::Notice::sent(&transfer.file_name, &[device_name.into()]),
                None,
            )
        } else {
            (
                crate::ui::notifications::Notice::send_failed(
                    &transfer.file_name,
                    device_name,
                    transfer.error.as_deref().unwrap_or("Transfer failed"),
                ),
                Some((
                    transfer.operation.clone(),
                    transfer.transfer_id.clone(),
                    transfer.device.clone(),
                )),
            )
        };
        self.notice_with_retry(notice, Vec::new(), None, 0, None, retry)
            .await
            .map(|_| ())
    }
    async fn notify_file(
        &self,
        file: &ReceivedFile,
        metadata: &pneu_proto::FileBegin,
        saved: bool,
        save_error: Option<pneu_proto::NackCode>,
        folder: &Path,
    ) -> Result<()> {
        self.notify_file_at(
            file,
            metadata,
            saved,
            save_error,
            folder,
            std::time::Instant::now(),
        )
        .await
    }
    async fn notify_file_at(
        &self,
        file: &ReceivedFile,
        metadata: &pneu_proto::FileBegin,
        saved: bool,
        save_error: Option<pneu_proto::NackCode>,
        folder: &Path,
        arrived: std::time::Instant,
    ) -> Result<()> {
        let copied = self.copy_results.lock().await.remove(file.path());
        let mut notice = crate::ui::notifications::Notice::file(
            metadata,
            saved,
            copied == Some(true),
            copied == Some(false),
            save_error,
            &crate::ui::display_path(folder),
            crate::ui::ReceiveDestination::SaveFolder,
        );
        let mut burst = self.burst.lock().await;
        let mut paths = vec![file.path().to_owned()];
        let mut replaces = 0;
        let mut count = 1;
        let mut kind = Some(metadata.kind);
        let mut all_saved = saved;
        let mut started = arrived;
        if save_error.is_none()
            && copied != Some(false)
            && let Some(previous) = burst.as_ref()
            && previous.accepts(arrived)
            && previous.source == file.source
        {
            let current = self.notifications().await?;
            if current
                .as_ref()
                .context("notifications")?
                .targets
                .lock()
                .await
                .contains_key(&previous.id)
            {
                replaces = previous.id;
                count = previous.count + 1;
                started = previous.started;
                kind = if previous.kind == kind { kind } else { None };
                all_saved = saved && previous.saved;
                paths.extend(previous.paths.iter().cloned());
                notice = crate::ui::notifications::Notice::burst(
                    count,
                    kind,
                    copied == Some(true),
                    all_saved,
                );
            }
        }
        notice = file.notice(
            notice,
            metadata.kind == pneu_proto::FileKind::ClipboardImage,
        );
        notice.urls = paths.iter().map(|p| file_uri(p)).collect::<Result<_>>()?;
        // Show retains navigation paths for the burst. Open retains only the
        // latest file's anchored descriptor.
        let id = self
            .notice_with_paths(
                notice,
                vec![file.clone()],
                None,
                replaces,
                None,
                None,
                paths.clone(),
            )
            .await?;
        *burst = if save_error.is_none() && copied != Some(false) {
            Some(Burst {
                id,
                paths,
                kind,
                count,
                arrived,
                started,
                saved: all_saved,
                source: file.source.clone(),
            })
        } else {
            None
        };
        Ok(())
    }
    async fn open(&self, path: &Path) -> Result<()> {
        self.open_received(&ReceivedFile::open(path)?).await
    }
    async fn open_received(&self, file: &ReceivedFile) -> Result<()> {
        self.begin_open_received(file).await?.await
    }
    async fn begin_open_received(
        &self,
        file: &ReceivedFile,
    ) -> Result<crate::ui::portal::Interaction> {
        begin_open_received_file(file, &self.bus_connection().await?, None).await
    }
    async fn shutdown(&self) -> Result<()> {
        self.jobs.drain().await;
        if let Some(n) = self.notifications.lock().await.take() {
            n.stop.send_replace(true);
            let _ = n.task.await;
        }
        Ok(())
    }
}
fn file_uri(path: &Path) -> Result<String> {
    Ok(url::Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("invalid local path"))?
        .to_string())
}
/// OpenFile transfers an FD to the portal, including ownership across the
/// asynchronous viewer launch. There is deliberately no pathname fallback.
#[cfg(test)]
async fn open_received_file(file: &ReceivedFile, connection: &zbus::Connection) -> Result<()> {
    begin_open_received_file(file, connection, None)
        .await?
        .await
}
pub(crate) async fn begin_open_received_file(
    file: &ReceivedFile,
    connection: &zbus::Connection,
    activation_token: Option<&str>,
) -> Result<crate::ui::portal::Interaction> {
    let request = crate::ui::portal::Request::new(connection).await?;
    let proxy = request
        .method_proxy("org.freedesktop.portal.OpenURI")
        .await?;
    let descriptor = file.viewer_descriptor()?;
    let fd = zbus::zvariant::Fd::from(&descriptor);
    let path = crate::ui::portal::accept(proxy.call(
        "OpenFile",
        &(
            "",
            fd,
            crate::ui::activation::open_uri_options(&request.token, activation_token),
        ),
    ))
    .await?;
    Ok(Box::pin(async move {
        let result = request.response(path).await;
        drop(descriptor); // Retain the read-only FD throughout interactive launch.
        result?;
        Ok(())
    }))
}
async fn notification_open(
    file: &ReceivedFile,
    connection: &zbus::Connection,
    activation_token: Option<&str>,
) -> Option<crate::ui::portal::Interaction> {
    match begin_open_received_file(file, connection, activation_token).await {
        Ok(wait) => Some(wait),
        Err(error) => {
            if error.is::<FileUnavailable>() {
                let _ = file_unavailable_notice(connection).await;
            }
            None
        }
    }
}
async fn file_unavailable_notice(connection: &zbus::Connection) -> Result<()> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let _: u32 = proxy
        .call(
            "Notify",
            &(
                "Pneu",
                0u32,
                "pneu",
                "The file was moved or deleted",
                "",
                Vec::<&str>::new(),
                HashMap::<&str, zbus::zvariant::Value<'_>>::new(),
                -1i32,
            ),
        )
        .await?;
    Ok(())
}
async fn show_files(
    urls: &[String],
    connection: &zbus::Connection,
    activation_token: Option<&str>,
) -> Result<()> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.FileManager1",
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
    )
    .await?;
    let _: () = proxy
        .call("ShowItems", &(urls, activation_token.unwrap_or_default()))
        .await?;
    Ok(())
}
pub async fn open_uri(uri: &str, connection: &zbus::Connection) -> Result<()> {
    begin_open_uri(uri, connection, None).await.await
}
async fn begin_open_uri(
    uri: &str,
    connection: &zbus::Connection,
    activation_token: Option<&str>,
) -> crate::ui::portal::Interaction {
    let portal = begin_open_uri_portal(uri, connection, activation_token).await;
    let uri = uri.to_owned();
    let activation_token = activation_token.map(str::to_owned);
    Box::pin(async move {
        crate::ui::portal::with_fallback(async { portal?.await }, || async {
            let status = crate::ui::activation::command("xdg-open", activation_token.as_deref())
                .arg(&uri)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status()
                .await?;
            anyhow::ensure!(status.success(), "URI opener exited unsuccessfully");
            Ok(())
        })
        .await
    })
}
pub(crate) async fn open_uri_portal(
    uri: &str,
    connection: &zbus::Connection,
    activation_token: Option<&str>,
) -> Result<()> {
    begin_open_uri_portal(uri, connection, activation_token)
        .await?
        .await
}
async fn begin_open_uri_portal(
    uri: &str,
    connection: &zbus::Connection,
    activation_token: Option<&str>,
) -> Result<crate::ui::portal::Interaction> {
    let request = crate::ui::portal::Request::new(connection).await?;
    let proxy = request
        .method_proxy("org.freedesktop.portal.OpenURI")
        .await?;
    let path = crate::ui::portal::accept(proxy.call(
        "OpenURI",
        &(
            "",
            uri,
            crate::ui::activation::open_uri_options(&request.token, activation_token),
        ),
    ))
    .await?;
    Ok(Box::pin(async move {
        request.response(path).await?;
        Ok(())
    }))
}
fn clipboard_sources(
    mime: String,
    bytes: Vec<u8>,
) -> Result<Vec<wl_clipboard_rs::copy::MimeSource>> {
    use wl_clipboard_rs::copy::{MimeSource, MimeType, Source};
    if mime == "application/x-pneu-secret-text" {
        let mut sources = clipboard_sources("text/plain;charset=utf-8".into(), bytes)?;
        sources.push(MimeSource {
            source: Source::Bytes(b"secret".to_vec().into()),
            mime_type: MimeType::Specific("x-kde-passwordManagerHint".into()),
        });
        return Ok(sources);
    }
    if mime == "application/x-pneu-formats" {
        anyhow::ensure!(bytes.len() >= 8, "invalid clipboard formats header");
        let length = u64::from_be_bytes(bytes[..8].try_into()?) as usize;
        anyhow::ensure!(
            length <= pneu_proto::MAX_CLIPBOARD_BYTES && bytes.len() >= 8 + length,
            "invalid clipboard formats length"
        );
        let text = std::str::from_utf8(&bytes[8..8 + length])?;
        let mut sources = clipboard_sources("image/png".into(), bytes[8 + length..].to_vec())?;
        sources.push(MimeSource {
            source: Source::Bytes(text.as_bytes().to_vec().into()),
            mime_type: MimeType::Specific("text/plain;charset=utf-8".into()),
        });
        return Ok(sources);
    }
    let mut sources = Vec::new();
    if mime.starts_with("image/") && mime != "image/png" {
        let mut png = std::io::Cursor::new(Vec::new());
        decode_image(&bytes)?.write_to(&mut png, image::ImageFormat::Png)?;
        anyhow::ensure!(
            png.get_ref().len() <= 32 * 1024 * 1024,
            "converted clipboard image too large"
        );
        sources.push(MimeSource {
            source: Source::Bytes(png.into_inner().into()),
            mime_type: MimeType::Specific("image/png".into()),
        });
    }
    sources.push(MimeSource {
        source: Source::Bytes(b"pneu".to_vec().into()),
        mime_type: MimeType::Specific(crate::ui::clipboard::ORIGIN_MIME.into()),
    });
    sources.push(MimeSource {
        source: Source::Bytes(bytes.into()),
        mime_type: MimeType::Specific(mime),
    });
    Ok(sources)
}
// Only the dedicated helper process calls this: neither daemon death nor a
// wedged daemon may leave a secret selection serving beyond its deadline.
fn guard_secret_lifetime(
    input: impl std::io::Read + Send + 'static,
    lifetime: std::time::Duration,
) {
    std::thread::spawn(move || {
        std::thread::sleep(lifetime);
        std::process::exit(0);
    });
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut { input }, &mut std::io::sink());
        std::process::exit(0); // Parent pipe closed (including SIGKILL).
    });
}
pub fn clipboard_helper(mime: String, ticket: u128) -> Result<()> {
    use std::io::{Read, Write};
    let secret = mime == "application/x-pneu-secret-text";
    let mut bytes = Vec::new();
    let mut input = std::io::stdin().lock();
    let mut length = [0; 8];
    input.read_exact(&mut length)?;
    let length = u64::from_be_bytes(length);
    anyhow::ensure!(
        length <= 16 * 1024 * 1024,
        "clipboard helper input too large"
    );
    input.by_ref().take(length).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 == length, "truncated helper input");
    anyhow::ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "clipboard helper input too large"
    );
    let mut options = wl_clipboard_rs::copy::Options::new();
    options.foreground(true);
    use sha2::{Digest, Sha256};
    let mut sources = clipboard_sources(mime, bytes)?;
    let hashes = sources
        .iter()
        .filter_map(|source| {
            if [
                crate::ui::clipboard::ORIGIN_MIME,
                "x-kde-passwordManagerHint",
            ]
            .iter()
            .any(|mime| {
                source.mime_type == wl_clipboard_rs::copy::MimeType::Specific((*mime).into())
            }) {
                return None;
            }
            if let wl_clipboard_rs::copy::Source::Bytes(bytes) = &source.source {
                Some(hex::encode(Sha256::digest(bytes)))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    // The daemon's ordered watcher is the sole installation witness.
    let marker = crate::ui::clipboard::install_marker(ticket);
    sources.push(wl_clipboard_rs::copy::MimeSource {
        source: wl_clipboard_rs::copy::Source::Bytes(Vec::new().into()),
        mime_type: wl_clipboard_rs::copy::MimeType::Specific(marker),
    });
    let selection = options.prepare_copy_multi(sources)?;
    println!("READY {hashes}");
    std::io::stdout().flush()?;
    let mut commit = [0];
    input.read_exact(&mut commit)?;
    anyhow::ensure!(commit == *b"G", "clipboard helper canceled");
    drop(input);
    if secret {
        guard_secret_lifetime(
            std::io::stdin(),
            std::time::Duration::from_secs(crate::sms::CLEAR_AFTER_SECS),
        );
    }
    selection.serve()?;
    Ok(())
}
#[cfg(test)]
#[path = "../actions_clipboard_tests.rs"]
mod clipboard_tests;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secret_helper_exits_on_own_deadline_and_parent_pipe_eof() -> Result<()> {
        use std::io::{BufRead, Write};
        const CHILD: &str = "PNEU_TEST_SECRET_GUARD";
        if let Ok(mode) = std::env::var(CHILD) {
            let duration = if mode == "deadline" { 250 } else { 10_000 };
            guard_secret_lifetime(std::io::stdin(), std::time::Duration::from_millis(duration));
            println!("GUARD READY");
            std::io::stdout().flush()?;
            std::thread::park();
            unreachable!();
        }
        for mode in ["deadline", "eof"] {
            let mut child = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", "actions::linux::tests::secret_helper_exits_on_own_deadline_and_parent_pipe_eof", "--nocapture"])
                .env(CHILD, mode)
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
            let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                if output.read_line(&mut line)? == 0 {
                    break;
                }
                if line.contains("GUARD READY") {
                    break;
                }
            }
            assert!(line.contains("GUARD READY"));
            assert!(child.try_wait()?.is_none());
            if mode == "eof" {
                drop(child.stdin.take());
            }
            let started = std::time::Instant::now();
            loop {
                if let Some(status) = child.try_wait()? {
                    assert!(status.success());
                    break;
                }
                if started.elapsed() > std::time::Duration::from_secs(2) {
                    child.kill()?;
                    child.wait()?;
                    panic!("secret guard did not exit on {mode}");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        Ok(())
    }
    #[test]
    fn anonymous_clipboard_backing_is_private_unlinked_and_descriptor_bound() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let dir = crate::test_support::scratch()?;
        let directory = std::fs::File::open(dir.path())?;
        for file in [
            clipboard_file(dir.path())?,
            unlinked_clipboard_file(&directory)?,
        ] {
            let metadata = file.metadata()?;
            assert!(metadata.is_file());
            assert_eq!(metadata.mode() & 0o777, 0o600);
            assert_eq!(metadata.nlink(), 0);
            assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
        }
        Ok(())
    }

    #[test]
    fn immediate_clipboard_bytes_and_formats_retain_payload_and_hashes() -> Result<()> {
        use sha2::{Digest, Sha256};
        let dir = crate::test_support::scratch()?;
        let png = b"clipboard image bytes".to_vec();
        let text = "clipboard text".to_owned();
        let image_hash: [u8; 32] = Sha256::digest(&png).into();
        let text_hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        for (input, hashes) in [
            (
                ClipboardInput::Bytes {
                    bytes: png.clone(),
                    mime: "image/png".into(),
                    cache: dir.path().to_owned(),
                },
                vec![image_hash],
            ),
            (
                ClipboardInput::Formats {
                    text,
                    png: png.clone(),
                    cache: dir.path().to_owned(),
                },
                vec![text_hash, image_hash],
            ),
        ] {
            let prepared = PreparedClipboard::immediate(input)?;
            assert_eq!(prepared.hashes, hashes);
            let ClipboardInput::Image(file, mime) = prepared.input() else {
                panic!("expected normalized image");
            };
            assert_eq!(mime, "image/png");
            assert_eq!(file.read_bounded(png.len())?, png);
            assert!(!file.path().exists());
            assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
        }
        Ok(())
    }

    #[test]
    fn burst_membership_uses_arrival_gaps_span_order_and_count() {
        use std::time::{Duration, Instant};
        let started = Instant::now() - Duration::from_secs(60);
        let mut burst = Burst {
            id: 1,
            paths: vec![],
            kind: None,
            count: 1,
            arrived: started,
            started,
            saved: true,
            source: None,
        };
        assert!(burst.accepts(started + Duration::from_secs(3)));
        assert!(!burst.accepts(started + Duration::from_secs(4)));
        assert!(!burst.accepts(started - Duration::from_secs(1)));
        burst.arrived = started + Duration::from_secs(28);
        assert!(burst.accepts(started + Duration::from_secs(29)));
        assert!(!burst.accepts(started + Duration::from_secs(30)));
        burst.count = 128;
        assert!(!burst.accepts(started + Duration::from_secs(29)));
    }
    #[test]
    fn clipboard_offers_original_and_real_png_bytes() -> Result<()> {
        let image = image::DynamicImage::new_rgb8(2, 2);
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image.write_to(&mut jpeg, image::ImageFormat::Jpeg)?;
        let sources = clipboard_sources("image/jpeg".into(), jpeg.into_inner())?;
        assert_eq!(sources.len(), 3);
        let wl_clipboard_rs::copy::Source::Bytes(bytes) = &sources[0].source else {
            panic!("expected bytes")
        };
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(decode_image(bytes)?.width(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn clipboard_preview_rejects_large_sparse_input_before_decoding() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("large.png");
        std::fs::File::create(&path)?.set_len(pneu_proto::MAX_FILE_SIZE)?;
        let actions = DesktopActions::new(dir.path().to_owned());
        let error = actions
            .notify_clipboard_image(&path, "image/png")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("bounded read"));
        Ok(())
    }
    #[test]
    fn received_descriptor_allows_files_above_the_outbox_quota() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("large-received.bin");
        let sparse = std::fs::File::create(&path)?;
        let size = crate::outbox::PEER_BYTES + 1;
        sparse.set_len(size)?;
        let received = ReceivedFile::from_file(path.clone(), sparse)?;
        assert_eq!(received.viewer_descriptor()?.metadata()?.len(), size);
        assert!(file_uri(received.path()).is_ok());
        assert!(crate::ipc::send_command(&[path.to_string_lossy().into_owned()]).is_err());
        Ok(())
    }

    #[test]
    fn received_viewer_descriptor_survives_path_replacement_and_unlink() -> Result<()> {
        for replacement in ["regular", "directory", "symlink", "parent", "deleted"] {
            let dir = tempfile::tempdir()?;
            let parent = dir.path().join("received");
            std::fs::create_dir(&parent)?;
            let path = parent.join("file");
            std::fs::write(&path, b"received bytes")?;
            let file = ReceivedFile::open(&path)?;
            let target = file.path().to_owned();
            if replacement == "parent" {
                std::fs::rename(&parent, dir.path().join("old-parent"))?;
                std::fs::create_dir(&parent)?;
                std::fs::write(&path, b"replacement")?;
            } else {
                std::fs::remove_file(&path)?;
                match replacement {
                    "regular" => std::fs::write(&path, b"replacement")?,
                    "directory" => std::fs::create_dir(&path)?,
                    "symlink" => std::os::unix::fs::symlink("/etc/passwd", &path)?,
                    "deleted" => (),
                    _ => unreachable!(),
                }
            }
            assert!(file_uri(&target).is_ok()); // Show is pathname navigation.
            let mut descriptor = file.viewer_descriptor()?;
            drop(file);
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut descriptor, &mut bytes)?;
            assert_eq!(bytes, b"received bytes", "{replacement}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn retained_file_and_preview_ignore_symlink_and_regular_replacements() -> Result<()> {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("received.png");
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3).write_to(&mut encoded, image::ImageFormat::Png)?;
        let bytes = encoded.into_inner();
        std::fs::write(&path, &bytes)?;
        let file = ReceivedFile::open(&path)?;
        assert!(file.is_image("image/png")?);
        let replacement = dir.path().join("secret");
        std::fs::write(&replacement, b"secret bytes")?;
        std::fs::remove_file(&path)?;
        symlink(&replacement, &path)?;
        assert!(ReceivedFile::open(&path).is_err());
        assert_eq!(file.read_bounded(16 * 1024 * 1024)?, bytes);
        let actions = DesktopActions::new(dir.path().to_owned());
        let preview = actions.preview(&file).await?;
        assert_eq!((preview.0, preview.1), (171, 256));
        std::fs::remove_file(&path)?;
        std::fs::write(&path, b"different inode")?;
        assert_eq!(file.read_bounded(16 * 1024 * 1024)?, bytes);
        let fifo = dir.path().join("pipe");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )?;
        assert!(
            ReceivedFile::open(&fifo).is_err(),
            "FIFO must fail without blocking"
        );
        actions.shutdown().await?;
        Ok(())
    }
    #[tokio::test]
    async fn shutdown_waits_for_canceled_preview_work() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let actions = Arc::new(DesktopActions::new(dir.path().to_owned()));
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocking) = std::sync::mpsc::channel();
        let job = crate::work::start_blocking(vec![actions.jobs.clone()], move || {
            let _ = entered.send(());
            blocking.recv()
        });
        wait.await?;
        job.abort(); // A started blocking job still owns its completion token.
        let shutdown = actions.clone();
        let task = tokio::spawn(async move { shutdown.shutdown().await });
        tokio::task::yield_now().await;
        let waiting = !task.is_finished();
        release.send(())?;
        task.await??;
        let _ = job.await;
        assert!(waiting);
        Ok(())
    }
    struct NotificationServer(
        std::sync::atomic::AtomicU32,
        Arc<std::sync::Mutex<HashMap<String, zbus::zvariant::OwnedValue>>>,
        Arc<std::sync::Mutex<Vec<String>>>,
        Arc<std::sync::Mutex<(String, String)>>,
    );
    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl NotificationServer {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            _app: &str,
            _replaces: u32,
            _icon: &str,
            summary: &str,
            body: &str,
            actions: Vec<String>,
            hints: HashMap<String, zbus::zvariant::OwnedValue>,
            _timeout: i32,
        ) -> u32 {
            *self.3.lock().unwrap() = (summary.into(), body.into());
            *self.1.lock().unwrap() = hints;
            *self.2.lock().unwrap() = actions;
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
        }
    }
    struct PortalServer(
        Arc<std::sync::Mutex<Vec<u8>>>,
        Arc<std::sync::atomic::AtomicU32>,
        Arc<std::sync::Mutex<Option<String>>>,
        Arc<std::sync::atomic::AtomicU32>,
        Arc<std::sync::atomic::AtomicU32>,
        Arc<std::sync::Mutex<Option<String>>>,
    );
    struct PortalRequest(Arc<std::sync::atomic::AtomicU32>);
    #[zbus::interface(name = "org.freedesktop.portal.Request")]
    impl PortalRequest {
        fn close(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    struct FileManagerServer(
        Arc<std::sync::atomic::AtomicU32>,
        Arc<std::sync::Mutex<String>>,
    );
    #[zbus::interface(name = "org.freedesktop.FileManager1")]
    impl FileManagerServer {
        fn show_items(&self, _urls: Vec<String>, startup: &str) {
            *self.1.lock().unwrap() = startup.to_owned();
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    impl PortalServer {
        async fn respond(
            &self,
            options: HashMap<String, zbus::zvariant::OwnedValue>,
            header: zbus::message::Header<'_>,
            connection: &zbus::Connection,
        ) -> zbus::zvariant::OwnedObjectPath {
            assert!(
                options
                    .keys()
                    .all(|key| matches!(key.as_str(), "handle_token" | "activation_token")),
                "unsupported OpenURI option"
            );
            *self.5.lock().unwrap() = options
                .get("activation_token")
                .map(|value| String::try_from(value.try_clone().unwrap()).unwrap());
            let token: String = options
                .get("handle_token")
                .unwrap()
                .try_clone()
                .unwrap()
                .try_into()
                .unwrap();
            let sender = header
                .sender()
                .unwrap()
                .as_str()
                .trim_start_matches(':')
                .replace('.', "_");
            let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
            self.3.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            connection
                .object_server()
                .at(path.clone(), PortalRequest(self.4.clone()))
                .await
                .unwrap();
            if self.1.load(std::sync::atomic::Ordering::SeqCst) == 3 {
                *self.2.lock().unwrap() = Some(path.clone());
                return path.try_into().unwrap();
            }
            // Emit before returning acceptance to exercise the subscription race.
            connection
                .emit_signal(
                    None::<&str>,
                    path.as_str(),
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(
                        self.1.load(std::sync::atomic::Ordering::SeqCst),
                        HashMap::<String, zbus::zvariant::OwnedValue>::new(),
                    ),
                )
                .await
                .unwrap();
            path.try_into().unwrap()
        }
    }
    #[zbus::interface(name = "org.freedesktop.portal.OpenURI")]
    impl PortalServer {
        async fn open_file(
            &self,
            _parent: &str,
            fd: zbus::zvariant::Fd<'_>,
            options: HashMap<String, zbus::zvariant::OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
        ) -> zbus::zvariant::OwnedObjectPath {
            use std::io::Read;
            use std::os::fd::AsFd;
            let file = std::fs::File::from(fd.as_fd().try_clone_to_owned().unwrap());
            let mut bytes = Vec::new();
            file.take(1024).read_to_end(&mut bytes).unwrap();
            *self.0.lock().unwrap() = bytes;
            self.respond(options, header, connection).await
        }
        #[zbus(name = "OpenURI")]
        async fn open_uri(
            &self,
            _parent: &str,
            _uri: &str,
            options: HashMap<String, zbus::zvariant::OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
        ) -> zbus::zvariant::OwnedObjectPath {
            self.respond(options, header, connection).await
        }
    }
    #[tokio::test]
    async fn peer_notifications_cannot_grow_the_action_mapping_without_bound() {
        let mut paths = HashMap::new();
        for id in 0..NOTIFICATION_ACTION_LIMIT as u32 * 2 {
            remember_notification(
                &mut paths,
                id,
                ReceivedFile::from_file(
                    PathBuf::from(format!("{id}.png")),
                    tempfile::tempfile().unwrap(),
                )
                .unwrap(),
            );
            assert!(paths.len() <= NOTIFICATION_ACTION_LIMIT);
        }
        assert_eq!(paths.len(), NOTIFICATION_ACTION_LIMIT);
        assert!(!paths.contains_key(&0));
        let last = NOTIFICATION_ACTION_LIMIT as u32 * 2 - 1;
        remember_notification(
            &mut paths,
            last,
            ReceivedFile::from_file(
                PathBuf::from("replacement.png"),
                tempfile::tempfile().unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(paths.len(), NOTIFICATION_ACTION_LIMIT);
        assert_eq!(paths[&last].path(), Path::new("replacement.png"));
    }
    #[tokio::test]
    async fn live_notification_actions_survive_65th_and_closed_ids_are_removed() -> Result<()> {
        let bus_dir = tempfile::tempdir()?;
        let config = bus_dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                "<busconfig><type>session</type><listen>unix:tmpdir={}</listen><auth>EXTERNAL</auth><apparmor mode=\"disabled\"/><policy context=\"default\"><allow user=\"*\"/><allow own=\"*\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/></policy></busconfig>",
                bus_dir.path().display()
            ),
        )?;
        let mut bus = tokio::process::Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut line = String::new();
        BufReader::new(bus.stdout.take().context("bus stdout")?)
            .read_line(&mut line)
            .await?;
        let address = line.trim();
        let portal_bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let portal_code = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let deferred_response = Arc::new(std::sync::Mutex::new(None));
        let notification_hints = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let notification_actions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notification_copy = Arc::new(std::sync::Mutex::new((String::new(), String::new())));
        let issued = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let closed = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let shown = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let startup_id = Arc::new(std::sync::Mutex::new(String::new()));
        let portal_activation = Arc::new(std::sync::Mutex::new(None::<String>));
        let server = zbus::connection::Builder::address(address)?
            .name("org.freedesktop.Notifications")?
            .name("org.freedesktop.portal.Desktop")?
            .name("org.freedesktop.FileManager1")?
            .serve_at(
                "/org/freedesktop/FileManager1",
                FileManagerServer(shown.clone(), startup_id.clone()),
            )?
            .serve_at(
                "/org/freedesktop/portal/desktop",
                PortalServer(
                    portal_bytes.clone(),
                    portal_code.clone(),
                    deferred_response.clone(),
                    issued.clone(),
                    closed.clone(),
                    portal_activation.clone(),
                ),
            )?
            .serve_at(
                "/org/freedesktop/Notifications",
                NotificationServer(
                    std::sync::atomic::AtomicU32::new(0),
                    notification_hints.clone(),
                    notification_actions.clone(),
                    notification_copy.clone(),
                ),
            )?
            .build()
            .await?;
        let connection = zbus::connection::Builder::address(address)?.build().await?;
        let dir = tempfile::tempdir()?;
        let mut actions = DesktopActions::new(dir.path().to_owned());
        actions.bus = Some(connection.clone());
        let original = dir.path().join("viewer.png");
        std::fs::write(&original, b"verified viewer bytes")?;
        let file = ReceivedFile::open(&original)?;
        rustix::fs::seek(&*file.file, rustix::fs::SeekFrom::End(0))?;
        let moved = original.with_extension("moved");
        std::fs::rename(&original, &moved)?;
        std::os::unix::fs::symlink("/etc/passwd", &original)?;
        actions.open_received(&file).await?;
        assert_eq!(&*portal_bytes.lock().unwrap(), b"verified viewer bytes");
        std::fs::remove_file(&original)?;
        std::fs::rename(&moved, &original)?;
        actions.open_received(&file).await?;
        assert_eq!(&*portal_bytes.lock().unwrap(), b"verified viewer bytes");
        for n in 0..70 {
            std::fs::write(dir.path().join(format!("{n}.bin")), b"file")?;
            actions
                .notify(
                    &dir.path().join(format!("{n}.bin")),
                    "application/octet-stream",
                )
                .await?;
        }
        {
            let current = actions.notifications().await?;
            let paths = current.as_ref().unwrap().paths.lock().await;
            assert_eq!(paths.len(), 70);
            assert!(paths.contains_key(&1));
        }
        server
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "NotificationClosed",
                &(1u32, 2u32),
            )
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let current = actions.notifications().await?;
                if !current
                    .as_ref()
                    .unwrap()
                    .paths
                    .lock()
                    .await
                    .contains_key(&1)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                drop(current);
                tokio::task::yield_now().await;
            }
        })
        .await??;
        {
            let current = actions.notifications().await?;
            let paths = current.as_ref().unwrap().paths.lock().await;
            assert_eq!(paths.len(), 69);
            assert!(paths.contains_key(&2));
        }
        let image_path = dir.path().join("notification.png");
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3).write_to(&mut png, image::ImageFormat::Png)?;
        let image_bytes = png.into_inner();
        std::fs::write(&image_path, &image_bytes)?;
        let file = ReceivedFile::open(&image_path)?;
        let moved_image = image_path.with_extension("moved");
        std::fs::rename(&image_path, &moved_image)?;
        std::os::unix::fs::symlink("/etc/passwd", &image_path)?;
        actions.notify_received(&file, "image/png").await?;
        {
            let hints = notification_hints.lock().unwrap();
            assert!(hints.contains_key("image-data"));
            assert!(!hints.contains_key("image-path"));
        }
        actions.notify_copied_received(&file, "image/png").await?;
        drop(file); // Notifications own the only retained receive handle.
        for action in ["open", "show"] {
            let counter = if action == "open" { &issued } else { &shown };
            let before = counter.load(std::sync::atomic::Ordering::SeqCst);
            server
                .emit_signal(
                    None::<&str>,
                    "/org/freedesktop/Notifications",
                    "org.freedesktop.Notifications",
                    "ActionInvoked",
                    &(71u32, action),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while counter.load(std::sync::atomic::Ordering::SeqCst) == before {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            if action == "open" {
                assert_eq!(*portal_bytes.lock().unwrap(), image_bytes);
            }
        }
        std::fs::remove_file(&image_path)?;
        std::fs::rename(&moved_image, &image_path)?;

        server
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "ActionInvoked",
                &(71u32, "open"),
            )
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while *portal_bytes.lock().unwrap() != image_bytes {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        // Verify production notification hints, including replacement bursts and
        // transient clipboard notifications, against the real zbus transport.
        let final_path = dir.path().join("final.png");
        std::fs::write(&final_path, &image_bytes)?;
        let final_file = ReceivedFile::open(&final_path)?;
        // Target churn must not prevent activation of a file still retained in paths.
        for _ in 0..600 {
            actions
                .notice(
                    crate::ui::notifications::Notice::new("churn"),
                    Vec::new(),
                    None,
                    0,
                    None,
                )
                .await?;
        }
        {
            let current = actions.notifications().await?;
            let n = current.as_ref().unwrap();
            assert!(n.paths.lock().await.contains_key(&2));
            let targets = n.targets.lock().await;
            assert_eq!(targets.len(), NOTIFICATION_ACTION_LIMIT);
            assert!(!targets.contains_key(&2));
        }
        // Repeated clicks on the retained file each use a fresh one-shot token.
        for token in [Some("retained-first"), Some("retained-second"), None] {
            let before = shown.load(std::sync::atomic::Ordering::SeqCst);
            if let Some(token) = token {
                server
                    .emit_signal(
                        None::<&str>,
                        "/org/freedesktop/Notifications",
                        "org.freedesktop.Notifications",
                        "ActivationToken",
                        &(2u32, token),
                    )
                    .await?;
            }
            server
                .emit_signal(
                    None::<&str>,
                    "/org/freedesktop/Notifications",
                    "org.freedesktop.Notifications",
                    "ActionInvoked",
                    &(2u32, "show"),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while shown.load(std::sync::atomic::Ordering::SeqCst) == before {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert_eq!(&*startup_id.lock().unwrap(), token.unwrap_or_default());
        }
        // Open keeps the receive FD with no caller handle left. Show navigates
        // recorded paths, including older items in a burst.
        let retained_path = dir.path().join("1.bin");
        let burst_id = actions
            .notice_with_paths(
                crate::ui::notifications::Notice::blocked("phone"),
                vec![final_file.clone()],
                None,
                0,
                None,
                None,
                vec![final_file.path().to_owned(), retained_path.clone()],
            )
            .await?;
        std::fs::remove_file(&retained_path)?;
        std::fs::write(&retained_path, b"replacement")?;
        for (id, action) in [(2, "open"), (2, "show"), (burst_id, "show")] {
            let counter = if action == "open" { &issued } else { &shown };
            let before = counter.load(std::sync::atomic::Ordering::SeqCst);
            server
                .emit_signal(
                    None::<&str>,
                    "/org/freedesktop/Notifications",
                    "org.freedesktop.Notifications",
                    "ActionInvoked",
                    &(id, action),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while counter.load(std::sync::atomic::Ordering::SeqCst) == before {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            if action == "open" {
                assert_eq!(&*portal_bytes.lock().unwrap(), b"file");
            }
        }

        // Real D-Bus signal ordering and all notification launch destinations.
        for action in ["default", "open", "show", "link", "folder", "pair"] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(2);
            actions.set_commands(tx);
            let id = actions
                .notice(
                    crate::ui::notifications::Notice::blocked("phone"),
                    vec![final_file.clone()],
                    Some("https://example.com".into()),
                    0,
                    None,
                )
                .await?;
            let before = if action == "show" {
                shown.load(std::sync::atomic::Ordering::SeqCst)
            } else {
                issued.load(std::sync::atomic::Ordering::SeqCst)
            };
            for (member, payload) in [
                ("ActivationToken", "notification-token"),
                ("ActionInvoked", action),
            ] {
                server
                    .emit_signal(
                        None::<&str>,
                        "/org/freedesktop/Notifications",
                        "org.freedesktop.Notifications",
                        member,
                        &(id, payload),
                    )
                    .await?;
            }
            if matches!(action, "folder" | "pair") {
                let command = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                    .await?
                    .unwrap();
                match command {
                    crate::ui::Command::AddDevice { activation_token } if action == "pair" => {
                        assert_eq!(activation_token.as_deref(), Some("notification-token"));
                    }
                    crate::ui::Command::ChooseFolder if action == "folder" => {}
                    _ => panic!("wrong notification command"),
                }
            } else {
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    loop {
                        let count = if action == "show" {
                            shown.load(std::sync::atomic::Ordering::SeqCst)
                        } else {
                            issued.load(std::sync::atomic::Ordering::SeqCst)
                        };
                        if count > before {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await?;
                if action == "show" {
                    assert_eq!(&*startup_id.lock().unwrap(), "notification-token");
                } else {
                    assert_eq!(
                        portal_activation.lock().unwrap().as_deref(),
                        Some("notification-token")
                    );
                }
            }
        }
        // Retained notice targets accept fresh tokens after prior actions too;
        // a missing token cannot reuse the previous click's credential.
        let id = actions
            .notice(
                crate::ui::notifications::Notice::new("repeat actions"),
                vec![final_file.clone()],
                None,
                0,
                None,
            )
            .await?;
        for token in [None, Some("first-click"), None, Some("second-click"), None] {
            let before = shown.load(std::sync::atomic::Ordering::SeqCst);
            if let Some(token) = token {
                server
                    .emit_signal(
                        None::<&str>,
                        "/org/freedesktop/Notifications",
                        "org.freedesktop.Notifications",
                        "ActivationToken",
                        &(id, token),
                    )
                    .await?;
            }
            server
                .emit_signal(
                    None::<&str>,
                    "/org/freedesktop/Notifications",
                    "org.freedesktop.Notifications",
                    "ActionInvoked",
                    &(id, "show"),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while shown.load(std::sync::atomic::Ordering::SeqCst) == before {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert_eq!(&*startup_id.lock().unwrap(), token.unwrap_or_default());
        }
        let metadata = pneu_proto::FileBegin {
            hlc: None,
            source_device: None,
            transfer_id: pneu_proto::Id::from_bytes([7; 16]),
            file_name: "final.png".into(),
            mime: "image/png".into(),
            size: image_bytes.len() as u64,
            sha256: pneu_proto::Hash::new("0".repeat(64))?,
            kind: pneu_proto::FileKind::Screenshot,
            created_at: 0,
            source_media_id: None,
        };
        for _ in 0..72 {
            actions
                .notify_file(&final_file, &metadata, true, None, dir.path())
                .await?;
        }
        {
            let hints = notification_hints.lock().unwrap();
            assert!(hints.contains_key("x-kde-urls"));
            assert!(!hints.contains_key("image-data"));
            assert!(!hints.contains_key("image-path"));
            assert!(!hints.contains_key("transient"));
            let urls: Vec<String> = hints["x-kde-urls"].try_clone()?.try_into()?;
            assert_eq!(urls.len(), 72);
            assert_eq!(u8::try_from(&hints["urgency"])?, 1);
            assert_eq!(
                String::try_from(hints["desktop-entry"].try_clone()?)?,
                "pneu"
            );
        }
        actions.notify_text("https://example.com/").await?;
        {
            let hints = notification_hints.lock().unwrap();
            assert!(bool::try_from(&hints["transient"])?);
            assert_eq!(u8::try_from(&hints["urgency"])?, 0);
            assert!(!hints.contains_key("x-kde-urls"));
        }
        for _ in 0..128 {
            actions
                .notify_file(&final_file, &metadata, true, None, dir.path())
                .await?;
        }
        {
            let current = actions.notifications().await?;
            let targets = current.as_ref().unwrap().targets.lock().await;
            assert!(targets.values().all(|t| t.paths.len() <= 128));
            assert_eq!(actions.burst.lock().await.as_ref().unwrap().count, 72);
        }
        actions.burst.lock().await.as_mut().unwrap().started =
            std::time::Instant::now() - std::time::Duration::from_secs(31);
        actions
            .notify_file(&final_file, &metadata, true, None, dir.path())
            .await?;
        assert_eq!(actions.burst.lock().await.as_ref().unwrap().count, 1);
        // Queued jobs are dispatched back-to-back, but their arrivals govern
        // both the four-second gap and the thirty-second burst lifetime.
        *actions.burst.lock().await = None;
        let arrived = std::time::Instant::now() - std::time::Duration::from_secs(60);
        for (seconds, count) in [
            (0, 1),
            (5, 1),
            (7, 2),
            (10, 3),
            (13, 4),
            (16, 5),
            (19, 6),
            (22, 7),
            (25, 8),
            (28, 9),
            (31, 10),
            (34, 11),
            (35, 1),
            (34, 1),
        ] {
            actions
                .notify_file_at(
                    &final_file,
                    &metadata,
                    true,
                    None,
                    dir.path(),
                    arrived + std::time::Duration::from_secs(seconds),
                )
                .await?;
            assert_eq!(actions.burst.lock().await.as_ref().unwrap().count, count);
        }
        actions
            .notify_image_failure(&final_file, &metadata, false)
            .await?;
        assert!(
            !notification_actions
                .lock()
                .unwrap()
                .iter()
                .any(|id| id == "show")
        );
        actions
            .notify_image_failure(&final_file, &metadata, true)
            .await?;
        assert!(
            notification_actions
                .lock()
                .unwrap()
                .iter()
                .any(|id| id == "show")
        );
        let id = {
            let current = actions.notifications().await?;
            let targets = current.as_ref().unwrap().targets.lock().await;
            *targets.keys().max().unwrap()
        };
        portal_bytes.lock().unwrap().clear();
        server
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "ActionInvoked",
                &(id, "open"),
            )
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while *portal_bytes.lock().unwrap() != image_bytes {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        // Arrival names survive Core's descriptor handoff. Bursts never merge
        // senders, and clip summaries identify the sender even with one device.
        let source = ArrivalSource {
            device: pneu_proto::Id::from_bytes([8; 16]),
            name: "Office <PC>".into(),
            several: true,
        };
        let named = final_file.clone().with_source(source.clone());
        actions
            .notify_file(&named, &metadata, true, None, dir.path())
            .await?;
        assert!(
            notification_copy
                .lock()
                .unwrap()
                .1
                .starts_with("From Office &lt;PC&gt;")
        );
        assert_eq!(actions.burst.lock().await.as_ref().unwrap().count, 1);
        actions
            .notify_file(&named, &metadata, true, None, dir.path())
            .await?;
        assert_eq!(actions.burst.lock().await.as_ref().unwrap().count, 2);
        let other = final_file.clone().with_source(ArrivalSource {
            device: pneu_proto::Id::from_bytes([9; 16]),
            name: "Phone".into(),
            several: true,
        });
        actions
            .notify_file(&other, &metadata, true, None, dir.path())
            .await?;
        assert_eq!(actions.burst.lock().await.as_ref().unwrap().count, 1);
        actions.notify_text_from("hello", &source).await?;
        assert_eq!(
            notification_copy.lock().unwrap().0,
            "Copied from Office <PC>"
        );
        let (tx, mut retry_rx) = tokio::sync::mpsc::channel(2);
        actions.set_commands(tx);
        let mut transfer = crate::outbox::Transfer::received(source.device.clone(), &metadata);
        transfer.direction = "outgoing".into();
        transfer.state = "failed".into();
        transfer.error = Some("Update required".into());
        actions.notify_delivery(&transfer, &source.name).await?;
        assert!(
            notification_copy
                .lock()
                .unwrap()
                .0
                .starts_with("Couldn't send final.png to Office")
        );
        assert!(
            notification_actions
                .lock()
                .unwrap()
                .iter()
                .any(|a| a == "retry")
        );
        let retry_id = {
            let current = actions.notifications().await?;
            let targets = current.as_ref().unwrap().targets.lock().await;
            *targets.keys().max().unwrap()
        };
        // A sibling failure must not overwrite this notification's artifact.
        let mut sibling = transfer.clone();
        sibling.transfer_id = pneu_proto::Id::from_bytes([77; 16]);
        sibling.file_name = "sibling.png".into();
        actions.notify_delivery(&sibling, &source.name).await?;
        server
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "ActionInvoked",
                &(retry_id, "retry"),
            )
            .await?;
        let command = tokio::time::timeout(std::time::Duration::from_secs(2), retry_rx.recv())
            .await?
            .context("retry command")?;
        assert!(
            matches!(command, crate::ui::Command::RetryTransfer { operation, artifact, device } if operation == transfer.operation && artifact == transfer.transfer_id && device == transfer.device)
        );
        transfer.state = "sent".into();
        actions.notify_delivery(&transfer, &source.name).await?;
        assert_eq!(
            notification_copy.lock().unwrap().0,
            "final.png sent to Office <PC>"
        );
        assert!(bool::try_from(
            &notification_hints.lock().unwrap()["transient"]
        )?);
        // A response failure (after acceptance) falls back; cancellation never does.
        for code in [0, 1, 2] {
            portal_code.store(code, std::sync::atomic::Ordering::SeqCst);
            let fallback = std::sync::atomic::AtomicBool::new(false);
            crate::ui::portal::with_fallback(
                open_uri_portal("https://example.com", &connection, None),
                || async {
                    fallback.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await?;
            assert_eq!(
                fallback.load(std::sync::atomic::Ordering::SeqCst),
                code == 2
            );
            assert_eq!(
                open_received_file(&final_file, &connection).await.is_err(),
                code == 2
            );
        }
        // Acceptance is immediate, but an application chooser may take time.
        portal_code.store(3, std::sync::atomic::Ordering::SeqCst);
        // Finish the real D-Bus acceptance phase before advancing time. Only
        // the interactive wait is virtual; kernel I/O stays on the real clock.
        let mut viewer = begin_open_received_file(&final_file, &connection, None).await?;
        assert!(futures_util::poll!(viewer.as_mut()).is_pending());
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        let waiting = futures_util::poll!(viewer.as_mut()).is_pending();
        tokio::time::resume();
        assert!(waiting, "interactive chooser was prematurely canceled");
        let path = deferred_response.lock().unwrap().take().unwrap();
        server
            .emit_signal(
                None::<&str>,
                path.as_str(),
                "org.freedesktop.portal.Request",
                "Response",
                &(0u32, HashMap::<String, zbus::zvariant::OwnedValue>::new()),
            )
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(10), viewer).await??;
        // Leave all three interactive entry points pending: legacy Open,
        // structured Open, and Open Link. Signals and cleanup must still run.
        portal_code.store(3, std::sync::atomic::Ordering::SeqCst);
        let baseline = issued.load(std::sync::atomic::Ordering::SeqCst);
        let pending_id = actions
            .notice(
                crate::ui::notifications::Notice::blocked("phone"),
                vec![final_file.clone()],
                Some("https://example.com".into()),
                0,
                None,
            )
            .await?;
        for (id, action) in [(2, "open"), (pending_id, "open"), (pending_id, "link")]
            .into_iter()
            .chain(std::iter::repeat_n((pending_id, "open"), 10))
        {
            server
                .emit_signal(
                    None::<&str>,
                    "/org/freedesktop/Notifications",
                    "org.freedesktop.Notifications",
                    "ActionInvoked",
                    &(id, action),
                )
                .await?;
        }
        // Notifications were already initialized with no sender; resolve at dispatch.
        for action in ["pair", "folder"] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(2);
            actions.set_commands(tx); // Also exercise replacement of a previous sender.
            let id = actions
                .notice(
                    crate::ui::notifications::Notice::blocked("phone"),
                    Vec::new(),
                    None,
                    0,
                    None,
                )
                .await?;
            server
                .emit_signal(
                    None::<&str>,
                    "/org/freedesktop/Notifications",
                    "org.freedesktop.Notifications",
                    "ActionInvoked",
                    &(id, action),
                )
                .await?;
            let command = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await?
                .context("notification command")?;
            assert!(matches!(
                (action, command),
                (
                    "pair",
                    crate::ui::Command::AddDevice {
                        activation_token: None
                    }
                ) | ("folder", crate::ui::Command::ChooseFolder)
            ));
        }
        assert_eq!(
            issued.load(std::sync::atomic::Ordering::SeqCst) - baseline,
            crate::ui::portal::Interactions::LIMIT as u32,
            "limit must apply before issuing requests"
        );
        server
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "ActionInvoked",
                &(pending_id, "show"),
            )
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while shown.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        for id in [2, pending_id] {
            server
                .emit_signal(
                    None::<&str>,
                    "/org/freedesktop/Notifications",
                    "org.freedesktop.Notifications",
                    "NotificationClosed",
                    &(id, 2u32),
                )
                .await?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let current = actions.notifications().await?;
                let n = current.as_ref().unwrap();
                if !n.paths.lock().await.contains_key(&2)
                    && !n.targets.lock().await.contains_key(&pending_id)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                drop(current);
                tokio::task::yield_now().await;
            }
        })
        .await??;
        actions.shutdown().await?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while closed.load(std::sync::atomic::Ordering::SeqCst)
                < crate::ui::portal::Interactions::LIMIT as u32
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        // All request kinds share ownership-loss handling. Viewer and URI waits
        // must free their bounded slots without involving the real URI fallback.
        let mut waiting = crate::ui::portal::Interactions::new();
        let failed = Arc::new(std::sync::atomic::AtomicU32::new(0));
        for index in 0..crate::ui::portal::Interactions::LIMIT {
            let wait = if index % 2 == 0 {
                begin_open_received_file(&final_file, &connection, None).await?
            } else {
                begin_open_uri_portal("https://example.com", &connection, None).await?
            };
            let failed = failed.clone();
            waiting.push(async move {
                assert!(wait.await.is_err());
                failed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        assert!(!waiting.available());
        assert!(
            server
                .release_name("org.freedesktop.portal.Desktop")
                .await?
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while failed.load(std::sync::atomic::Ordering::SeqCst)
                != crate::ui::portal::Interactions::LIMIT as u32
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(waiting.available());
        portal_code.store(0, std::sync::atomic::Ordering::SeqCst);
        let replacement = zbus::connection::Builder::address(address)?
            .name("org.freedesktop.portal.Desktop")?
            .serve_at(
                "/org/freedesktop/portal/desktop",
                PortalServer(
                    portal_bytes.clone(),
                    portal_code,
                    deferred_response,
                    issued,
                    closed,
                    portal_activation.clone(),
                ),
            )?
            .build()
            .await?;
        open_received_file(&final_file, &connection).await?;
        open_uri_portal("https://example.com", &connection, None).await?;
        waiting.shutdown().await;
        drop(replacement);
        drop(server);
        bus.kill().await?;
        bus.wait().await?;
        Ok(())
    }
}

/// Prefer logind; screensaver is the per-session fallback. Unknown permits copy.
async fn session_locked() -> Option<bool> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        if let Ok(bus) = zbus::Connection::system().await {
            if let Ok(manager) = zbus::Proxy::new(
                &bus,
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
            )
            .await
            {
                let path: Result<zbus::zvariant::OwnedObjectPath, _> = manager
                    .call("GetSessionByPID", &(std::process::id(),))
                    .await;
                if let Ok(path) = path {
                    if let Ok(session) = zbus::Proxy::new(
                        &bus,
                        "org.freedesktop.login1",
                        path,
                        "org.freedesktop.login1.Session",
                    )
                    .await
                    {
                        if let Ok(locked) = session.get_property::<bool>("LockedHint").await {
                            return Some(locked);
                        }
                    }
                }
            }
        }
        let bus = zbus::Connection::session().await.ok()?;
        let proxy = zbus::Proxy::new(
            &bus,
            "org.freedesktop.ScreenSaver",
            "/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        )
        .await
        .ok()?;
        proxy.call::<_, _, bool>("GetActive", &()).await.ok()
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod sms_marker_tests {
    use super::*;
    #[test]
    fn sms_secret_offer_carries_history_exclusion_and_origin() -> Result<()> {
        use wl_clipboard_rs::copy::{MimeType, Source};
        let sources =
            clipboard_sources("application/x-pneu-secret-text".into(), b"482913".to_vec())?;
        assert!(sources.iter().any(|s| s.mime_type
            == MimeType::Specific("x-kde-passwordManagerHint".into())
            && matches!(&s.source,Source::Bytes(b) if b.as_ref()==b"secret")));
        assert!(
            sources.iter().any(
                |s| s.mime_type == MimeType::Specific(crate::ui::clipboard::ORIGIN_MIME.into())
            )
        );
        assert!(sources.iter().any(|s| s.mime_type
            == MimeType::Specific("text/plain;charset=utf-8".into())
            && matches!(&s.source,Source::Bytes(b) if b.as_ref()==b"482913")));
        Ok(())
    }
}
