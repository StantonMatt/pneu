//! Native Windows clipboard, Shell and session-scoped toast effects.
use super::*;
use anyhow::Context;
#[path = "windows_shell.rs"]
mod shell;
#[path = "windows_toasts.rs"]
mod toasts;
use crate::{
    Core,
    ui::{
        notifications::Notice,
        portal::Interaction,
        windows_toast_model::{Record, Targets},
    },
};
use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock},
};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone, Default)]
struct Target {
    files: Vec<ReceivedFile>,
    paths: Vec<PathBuf>,
    url: Option<String>,
    retry: Option<(pneu_proto::Id, pneu_proto::Id, pneu_proto::Id)>,
    attribution: Option<String>,
    device: Option<pneu_proto::Id>,
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
struct Submission {
    targets: Arc<Mutex<Targets<Target>>>,
    record: Arc<Record<Target>>,
    sender: toasts::Sender,
    acknowledged: bool,
}
impl Drop for Submission {
    fn drop(&mut self) {
        if !self.acknowledged {
            let mut targets = self.targets.lock().expect("toast targets");
            if targets.remove(&self.record) {
                self.sender.remove(self.record.id, self.record.group());
            }
        }
    }
}

struct PhoneSubmission {
    sender: toasts::Sender,
    tag: String,
    life: Arc<crate::ui::notice_lifecycle::Lifecycle>,
    accepted: bool,
}
impl Drop for PhoneSubmission {
    fn drop(&mut self) {
        if !self.accepted {
            self.life.cancel();
            self.sender.remove_tag(
                self.tag.clone(),
                crate::ui::windows_toast_model::PHONE_GROUP.into(),
            );
        }
    }
}

pub async fn open_folder(path: &Path) -> Result<()> {
    shell::open_folder(path.to_owned()).await?.await
}

pub async fn show_files(paths: &[PathBuf]) -> Result<()> {
    shell::show_files(paths.to_owned()).await?.await
}

pub async fn open_url(url: &str) -> Result<()> {
    shell::open_url(url.to_owned()).await?.await
}

pub struct DesktopActions {
    // Preparation and reaping of retired offers only. Installed writers must
    // never hold this drain open while a replacement is being retired.
    jobs: Arc<crate::work::Work>,
    writers: Arc<crate::work::Work>,
    cache: PathBuf,
    targets: Arc<Mutex<Targets<Target>>>,
    sender: OnceLock<toasts::Sender>,
    host: Mutex<Option<toasts::Host>>,
    activations: Mutex<Option<tokio::sync::mpsc::Receiver<toasts::Activation>>>,
    copied: AsyncMutex<HashSet<PathBuf>>,
    burst: AsyncMutex<Option<Burst>>,
}
impl DesktopActions {
    pub fn new(cache: PathBuf) -> Self {
        Self {
            jobs: Arc::new(crate::work::Work::default()),
            writers: Arc::new(crate::work::Work::default()),
            cache,
            targets: Arc::default(),
            sender: OnceLock::new(),
            host: Mutex::new(None),
            activations: Mutex::new(None),
            copied: AsyncMutex::new(HashSet::new()),
            burst: AsyncMutex::new(None),
        }
    }
    pub async fn start_notifications(&self) -> Result<()> {
        crate::ui::runtime::require_ui_session()?;
        let (host, activations) = toasts::Host::start(self.cache.clone()).await?;
        self.sender
            .set(host.sender())
            .map_err(|_| anyhow::anyhow!("toast host already started"))?;
        *self.activations.lock().expect("toast activations") = Some(activations);
        *self.host.lock().expect("toast host") = Some(host);
        Ok(())
    }
    pub(crate) fn take_activations(
        &self,
    ) -> Option<tokio::sync::mpsc::Receiver<toasts::Activation>> {
        self.activations.lock().expect("toast activations").take()
    }
    pub(crate) async fn withdraw_notice(&self, id: u32) -> Result<()> {
        let mut targets = self.targets.lock().expect("toast targets");
        if let Some(record) = targets.records.get(&id).cloned() {
            targets.remove(&record);
            if let Some(sender) = self.sender.get() {
                sender.remove(record.id, record.group());
            }
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
                attribution: None,
                device: None,
            },
            replaces,
        )
        .await
    }
    async fn issue_notice_target(
        &self,
        notice: Notice,
        mut target: Target,
        replaces: u32,
    ) -> Result<(u32, Interaction)> {
        target.device = notice.device.clone();
        let sender = self
            .sender
            .get()
            .context("Windows notifications require an interactive daemon")?
            .clone();
        let (record, wait) = {
            // Serialize target changes and native queue order, including a
            // replacement that reuses an activated/cancelled target's tag.
            let mut targets = self.targets.lock().expect("toast targets");
            let (record, retired) = targets.issue(notice, target, replaces)?;
            for old in retired {
                sender.remove(old.id, old.group());
            }
            let wait = sender.show(
                record.xml(record.target.attribution.as_deref()),
                record.id.to_string(),
                record.group().into(),
                record.life.clone(),
            );
            (record, wait)
        };
        let guard = Submission {
            targets: self.targets.clone(),
            record: record.clone(),
            sender: sender.clone(),
            acknowledged: false,
        };
        let wait = wait?;
        if let Some(deadline) = record.life.deadline {
            let targets = Arc::downgrade(&self.targets);
            let record = Arc::downgrade(&record);
            tokio::spawn(async move {
                tokio::time::sleep_until(deadline).await;
                if let (Some(targets), Some(record)) = (targets.upgrade(), record.upgrade()) {
                    let mut targets = targets.lock().expect("toast targets");
                    if targets.remove(&record) {
                        sender.remove(record.id, record.group());
                    }
                }
            });
        }
        Ok((
            record.id,
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
    fn take_notification(&self, arguments: &str) -> Result<(Arc<Record<Target>>, String)> {
        let mut targets = self.targets.lock().expect("toast targets");
        let (record, action) = targets.take(arguments)?;
        if let Some(sender) = self.sender.get() {
            sender.remove(record.id, record.group());
        }
        Ok((record, action))
    }
    pub(crate) async fn notification_action(
        &self,
        core: &Core,
        activation: toasts::Activation,
    ) -> Result<()> {
        let target = if activation.inputs.is_empty() {
            self.take_notification(&activation.arguments)
        } else {
            anyhow::bail!("toast action has unexpected input")
        };
        let (record, action) = match target {
            Ok(target) => target,
            Err(_) => {
                match activation.arguments.rsplit('-').next().unwrap_or("") {
                    "pair" => core.request_add_device(),
                    "folder" => core.request_window(true),
                    "open" | "default" | "show" => open_folder(&core.config().save_path).await?,
                    _ => core.request_window(false),
                }
                return Ok(());
            }
        };
        eprintln!(
            "pneud: notification action notification={} action={action}",
            record.id
        );
        let target = &record.target;
        match action.as_str() {
            "allow" | "deny" => {
                crate::ui::window_setup::decide(
                    core,
                    &record.notice.setup_requests,
                    action == "allow",
                )
                .await
            }
            "window" => core.request_window(false),
            "default" | "open" => {
                if let Some(file) = target.files.last() {
                    self.open_received(file).await?;
                }
            }
            "show" => show_files(&target.paths).await?,
            "link" => {
                if let Some(url) = &target.url {
                    shell::open_url(url.clone()).await?.await?;
                }
            }
            "folder" => core.request_window(true),
            "pair" => core.request_add_device(),
            "remove" => {
                if let Some(device) = &target.device {
                    core.request_remove_window(device.clone(), None);
                }
            }
            "retry" => {
                if let Some((operation, artifact, device)) = &target.retry {
                    core.retry_transfer_artifact(
                        operation.clone(),
                        Some(artifact.clone()),
                        device.clone(),
                    )
                    .await?;
                }
            }
            _ => anyhow::bail!("unsupported notification action"),
        }
        Ok(())
    }
}
fn file_attribution(metadata: &pneu_proto::FileBegin) -> Option<String> {
    use pneu_proto::FileKind;
    matches!(
        metadata.kind,
        FileKind::Photo | FileKind::Screenshot | FileKind::Recording
    )
    .then(|| metadata.file_name.clone())
}

#[async_trait]
impl PcActions for DesktopActions {
    async fn show_phone_notification(
        &self,
        id: &str,
        notice: &crate::ui::notifications::PhoneNotice,
        replaces: u32,
    ) -> Result<u32> {
        self.begin_phone_notification(id, notice, replaces)
            .await?
            .await
    }
    async fn begin_phone_notification<'a>(
        &'a self,
        id: &'a str,
        notice: &'a crate::ui::notifications::PhoneNotice,
        _replaces: u32,
    ) -> Result<super::PhoneAcknowledgement<'a>> {
        use crate::ui::{notice_lifecycle::Lifecycle, windows_toast_model::*};
        let sender = self
            .sender
            .get()
            .context("Windows notifications require an interactive daemon")?;
        let submission = PhoneSubmission {
            sender: sender.clone(),
            tag: phone_tag(id),
            life: Arc::new(Lifecycle::new(None)),
            accepted: false,
        };
        let wait = sender.show(
            phone_xml(notice),
            submission.tag.clone(),
            PHONE_GROUP.into(),
            submission.life.clone(),
        )?;
        Ok(Box::pin(async move {
            // Move the complete cancellation guard into the future. Capturing
            // only its Copy bool drops the guard before submission is awaited.
            let mut submission = submission;
            wait.await?;
            submission.accepted = true;
            Ok(0)
        }))
    }
    async fn remove_phone_notification(&self, id: &str, _handle: u32) -> Result<()> {
        use crate::ui::windows_toast_model::*;
        self.sender
            .get()
            .context("toast host unavailable")?
            .remove_tag(phone_tag(id), PHONE_GROUP.into());
        Ok(())
    }

    async fn clipboard_text(&self, _text: &str) -> Result<()> {
        anyhow::bail!("Windows clipboard writes require the clipboard actor")
    }
    async fn clipboard_image(&self, _path: &Path, _mime: &str) -> Result<()> {
        anyhow::bail!("Windows clipboard writes require the clipboard actor")
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
            let native = crate::ui::clipboard::Offer::prepare_sensitive(&input, ticket, sensitive)?;
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
            .as_mut()
            .context("missing native clipboard offer")?
            .install_tracked(self.writers.clone(), self.jobs.clone())
            .await
    }
    async fn drain_clipboard(&self) {
        self.jobs.drain().await;
    }
    async fn shutdown(&self) -> Result<()> {
        let host = self.host.lock().expect("toast host").take();
        if let Some(host) = host {
            host.shutdown().await;
        }
        for record in self.targets.lock().expect("toast targets").records.values() {
            record.life.cancel();
        }
        self.jobs.drain().await;
        self.writers.drain().await;
        Ok(())
    }
    async fn record_clipboard_copy(&self, file: &ReceivedFile) {
        let mut copied = self.copied.lock().await;
        if copied.len() >= 256 {
            copied.clear();
        }
        copied.insert(file.path().to_owned());
    }
    async fn notify(&self, _path: &Path, _mime: &str) -> Result<()> {
        self.notify_received(&ReceivedFile::open(_path)?, _mime)
            .await
    }
    async fn notify_clipboard_image(&self, _path: &Path, _mime: &str) -> Result<()> {
        self.notify_copied_received(&ReceivedFile::open(_path)?, _mime)
            .await
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
        let (_, wait) = self
            .issue_notice_target(
                n,
                Target {
                    attribution: file_attribution(metadata),
                    ..Target::default()
                },
                0,
            )
            .await?;
        wait.await
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
        self.issue_notice_target(
            n,
            Target {
                attribution: file_attribution(metadata),
                ..Target::default()
            },
            0,
        )
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
    async fn notify_ring_failure(&self, phone: &str) -> Result<()> {
        let mut notice = crate::ui::notifications::Notice::new(format!("Couldn't ring {phone}"));
        notice.body = crate::ui::notifications::escape(&format!("{phone} didn't respond"));
        self.notice(notice, Vec::new(), None, 0, None)
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
                    attribution: None,
                    device: None,
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
        self.issue_notice_target(
            n,
            Target {
                files: vec![file.clone()],
                paths: vec![file.path().to_owned()],
                attribution: file_attribution(metadata),
                ..Target::default()
            },
            0,
        )
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
            copied,
            false,
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
                .records
                .get(&previous.id)
                .is_some_and(|r| r.life.active())
        {
            replaces = previous.id;
            count = previous.count + 1;
            started = previous.started;
            kind = if previous.kind == kind { kind } else { None };
            all_saved = saved && previous.saved;
            paths.extend(previous.paths.iter().cloned());
            notice = Notice::burst(count, kind, copied, all_saved);
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
                    attribution: (count == 1).then(|| file_attribution(metadata)).flatten(),
                    device: None,
                },
                replaces,
            )
            .await?;
        *burst = if save_error.is_none() {
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
        let file = ReceivedFile::open(path).map_err(|_| FileUnavailable)?;
        self.open_received(&file).await
    }
    async fn open_received(&self, file: &ReceivedFile) -> Result<()> {
        self.begin_open_received(file).await?.await
    }
    async fn begin_open_received(
        &self,
        file: &ReceivedFile,
    ) -> Result<crate::ui::portal::Interaction> {
        shell::open_received(file.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    // The real worker acknowledges installation before its owner window exits.
    // Keep that lifetime in this seam, without requiring an interactive desktop.
    async fn install_serving(
        actions: &DesktopActions,
        offer: &mut PreparedClipboard,
    ) -> Result<()> {
        offer
            .native
            .as_mut()
            .unwrap()
            .install_worker(
                actions.writers.clone(),
                actions.jobs.clone(),
                |cancel, tx| {
                    let _ = tx.send(Ok(()));
                    while !cancel.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                },
            )
            .await
    }

    #[tokio::test]
    async fn clipboard_retirement_does_not_drain_installed_predecessor() -> Result<()> {
        for mode in [
            "prepared",
            "cancel",
            "timeout",
            "failure",
            "drop",
            "cancel-stop",
        ] {
            let actions = DesktopActions::new(PathBuf::new());
            let mut first = actions
                .prepare_clipboard(ClipboardInput::Text("first".into()), 1)
                .await?;
            install_serving(&actions, &mut first).await?;
            let mut second = actions
                .prepare_clipboard(ClipboardInput::Text("second".into()), 2)
                .await?;
            let exited = Arc::new(AtomicBool::new(mode == "prepared"));
            let (release_cleanup, cleanup_gate) = std::sync::mpsc::channel();
            if mode != "prepared" {
                let completed = exited.clone();
                let (started, ready) = tokio::sync::oneshot::channel();
                let install = second.native.as_mut().unwrap().install_worker(
                    actions.writers.clone(),
                    actions.jobs.clone(),
                    move |cancel, reply| {
                        let _ = started.send(());
                        if mode == "failure" {
                            let _ = reply.send(Err(anyhow::anyhow!("failure after write")));
                        } else {
                            // No acknowledgement: model a contended write or
                            // canceled installation before marker confirmation.
                            let _reply = reply;
                            while !cancel.load(Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                        }
                        if mode == "cancel-stop" {
                            cleanup_gate.recv_timeout(Duration::from_secs(5)).unwrap();
                        }
                        // Cleanup must finish before the driver emits Stopped.
                        std::thread::sleep(Duration::from_millis(30));
                        completed.store(true, Ordering::SeqCst);
                    },
                );
                if mode == "failure" {
                    assert!(install.await.is_err());
                } else if mode == "timeout" {
                    assert!(
                        tokio::time::timeout(Duration::from_millis(50), install)
                            .await
                            .is_err()
                    );
                } else {
                    tokio::select! {
                        result = install => panic!("installation unexpectedly finished: {result:?}"),
                        result = ready => result?,
                    }
                }
            }
            if mode == "cancel-stop" {
                // A canceled stop must leave its handle available to Drop.
                assert!(
                    tokio::time::timeout(Duration::from_millis(1), second.stop())
                        .await
                        .is_err()
                );
                release_cleanup.send(())?;
                drop(second);
            } else if mode == "drop" {
                // Same cleanup path as unwinding a driver helper.
                drop(second);
            } else {
                tokio::time::timeout(Duration::from_secs(2), second.stop()).await?;
                assert!(
                    exited.load(Ordering::SeqCst),
                    "stop returned before cleanup: {mode}"
                );
                drop(second);
            }
            // This is the exact gate immediately before Stopped(ticket).
            tokio::time::timeout(Duration::from_secs(2), actions.drain_clipboard()).await?;
            assert!(
                exited.load(Ordering::SeqCst),
                "drain returned before cleanup: {mode}"
            );
            assert_eq!(actions.writers.active(), 1, "predecessor retired: {mode}");
            let mut third = actions
                .prepare_clipboard(ClipboardInput::Text("third".into()), 3)
                .await?;
            tokio::time::timeout(
                Duration::from_secs(2),
                install_serving(&actions, &mut third),
            )
            .await??;
            assert_eq!(actions.writers.active(), 2);
            third.stop().await;
            first.stop().await;
            tokio::time::timeout(Duration::from_secs(2), actions.shutdown()).await??;
            assert_eq!(actions.writers.active(), 0);
            assert_eq!(actions.jobs.active(), 0);
        }
        Ok(())
    }

    #[tokio::test]
    async fn clipboard_shutdown_waits_for_serving_writer() -> Result<()> {
        let actions = DesktopActions::new(PathBuf::new());
        let mut offer = actions
            .prepare_clipboard(ClipboardInput::Text("serving".into()), 1)
            .await?;
        install_serving(&actions, &mut offer).await?;
        tokio::time::timeout(Duration::from_secs(2), actions.drain_clipboard()).await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), actions.shutdown())
                .await
                .is_err()
        );
        drop(offer);
        tokio::time::timeout(Duration::from_secs(2), actions.shutdown()).await??;
        assert_eq!(actions.writers.active(), 0);
        assert_eq!(actions.jobs.active(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn headless_effects_require_their_native_actors() -> Result<()> {
        let actions = DesktopActions::new(PathBuf::new());
        assert!(
            actions
                .clipboard_text("test")
                .await
                .unwrap_err()
                .to_string()
                .contains("require the clipboard actor")
        );
        assert!(
            actions
                .clipboard_image(Path::new("unused"), "image/png")
                .await
                .unwrap_err()
                .to_string()
                .contains("require the clipboard actor")
        );
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("received.txt");
        std::fs::write(&path, "received")?;
        for result in [
            actions.notify(&path, "text/plain").await,
            actions.notify_clipboard_failure().await,
            actions.notify_text_failure().await,
            actions.notify_text("test").await,
            actions.notify_send_admission_failure("test").await,
        ] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Windows notifications require an interactive daemon")
            );
        }
        Ok(())
    }
}
