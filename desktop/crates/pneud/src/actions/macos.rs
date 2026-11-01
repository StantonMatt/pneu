//! Daemon-side native effects. The UI child never owns Core, pairing authority,
//! clipboard admission, or received-file path resolution.
use super::*;
use crate::Core;
use crate::ui::{
    clipboard::Offer,
    macos::bridge::{self, Notification, Request},
    notifications::Notice,
};
use crate::ui::{notice_lifecycle::Lifecycle, portal::Interaction};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::Mutex;

#[derive(Clone)]
struct Target {
    files: Vec<ReceivedFile>,
    paths: Vec<PathBuf>,
    url: Option<String>,
    retry: Option<(pneu_proto::Id, pneu_proto::Id, pneu_proto::Id)>,
    actions: Vec<String>,
    device: Option<pneu_proto::Id>,
    setup_requests: Vec<crate::ui::window_setup::Decision>,
}
struct NotificationRecord {
    target: Target,
    life: Lifecycle,
    notification: Notification,
}
impl NotificationRecord {
    fn withdraw(&self) {
        self.life.cancel();
        let n = &self.notification;
        // Begin enqueues immediately, even though this best-effort cleanup has
        // no caller waiting for acknowledgement. Session health owns its deadline.
        if let Ok(wait) = bridge::begin(
            Request::Withdraw {
                id: n.id,
                namespace: n.namespace.clone(),
                revision: n.revision.clone(),
            },
            None,
        ) {
            tokio::spawn(async move {
                let _ = wait.await;
            });
        }
    }
}
struct Submission {
    registry: Arc<std::sync::Mutex<BTreeMap<u32, Arc<NotificationRecord>>>>,
    record: Arc<NotificationRecord>,
    acknowledged: bool,
}
impl Drop for Submission {
    fn drop(&mut self) {
        if self.acknowledged {
            return;
        }
        let mut registry = self.registry.lock().expect("notification targets");
        let id = self.record.notification.id;
        if registry
            .get(&id)
            .is_some_and(|r| Arc::ptr_eq(r, &self.record))
        {
            registry.remove(&id);
        }
        self.record.withdraw();
    }
}
struct Burst {
    id: u32,
    paths: Vec<PathBuf>,
    source: Option<ArrivalSource>,
    kind: Option<pneu_proto::FileKind>,
    count: usize,
    saved: bool,
    started: std::time::Instant,
    arrived: std::time::Instant,
}
pub struct DesktopActions {
    jobs: Arc<crate::work::Work>,
    next: AtomicU32,
    namespace: String,
    targets: Arc<std::sync::Mutex<BTreeMap<u32, Arc<NotificationRecord>>>>,
    copied: Mutex<HashMap<PathBuf, bool>>,
    burst: Mutex<Option<Burst>>,
}
impl DesktopActions {
    pub fn new(_cache: PathBuf) -> Self {
        Self {
            jobs: Arc::new(crate::work::Work::default()),
            next: AtomicU32::new(1),
            namespace: crate::random_hex(16),
            targets: Arc::default(),
            copied: Mutex::new(HashMap::new()),
            burst: Mutex::new(None),
        }
    }
    pub(crate) async fn withdraw_notice(&self, id: u32) -> Result<()> {
        if let Some(record) = self
            .targets
            .lock()
            .expect("notification targets")
            .remove(&id)
        {
            record.withdraw();
        }
        Ok(())
    }
    pub async fn notice(
        &self,
        notice: Notice,
        files: Vec<ReceivedFile>,
        url: Option<String>,
        replaces: u32,
        _preview: Option<&ReceivedFile>,
    ) -> Result<u32> {
        let (id, wait) = self.issue_notice(notice, files, url, replaces).await?;
        wait.await?;
        Ok(id)
    }
    async fn issue_notice(
        &self,
        notice: Notice,
        files: Vec<ReceivedFile>,
        url: Option<String>,
        replaces: u32,
    ) -> Result<(u32, Interaction)> {
        let paths = files.iter().map(|f| f.path().to_owned()).collect();
        self.issue_notice_target(
            notice,
            Target {
                files,
                paths,
                url,
                retry: None,
                actions: Vec::new(),
                device: None,
                setup_requests: Vec::new(),
            },
            replaces,
        )
        .await
    }
    async fn issue_notice_target(
        &self,
        mut notice: Notice,
        mut target: Target,
        replaces: u32,
    ) -> Result<(u32, Interaction)> {
        if std::env::var_os("PNEU_UI_TEST").is_some() && !notice.summary.starts_with("Pneu test") {
            notice.summary = format!("Pneu test: {}", notice.summary);
        }
        let id = if replaces == 0 {
            self.next.fetch_add(1, Ordering::Relaxed)
        } else {
            replaces
        };
        anyhow::ensure!(id != 0, "notification identifier exhausted");
        target.actions = notice.actions.iter().map(|(id, _)| (*id).into()).collect();
        target.device = notice.device.clone();
        target.setup_requests = notice.setup_requests.clone();
        let notification = Notification {
            id,
            namespace: self.namespace.clone(),
            revision: crate::random_hex(16),
            transient: notice.transient,
            expires: notice.expires,
            issued_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as u64,
            summary: notice.summary,
            body: notice.body,
            actions: notice
                .actions
                .into_iter()
                .map(|(id, label)| (id.into(), label.into()))
                .collect(),
        };
        let record = Arc::new(NotificationRecord {
            life: Lifecycle::new(notification.lifetime()),
            target,
            notification: notification.clone(),
        });
        {
            let mut registry = self.targets.lock().expect("notification targets");
            if let Some(previous) = registry.remove(&id) {
                previous.withdraw();
            }
            while registry.len() >= 128 {
                if let Some((_, oldest)) = registry.pop_first() {
                    oldest.withdraw();
                }
            }
            registry.insert(id, record.clone());
        }
        // This guard exists before any fallible issuance or await. Dropping the
        // caller therefore removes its descriptor target and cancels submission.
        let guard = Submission {
            registry: self.targets.clone(),
            record: record.clone(),
            acknowledged: false,
        };
        let wait = bridge::begin(Request::Notify(notification), None)?;
        if let Some(deadline) = record.life.deadline {
            let registry = Arc::downgrade(&self.targets);
            let record = Arc::downgrade(&record);
            tokio::spawn(async move {
                tokio::time::sleep_until(deadline).await;
                if let (Some(registry), Some(record)) = (registry.upgrade(), record.upgrade()) {
                    let mut registry = registry.lock().expect("notification targets");
                    if registry.get(&id).is_some_and(|r| Arc::ptr_eq(r, &record)) {
                        registry.remove(&id);
                    }
                    record.withdraw();
                }
            });
        }
        Ok((
            id,
            Box::pin(async move {
                let mut guard = guard;
                wait.await?;
                anyhow::ensure!(
                    guard.record.life.acknowledge(),
                    "notification cancelled or expired"
                );
                guard.acknowledged = true;
                Ok(())
            }),
        ))
    }

    fn begin_viewer(
        &self,
        file: &ReceivedFile,
        preview: bool,
    ) -> Result<crate::ui::portal::Interaction> {
        bridge::begin(
            Request::Open {
                name: file
                    .path()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                preview,
            },
            Some(file.viewer_descriptor()?),
        )
    }
    pub(crate) async fn clear_notifications(&self) {
        for (_, record) in std::mem::take(&mut *self.targets.lock().expect("notification targets"))
        {
            record.withdraw();
        }
        self.burst.lock().await.take();
    }
    pub(crate) fn clear_stale_native_notifications(&self) -> Result<Interaction> {
        bridge::begin(
            Request::ClearStale {
                namespace: self.namespace.clone(),
            },
            None,
        )
    }
    async fn take_notification(&self, id: u32, namespace: &str, action: &str) -> Result<Target> {
        let (session, revision) = namespace
            .rsplit_once('-')
            .context("notification has no revision")?;
        anyhow::ensure!(
            session == self.namespace,
            "notification belongs to another daemon session"
        );
        let mut targets = self.targets.lock().expect("notification targets");
        let target = targets
            .get(&id)
            .context("unknown or expired notification")?;
        anyhow::ensure!(target.life.active(), "notification expired or cancelled");
        anyhow::ensure!(
            target.notification.revision == revision,
            "notification was replaced"
        );
        anyhow::ensure!(
            action == "dismiss" || target.target.actions.iter().any(|a| a == action),
            "action not offered by notification"
        );
        let record = targets.remove(&id).expect("validated notification target");
        record.withdraw();
        Ok(record.target.clone())
    }
    pub(crate) async fn notification_action(
        &self,
        core: &Core,
        id: u32,
        namespace: &str,
        action: &str,
    ) -> Result<()> {
        let target = self.take_notification(id, namespace, action).await?;
        if action == "dismiss" {
            return Ok(());
        }
        // Safe attribution evidence contains identifiers, never clipboard text,
        // paths, pairing links, or notification bodies.
        eprintln!("pneud: notification action notification={id} action={action}");
        match action {
            "allow" | "deny" => {
                crate::ui::window_setup::decide(core, &target.setup_requests, action == "allow")
                    .await
            }
            "default" | "open" => {
                if let Some(file) = target.files.last() {
                    self.begin_viewer(file, false)?.await?;
                }
            }
            "show" => {
                bridge::begin(
                    Request::Reveal {
                        paths: target.paths,
                    },
                    None,
                )?
                .await?
            }
            "link" => {
                if let Some(url) = target.url {
                    bridge::begin(Request::OpenUrl(url), None)?.await?;
                }
            }
            "folder" => core.request_window(true),
            "pair" => core.request_add_device(),
            "remove" => {
                if let Some(device) = target.device {
                    core.request_remove_window(device, None);
                }
            }
            "retry" => {
                if let Some((operation, artifact, device)) = target.retry {
                    core.retry_transfer_artifact(operation, Some(artifact), device)
                        .await?;
                }
            }
            _ => anyhow::bail!("unsupported notification action"),
        }
        Ok(())
    }
}
#[async_trait]
impl PcActions for DesktopActions {
    async fn ui_test_notification(&self, transient: bool) -> Result<u32> {
        if transient {
            let mut notice = Notice::text("https://example.com/Pneu-test");
            notice.summary = "Pneu test: transient notification".into();
            return self
                .notice(
                    notice,
                    Vec::new(),
                    Some("https://example.com/Pneu-test".into()),
                    0,
                    None,
                )
                .await;
        }
        let mut notice = Notice::new("Pneu test: notification action");
        notice.body = "Choose Open Pneu to verify notification and action attribution.".into();
        notice.actions = vec![("pair", "Open Pneu")];
        self.notice(notice, Vec::new(), None, 0, None).await
    }

    async fn shutdown(&self) -> Result<()> {
        self.jobs.drain().await;
        self.clear_notifications().await;
        Ok(())
    }
    async fn clipboard_text(&self, _text: &str) -> Result<()> {
        anyhow::bail!("macOS clipboard writes require an actor install ticket")
    }
    async fn clipboard_image(&self, _path: &Path, _mime: &str) -> Result<()> {
        anyhow::bail!("macOS clipboard writes require an actor install ticket")
    }
    async fn clipboard_received(&self, _file: &ReceivedFile, _mime: &str) -> Result<()> {
        anyhow::bail!("macOS clipboard writes require an actor install ticket")
    }
    async fn prepare_clipboard(
        &self,
        input: ClipboardInput,
        ticket: u128,
    ) -> Result<PreparedClipboard> {
        self.prepare_clipboard_sensitive(input, ticket, false).await
    }
    async fn prepare_clipboard_sensitive(
        &self,
        input: ClipboardInput,
        ticket: u128,
        sensitive: bool,
    ) -> Result<PreparedClipboard> {
        crate::work::blocking(self.jobs.clone(), move || {
            let native = Offer::prepare_sensitive(&input, ticket, sensitive)?;
            Ok(PreparedClipboard {
                hashes: native.hashes(),
                marker_confirmation: true,
                input,
                native: Some(native),
                #[cfg(test)]
                test_exited: false,
            })
        })
        .await?
    }
    async fn install_clipboard(&self, prepared: &mut PreparedClipboard) -> Result<()> {
        prepared
            .native
            .as_ref()
            .context("missing native clipboard offer")?
            .install()
    }
    async fn record_clipboard_copy(&self, file: &ReceivedFile) {
        let mut copied = self.copied.lock().await;
        if copied.len() >= 256 {
            copied.clear();
        }
        copied.insert(file.path().to_owned(), true);
    }
    async fn notify(&self, _path: &Path, _mime: &str) -> Result<()> {
        anyhow::bail!("macOS notifications require a received descriptor")
    }
    async fn notify_clipboard_image(&self, _path: &Path, _mime: &str) -> Result<()> {
        anyhow::bail!("macOS notifications require a received descriptor")
    }
    async fn notify_received(&self, file: &ReceivedFile, _mime: &str) -> Result<()> {
        let mut notice = Notice::new(
            file.path()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
        );
        notice.actions = vec![
            ("default", "Open"),
            ("open", "Open"),
            ("show", "Show in Folder"),
        ];
        self.notice(
            file.notice(notice, false),
            vec![file.clone()],
            None,
            0,
            None,
        )
        .await
        .map(|_| ())
    }
    async fn notify_copied_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        self.begin_notify_copied_received(file, mime).await?.await
    }
    async fn begin_notify_copied_received(
        &self,
        file: &ReceivedFile,
        _mime: &str,
    ) -> Result<Interaction> {
        self.copied.lock().await.remove(file.path());
        let mut notice = Notice::new("Image copied from phone");
        notice.body = "Copied to clipboard".into();
        notice.actions = vec![
            ("default", "Open"),
            ("open", "Open"),
            ("show", "Show in Folder"),
        ];
        notice.transient = true;
        notice.expires = 4000;
        self.issue_notice(file.notice(notice, true), vec![file.clone()], None, 0)
            .await
            .map(|(_, wait)| wait)
    }
    async fn notify_text(&self, text: &str) -> Result<()> {
        self.notice(
            Notice::text(text),
            Vec::new(),
            crate::ui::notifications::single_url(text),
            0,
            None,
        )
        .await
        .map(|_| ())
    }
    async fn notify_text_from(&self, text: &str, source: &ArrivalSource) -> Result<()> {
        self.begin_notify_text_from(text, source).await?.await
    }
    async fn begin_notify_text_from(
        &self,
        text: &str,
        source: &ArrivalSource,
    ) -> Result<Interaction> {
        self.issue_notice(
            Notice::text(text).copied_from(&source.name),
            Vec::new(),
            crate::ui::notifications::single_url(text),
            0,
        )
        .await
        .map(|(_, wait)| wait)
    }
    async fn notify_text_failure(&self) -> Result<()> {
        let mut n = Notice::new("Copied from phone");
        n.body = "Couldn't copy to clipboard".into();
        self.notice(n, Vec::new(), None, 0, None).await.map(|_| ())
    }
    async fn notify_text_failure_from(&self, source: &ArrivalSource) -> Result<()> {
        self.begin_notify_text_failure_from(source).await?.await
    }
    async fn begin_notify_text_failure_from(&self, source: &ArrivalSource) -> Result<Interaction> {
        let mut n = Notice::new("Copied from phone").copied_from(&source.name);
        n.body = "Couldn't copy to clipboard".into();
        self.issue_notice(n, Vec::new(), None, 0)
            .await
            .map(|(_, wait)| wait)
    }
    async fn notify_clipboard_failure(&self) -> Result<()> {
        self.notice(
            Notice::new("Couldn't copy image"),
            Vec::new(),
            None,
            0,
            None,
        )
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
        let mut n = Notice::file(
            metadata,
            false,
            false,
            false,
            Some(code),
            &crate::ui::display_path(folder),
            destination,
        );
        n.actions
            .retain(|(id, _)| !matches!(*id, "default" | "open"));
        self.notice(n, Vec::new(), None, 0, None).await.map(|_| ())
    }
    async fn notify_save_failure_from(
        &self,
        metadata: &pneu_proto::FileBegin,
        code: pneu_proto::NackCode,
        destination: crate::ui::ReceiveDestination,
        folder: &Path,
        source: &ArrivalSource,
    ) -> Result<()> {
        self.begin_notify_save_failure_from(metadata, code, destination, folder, source)
            .await?
            .await
    }
    async fn begin_notify_save_failure_from(
        &self,
        metadata: &pneu_proto::FileBegin,
        code: pneu_proto::NackCode,
        destination: crate::ui::ReceiveDestination,
        folder: &Path,
        source: &ArrivalSource,
    ) -> Result<Interaction> {
        let mut n = source.file_notice(
            Notice::file(
                metadata,
                false,
                false,
                false,
                Some(code),
                &crate::ui::display_path(folder),
                destination,
            ),
            false,
        );
        n.actions
            .retain(|(id, _)| !matches!(*id, "default" | "open"));
        self.issue_notice(n, Vec::new(), None, 0)
            .await
            .map(|(_, wait)| wait)
    }
    async fn notify_clipboard_too_large(&self) -> Result<()> {
        self.notice(
            crate::ui::notifications::Notice::image_too_large(),
            Vec::new(),
            None,
            0,
            None,
        )
        .await
        .map(|_| ())
    }
    async fn notify_send_admission_failure(&self, reason: &str) -> Result<()> {
        let mut n = Notice::new("Couldn't send files");
        n.body = crate::ui::notifications::escape(reason);
        self.notice(n, Vec::new(), None, 0, None).await.map(|_| ())
    }
    async fn notify_delivery(
        &self,
        transfer: &crate::outbox::Transfer,
        device_name: &str,
    ) -> Result<()> {
        let (notice, retry) = if transfer.state == "sent" {
            (
                Notice::sent(&transfer.file_name, &[device_name.into()]),
                None,
            )
        } else {
            (
                Notice::send_failed(
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
        let (_, wait) = self
            .issue_notice_target(
                notice,
                Target {
                    files: Vec::new(),
                    paths: Vec::new(),
                    url: None,
                    retry,
                    actions: Vec::new(),
                    device: None,
                    setup_requests: Vec::new(),
                },
                0,
            )
            .await?;
        wait.await
    }
    async fn notify_image_failure(
        &self,
        file: &ReceivedFile,
        metadata: &pneu_proto::FileBegin,
        saved: bool,
    ) -> Result<()> {
        self.begin_notify_image_failure(file, metadata, saved)
            .await?
            .await
    }
    async fn begin_notify_image_failure(
        &self,
        file: &ReceivedFile,
        metadata: &pneu_proto::FileBegin,
        saved: bool,
    ) -> Result<Interaction> {
        self.copied.lock().await.remove(file.path());
        let n = file.notice(
            Notice::file(
                metadata,
                saved,
                false,
                true,
                None,
                "",
                crate::ui::ReceiveDestination::Cache,
            ),
            metadata.kind == pneu_proto::FileKind::ClipboardImage,
        );
        self.issue_notice(n, vec![file.clone()], None, 0)
            .await
            .map(|(_, wait)| wait)
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
        self.begin_notify_file_at(file, metadata, saved, save_error, folder, arrived)
            .await?
            .await
    }
    async fn begin_notify_file_at(
        &self,
        file: &ReceivedFile,
        metadata: &pneu_proto::FileBegin,
        saved: bool,
        save_error: Option<pneu_proto::NackCode>,
        folder: &Path,
        arrived: std::time::Instant,
    ) -> Result<Interaction> {
        let copied = self.copied.lock().await.remove(file.path());
        let mut notice = Notice::file(
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
            && previous.source == file.source
            && arrived.saturating_duration_since(previous.arrived)
                < std::time::Duration::from_secs(2)
            && arrived.saturating_duration_since(previous.started)
                < std::time::Duration::from_secs(10)
            && previous.count < 50
            && self
                .targets
                .lock()
                .expect("notification targets")
                .contains_key(&previous.id)
        {
            replaces = previous.id;
            count = previous.count + 1;
            started = previous.started;
            kind = if previous.kind == kind { kind } else { None };
            all_saved = saved && previous.saved;
            paths.extend(previous.paths.iter().cloned());
            notice = Notice::burst(count, kind, copied == Some(true), all_saved);
        }
        notice = file.notice(
            notice,
            metadata.kind == pneu_proto::FileKind::ClipboardImage,
        );
        let (id, wait) = self
            .issue_notice_target(
                notice,
                Target {
                    files: vec![file.clone()],
                    paths: paths.clone(),
                    url: None,
                    retry: None,
                    actions: Vec::new(),
                    device: None,
                    setup_requests: Vec::new(),
                },
                replaces,
            )
            .await?;
        *burst = if save_error.is_none() && copied != Some(false) {
            Some(Burst {
                id,
                paths,
                source: file.source.clone(),
                kind,
                count,
                saved: all_saved,
                started,
                arrived,
            })
        } else {
            None
        };
        Ok(wait)
    }
    async fn open(&self, path: &Path) -> Result<()> {
        self.open_received(&ReceivedFile::from_file(
            path.to_owned(),
            crate::storage::open_regular_file(path)?,
        )?)
        .await
    }
    async fn open_received(&self, file: &ReceivedFile) -> Result<()> {
        self.begin_viewer(file, false)?.await
    }
    async fn begin_open_received(
        &self,
        file: &ReceivedFile,
    ) -> Result<crate::ui::portal::Interaction> {
        // Core uses this begin/wait split for configured automatic image opens.
        // Notification Open and Recent keep the file's normal application.
        self.begin_viewer(file, true)
    }
}
pub fn clipboard_helper(_mime: String, _ticket: u128) -> Result<()> {
    anyhow::bail!("clipboard helper unavailable on macOS: the daemon writes NSPasteboard directly")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{
        child_bridge::{Frame, Session},
        child_channel as transport,
    };
    use std::os::unix::net::UnixStream;
    #[tokio::test]
    async fn path_only_notifications_never_reopen_a_received_display_path() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let actions = DesktopActions::new(temp.path().to_owned());
        let path = temp.path().join("received.png");
        for exists in [true, false] {
            if exists {
                std::fs::write(&path, b"replacement")?;
            } else {
                std::fs::remove_file(&path)?;
            }
            assert!(
                actions
                    .notify(&path, "image/png")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("require a received descriptor")
            );
            assert!(
                actions
                    .notify_clipboard_image(&path, "image/png")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("require a received descriptor")
            );
        }
        Ok(())
    }
    async fn session() -> Result<(Arc<Session>, std::thread::JoinHandle<()>)> {
        let (parent, mut child) = UnixStream::pair()?;
        let worker = std::thread::spawn(move || {
            while let Ok((frame, _)) = transport::receive::<Frame>(&mut child) {
                let reply = match frame {
                    Frame::Hello { token } => Frame::Ready { token },
                    Frame::Request { id, .. } | Frame::Cancel { id } => {
                        Frame::Reply { id, error: None }
                    }
                    _ => break,
                };
                if transport::send(&mut child, &reply, None).is_err() {
                    break;
                }
            }
        });
        let (events, _) = tokio::sync::mpsc::channel(2);
        let session = Session::start(parent, "f".repeat(64), events)?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !session.ready.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        session.activate();
        Ok((session, worker))
    }
    #[tokio::test]
    async fn copied_notice_retains_descriptor_and_provenance_after_path_replacement() -> Result<()>
    {
        let _serial = crate::ui::child_bridge::tests::SERIAL.lock().await;
        let (session, worker) = session().await?;
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("received.png");
        std::fs::write(&path, b"original received bytes")?;
        let source = ArrivalSource {
            device: pneu_proto::Id::new("11".repeat(16))?,
            name: "Source Mac".into(),
            several: true,
        };
        let file = ReceivedFile::open(&path)?.with_source(source.clone());
        let actions = DesktopActions::new(temp.path().to_owned());
        actions.record_clipboard_copy(&file).await;
        std::fs::remove_file(&path)?;
        std::fs::write(&path, b"replacement")?;
        let wait = actions
            .begin_notify_copied_received(&file, "image/png")
            .await?;
        let record = actions
            .targets
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        let mut bytes = Vec::new();
        use std::io::Read;
        record.target.files[0]
            .viewer_descriptor()?
            .read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"original received bytes");
        assert_eq!(record.target.files[0].source, Some(source));
        assert!(record.notification.summary.contains("Source Mac"));
        assert!(actions.copied.lock().await.is_empty());
        wait.await?;
        actions.clear_notifications().await;
        session.close();
        worker.join().unwrap();
        Ok(())
    }
    #[tokio::test]
    async fn target_insertion_is_bounded_and_cancellation_releases_targets() -> Result<()> {
        let _serial = crate::ui::child_bridge::tests::SERIAL.lock().await;
        let (session, worker) = session().await?;
        let actions = DesktopActions::new(PathBuf::from("/unused"));
        for _ in 0..300 {
            if let Ok((_, wait)) = actions
                .issue_notice(Notice::new("Pneu test cancellation"), Vec::new(), None, 0)
                .await
            {
                drop(wait);
            }
            assert!(actions.targets.lock().unwrap().is_empty());
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        // Drain the child's replies before measuring successful retention.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        for _ in 0..200 {
            let (_, wait) = actions
                .issue_notice(Notice::new("Pneu test capacity"), Vec::new(), None, 0)
                .await?;
            assert!(actions.targets.lock().unwrap().len() <= 128);
            wait.await?;
        }
        assert_eq!(actions.targets.lock().unwrap().len(), 128);
        actions.clear_notifications().await;
        session.close();
        worker.join().unwrap();
        Ok(())
    }
    #[tokio::test]
    async fn copied_bookkeeping_is_bounded_without_notification_completions() -> Result<()> {
        let actions = DesktopActions::new(PathBuf::from("/unused"));
        for index in 0..1000 {
            let file = ReceivedFile::from_file(
                PathBuf::from(format!("/unused/{index}")),
                tempfile::tempfile()?,
            )?;
            actions.record_clipboard_copy(&file).await;
            assert!(actions.copied.lock().await.len() <= 256);
        }
        Ok(())
    }
    #[tokio::test]
    async fn stale_sessions_unoffered_actions_and_expired_targets_are_rejected() -> Result<()> {
        let _serial = crate::ui::child_bridge::tests::SERIAL.lock().await;
        let (session, worker) = session().await?;
        let actions = DesktopActions::new(PathBuf::from("/unused"));
        let mut notice = Notice::new("Pneu test actions");
        notice.actions = vec![("open", "Open")];
        let (id, wait) = actions.issue_notice(notice, Vec::new(), None, 0).await?;
        wait.await?;
        let namespace = format!(
            "{}-{}",
            actions.namespace,
            actions
                .targets
                .lock()
                .unwrap()
                .get(&id)
                .unwrap()
                .notification
                .revision
        );
        assert!(
            actions
                .take_notification(id, "old-session", "open")
                .await
                .is_err()
        );
        assert!(
            actions
                .take_notification(id, &namespace, "retry")
                .await
                .is_err()
        );
        assert!(actions.targets.lock().unwrap().contains_key(&id));
        actions.take_notification(id, &namespace, "open").await?;
        assert!(
            actions
                .take_notification(id, &namespace, "open")
                .await
                .is_err()
        );
        let mut notice = Notice::text("https://example.com");
        notice.expires = 20;
        let (id, wait) = actions.issue_notice(notice, Vec::new(), None, 0).await?;
        wait.await?;
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        assert!(!actions.targets.lock().unwrap().contains_key(&id));
        assert!(
            actions
                .take_notification(id, &namespace, "link")
                .await
                .is_err()
        );
        actions.clear_notifications().await;
        session.close();
        worker.join().unwrap();
        Ok(())
    }
}
