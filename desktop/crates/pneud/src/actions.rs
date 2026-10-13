use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
};

/// An anchored received file. Reads use independent positional offsets; no
/// desktop consumer is allowed to resolve its display pathname again.
#[derive(Clone, Debug)]
pub struct ReceivedFile {
    path: PathBuf,
    file: Arc<std::fs::File>,
}
impl ReceivedFile {
    pub(crate) fn from_file(path: PathBuf, file: std::fs::File) -> Result<Self> {
        anyhow::ensure!(file.metadata()?.is_file(), "received file must be regular");
        Ok(Self {
            path,
            file: Arc::new(file),
        })
    }
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::from(rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?);
        Self::from_file(path.to_owned(), file)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn viewer_descriptor(&self) -> Result<std::fs::File> {
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
        // dup/try_clone shares the streaming cursor (usually at EOF). Reopen
        // through our still-owned descriptor, never through the received path,
        // to give each portal launch a read-only cursor starting at byte zero.
        let descriptor = std::fs::File::from(rustix::fs::open(
            format!("/proc/self/fd/{}", self.file.as_raw_fd()),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?);
        let original = self.file.metadata()?;
        let opened = descriptor.metadata()?;
        anyhow::ensure!(
            opened.is_file() && (opened.dev(), opened.ino()) == (original.dev(), original.ino()),
            "viewer descriptor changed identity"
        );
        Ok(descriptor)
    }
    pub fn read_bounded(&self, limit: usize) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        anyhow::ensure!(
            self.file.metadata()?.len() <= limit as u64,
            "input exceeds bounded read limit"
        );
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        while bytes.len() <= limit {
            let cap = buffer.len().min(limit + 1 - bytes.len());
            let n = self.file.read_at(&mut buffer[..cap], bytes.len() as u64)?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        anyhow::ensure!(bytes.len() <= limit, "input grew beyond bounded read limit");
        Ok(bytes)
    }
    pub(crate) fn is_image(&self, mime: &str) -> Result<bool> {
        use std::os::unix::fs::FileExt;
        let mut header = [0; 32];
        let n = self.file.read_at(&mut header, 0)?;
        let bytes = &header[..n];
        Ok(match mime {
            "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
            "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
            "image/bmp" => bytes.starts_with(b"BM"),
            _ => false,
        })
    }
}

#[async_trait]
pub trait PcActions: Send + Sync {
    async fn clipboard_text(&self, text: &str) -> Result<()>;
    async fn clipboard_image(&self, path: &Path, mime: &str) -> Result<()>;
    async fn notify(&self, path: &Path, mime: &str) -> Result<()>;
    async fn open(&self, path: &Path) -> Result<()>;
    async fn notify_clipboard_image(&self, path: &Path, mime: &str) -> Result<()> {
        self.notify(path, mime).await
    }
    // Defaults keep lightweight integrations usable; the desktop backend consumes
    // descriptors for every data-bearing action and notification-button handoff.
    async fn clipboard_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        self.clipboard_image(file.path(), mime).await
    }
    async fn notify_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        self.notify(file.path(), mime).await
    }
    async fn notify_copied_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        self.notify_clipboard_image(file.path(), mime).await
    }
    async fn open_received(&self, file: &ReceivedFile) -> Result<()> {
        self.open(file.path()).await
    }
    /// Issue a viewer request promptly. Human interaction belongs exclusively
    /// to the returned future. Noninteractive backends can use this default.
    async fn begin_open_received(
        &self,
        file: &ReceivedFile,
    ) -> Result<crate::ui::portal::Interaction> {
        self.open_received(file).await?;
        Ok(Box::pin(async { Ok(()) }))
    }
    async fn notify_clipboard_failure(&self) -> Result<()> {
        Ok(())
    }
    async fn notify_file(
        &self,
        file: &ReceivedFile,
        metadata: &pneu_proto::FileBegin,
        _saved: bool,
        _save_error: Option<pneu_proto::NackCode>,
        _folder: &Path,
    ) -> Result<()> {
        self.notify_received(file, &metadata.mime).await
    }
    /// Queued callers carry the transfer arrival so backend grouping is
    /// independent of time spent on earlier desktop effects.
    async fn notify_file_at(
        &self,
        file: &ReceivedFile,
        metadata: &pneu_proto::FileBegin,
        saved: bool,
        save_error: Option<pneu_proto::NackCode>,
        folder: &Path,
        _arrived: std::time::Instant,
    ) -> Result<()> {
        self.notify_file(file, metadata, saved, save_error, folder)
            .await
    }
    async fn notify_image_failure(
        &self,
        _file: &ReceivedFile,
        _metadata: &pneu_proto::FileBegin,
        _saved: bool,
    ) -> Result<()> {
        self.notify_clipboard_failure().await
    }
    async fn notify_text_failure(&self) -> Result<()> {
        Ok(())
    }
    async fn notify_save_failure(
        &self,
        _metadata: &pneu_proto::FileBegin,
        _code: pneu_proto::NackCode,
        _destination: crate::ui::ReceiveDestination,
        _folder: &Path,
    ) -> Result<()> {
        Ok(())
    }
    async fn notify_text(&self, _text: &str) -> Result<()> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
/// Desktop integration owns its clipboard helper and one D-Bus signal task.
/// wl-clipboard-rs owns the selection bytes; large images are capped to bound
/// its unavoidable buffers. The helper then serves the
/// data-control selection without a fork from a multithreaded Tokio process.
pub struct DesktopActions {
    clipboard: Mutex<Option<Child>>,
    clipboard_gate: Mutex<()>,
    cache: PathBuf,
    notifications: Mutex<Option<Notifications>>,
    jobs: Arc<crate::work::Work>,
    pub echo: Arc<crate::ui::clipboard::EchoGuard>,
    commands: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<crate::ui::Command>>>>,
    copy_results: Mutex<HashMap<PathBuf, bool>>,
    burst: Mutex<Option<Burst>>,
    #[cfg(test)]
    bus: Option<zbus::Connection>,
    #[cfg(test)]
    helper: Option<PathBuf>,
}
#[derive(Clone)]
struct Target {
    files: Vec<ReceivedFile>,
    urls: Vec<String>,
    link: Option<String>,
}
struct Burst {
    id: u32,
    urls: Vec<String>,
    kind: Option<pneu_proto::FileKind>,
    count: usize,
    arrived: std::time::Instant,
    started: std::time::Instant,
    saved: bool,
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
            clipboard: Mutex::new(None),
            clipboard_gate: Mutex::new(()),
            cache,
            notifications: Mutex::new(None),
            jobs: Arc::new(crate::work::Work::default()),
            echo: Arc::new(crate::ui::clipboard::EchoGuard::default()),
            commands: Arc::new(std::sync::Mutex::new(None)),
            copy_results: Mutex::new(HashMap::new()),
            burst: Mutex::new(None),
            #[cfg(test)]
            bus: None,
            #[cfg(test)]
            helper: None,
        }
    }
    pub fn set_commands(&self, commands: tokio::sync::mpsc::Sender<crate::ui::Command>) {
        *self.commands.lock().expect("UI commands") = Some(commands);
    }
    async fn copy(
        &self,
        file: Option<&ReceivedFile>,
        text: Option<&str>,
        mime: &str,
    ) -> Result<()> {
        let _guard = self.clipboard_gate.lock().await;
        let bytes = if let Some(file) = file {
            let file = file.clone();
            Some(
                crate::work::start_blocking(vec![self.jobs.clone()], move || {
                    file.read_bounded(16 * 1024 * 1024)
                })
                .await??,
            )
        } else {
            None
        };
        use sha2::{Digest, Sha256};
        // Prepare the candidate hashes without altering the successful selection.
        let mut hashes = Vec::new();
        if let Some(bytes) = &bytes {
            hashes.push(Sha256::digest(bytes).into());
        }
        if let Some(text) = text {
            hashes.push(Sha256::digest(text.as_bytes()).into());
        }
        let executable = std::env::current_exe()?;
        #[cfg(test)]
        let executable = self.helper.clone().unwrap_or(executable);
        let mut command = Command::new(executable);
        command
            .arg("clipboard-helper")
            .arg(mime)
            .env("TMPDIR", &self.cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let mut input = child.stdin.take().context("helper stdin")?;
        if let Some(bytes) = bytes {
            input.write_all(&bytes).await?;
        }
        if let Some(text) = text {
            input.write_all(text.as_bytes()).await?;
        }
        drop(input);
        let mut output = BufReader::new(child.stdout.take().context("helper stdout")?.take(256));
        let mut line = String::new();
        let ready = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            output.read_line(&mut line),
        )
        .await;
        if !matches!(ready, Ok(Ok(n)) if n > 0) || line.split_whitespace().next() != Some("READY") {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!("Wayland data-control clipboard unavailable");
        }
        for hash in line.split_whitespace().skip(1) {
            let mut digest = [0; 32];
            if hex::decode_to_slice(hash, &mut digest).is_ok() {
                hashes.push(digest);
            }
        }
        self.echo.replace_hashes(hashes);
        if let Some(mut previous) = self.clipboard.lock().await.replace(child) {
            let _ = previous.kill().await;
            let _ = previous.wait().await;
        }
        Ok(())
    }
    async fn preview(
        &self,
        file: &ReceivedFile,
    ) -> Result<(i32, i32, i32, bool, i32, i32, Vec<u8>)> {
        let file = file.clone();
        crate::work::start_blocking(vec![self.jobs.clone()], move || {
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
                                        "default"|"open" => { if interactions.available() && let Some(file)=target.files.first() && let Ok(wait)=begin_open_received_file(file,&action_connection,token.as_deref()).await { interactions.push(async move { let _=wait.await; }); } },
                                        "show" => { let _=show_files(&target.urls,&action_connection,token.as_deref()).await; },
                                        "link" => { if interactions.available() && let Some(link)=target.link { let wait=begin_open_uri(&link,&action_connection,token.as_deref()).await; interactions.push(async move { let _=wait.await; }); } },
                                        "folder"|"pair" => { if let Some(commands)=commands.lock().expect("UI commands").clone() { let _=commands.try_send(if action=="folder" { crate::ui::Command::ChooseFolder } else { crate::ui::Command::Window { activation_token: token } }); } },
                                        _=>{},
                                    }
                                    continue;
                                }
                                if let Some(path)=path {match action.as_str() {"default"|"open"=>{if interactions.available() && let Ok(wait)=begin_open_received_file(&path, &action_connection,token.as_deref()).await { interactions.push(async move { let _=wait.await; }); }},"show"=>{if let Ok(uri)=file_uri(path.path()) { let _=show_files(&[uri], &action_connection,token.as_deref()).await; }},_=>{}}}
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
        let current = self.notifications().await?;
        let n = current.as_ref().context("notification proxy")?;
        let mut hints = HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        hints.insert("desktop-entry", zbus::zvariant::Value::from("pneu"));
        hints.insert("urgency", zbus::zvariant::Value::from(notice.urgency));
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
        targets.insert(id, Target { files, urls, link });
        Ok(id)
    }
}
#[async_trait]
impl PcActions for DesktopActions {
    async fn clipboard_text(&self, text: &str) -> Result<()> {
        self.copy(None, Some(text), "text/plain;charset=utf-8")
            .await
    }
    async fn clipboard_image(&self, path: &Path, mime: &str) -> Result<()> {
        self.clipboard_received(&ReceivedFile::open(path)?, mime)
            .await
    }
    async fn clipboard_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        let result = self.copy(Some(file), None, mime).await;
        let mut copies = self.copy_results.lock().await;
        if copies.len() >= 256 {
            copies.clear();
        }
        copies.insert(file.path().to_owned(), result.is_ok());
        result
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
                    "Image copied from phone",
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
            crate::ui::notifications::Notice::file(
                metadata,
                saved,
                false,
                true,
                None,
                "",
                crate::ui::ReceiveDestination::Cache,
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
        let mut urls = vec![file_uri(file.path())?];
        let mut replaces = 0;
        let mut count = 1;
        let mut kind = Some(metadata.kind);
        let mut all_saved = saved;
        let mut started = arrived;
        if save_error.is_none()
            && copied != Some(false)
            && let Some(previous) = burst.as_ref()
            && previous.accepts(arrived)
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
                urls.extend(previous.urls.iter().cloned());
                notice = crate::ui::notifications::Notice::burst(
                    count,
                    kind,
                    copied == Some(true),
                    all_saved,
                );
            }
        }
        notice.urls = urls.clone();
        // Burst folder actions need every pathname, but only the latest file's
        // descriptor is needed for Open. Avoid retaining one FD per arrival.
        let id = self
            .notice(notice, vec![file.clone()], None, replaces, None)
            .await?;
        *burst = if save_error.is_none() && copied != Some(false) {
            Some(Burst {
                id,
                urls,
                kind,
                count,
                arrived,
                started,
                saved: all_saved,
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
        let _guard = self.clipboard_gate.lock().await;
        if let Some(mut child) = self.clipboard.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
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
async fn begin_open_received_file(
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
fn decode_image(bytes: &[u8]) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    Ok(reader.decode()?)
}
fn clipboard_sources(
    mime: String,
    bytes: Vec<u8>,
) -> Result<Vec<wl_clipboard_rs::copy::MimeSource>> {
    use wl_clipboard_rs::copy::{MimeSource, MimeType, Source};
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
pub fn clipboard_helper(mime: String) -> Result<()> {
    use std::io::{Read, Write};
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "clipboard helper input too large"
    );
    let mut options = wl_clipboard_rs::copy::Options::new();
    options.foreground(true);
    use sha2::{Digest, Sha256};
    let sources = clipboard_sources(mime, bytes)?;
    let hashes = sources
        .iter()
        .filter_map(|source| {
            if source.mime_type
                == wl_clipboard_rs::copy::MimeType::Specific(
                    crate::ui::clipboard::ORIGIN_MIME.into(),
                )
            {
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
    let selection = options.prepare_copy_multi(sources)?;
    println!("READY {hashes}");
    std::io::stdout().flush()?;
    selection.serve()?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn burst_membership_uses_arrival_gaps_span_order_and_count() {
        use std::time::{Duration, Instant};
        let started = Instant::now() - Duration::from_secs(60);
        let mut burst = Burst {
            id: 1,
            urls: vec![],
            kind: None,
            count: 1,
            arrived: started,
            started,
            saved: true,
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
    async fn echo_guard_commits_successful_selection_and_keeps_it_on_every_failure() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let mut actions = DesktopActions::new(dir.path().to_owned());
        let helper = dir.path().join("helper");
        use sha2::{Digest, Sha256};
        let converted = hex::encode(Sha256::digest(b"converted image"));
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh
cat >/dev/null
printf 'READY {converted}\n'
exec sleep 60
"
            ),
        )?;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))?;
        actions.helper = Some(helper);
        actions.echo.remember(b"previous");
        actions
            .copy(None, Some("new successful text"), "text/plain")
            .await?;
        assert!(!actions.echo.contains(b"previous"));
        assert!(actions.echo.contains(b"new successful text"));
        assert!(actions.echo.contains(b"converted image"));
        for helper in [
            PathBuf::from("/no/such/helper"),
            PathBuf::from("/bin/false"),
        ] {
            actions.helper = Some(helper);
            assert!(
                actions
                    .copy(None, Some("failed local text"), "text/plain")
                    .await
                    .is_err()
            );
            let path = dir.path().join("image");
            std::fs::write(&path, b"failed image")?;
            assert!(
                actions
                    .copy(Some(&ReceivedFile::open(&path)?), None, "image/png")
                    .await
                    .is_err()
            );
            assert!(!actions.echo.contains(b"failed local text"));
            assert!(!actions.echo.contains(b"failed image"));
            assert!(actions.echo.contains(b"new successful text"));
            assert!(actions.echo.contains(b"converted image"));
        }
        actions.shutdown().await?;
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
    );
    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl NotificationServer {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            _app: &str,
            _replaces: u32,
            _icon: &str,
            _summary: &str,
            _body: &str,
            actions: Vec<String>,
            hints: HashMap<String, zbus::zvariant::OwnedValue>,
            _timeout: i32,
        ) -> u32 {
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
        std::fs::remove_file(&original)?;
        std::os::unix::fs::symlink("/etc/passwd", &original)?;
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
        std::fs::remove_file(&image_path)?;
        std::os::unix::fs::symlink("/etc/passwd", &image_path)?;
        actions.notify_received(&file, "image/png").await?;
        {
            let hints = notification_hints.lock().unwrap();
            assert!(hints.contains_key("image-data"));
            assert!(!hints.contains_key("image-path"));
        }
        actions.notify_copied_received(&file, "image/png").await?;
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
                    crate::ui::Command::Window { activation_token } if action == "pair" => {
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
            assert!(targets.values().all(|t| t.urls.len() <= 128));
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
        let file = final_file.clone();
        let viewer_bus = connection.clone();
        let viewer = tokio::spawn(async move { open_received_file(&file, &viewer_bus).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while deferred_response.lock().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        assert!(
            !viewer.is_finished(),
            "interactive chooser was prematurely canceled"
        );
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
        tokio::time::timeout(std::time::Duration::from_secs(2), viewer).await???;
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
                    crate::ui::Command::Window {
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
