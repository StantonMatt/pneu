//! Native clipboard effects; other desktop integrations arrive later.
use super::*;
use anyhow::Context;

pub struct DesktopActions {
    // Preparation and reaping of retired offers only. Installed writers must
    // never hold this drain open while a replacement is being retired.
    jobs: Arc<crate::work::Work>,
    writers: Arc<crate::work::Work>,
}
impl DesktopActions {
    pub fn new(_cache: PathBuf) -> Self {
        Self {
            jobs: Arc::new(crate::work::Work::default()),
            writers: Arc::new(crate::work::Work::default()),
        }
    }
}
#[async_trait]
impl PcActions for DesktopActions {
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
        crate::work::blocking(self.jobs.clone(), move || {
            let native = crate::ui::clipboard::Offer::prepare(&input, ticket)?;
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
        self.jobs.drain().await;
        self.writers.drain().await;
        Ok(())
    }
    async fn notify(&self, _path: &Path, _mime: &str) -> Result<()> {
        anyhow::bail!("notifications are not yet supported on Windows")
    }
    async fn notify_clipboard_failure(&self) -> Result<()> {
        anyhow::bail!("notifications are not yet supported on Windows")
    }
    async fn notify_text_failure(&self) -> Result<()> {
        anyhow::bail!("notifications are not yet supported on Windows")
    }
    async fn notify_save_failure(
        &self,
        _metadata: &pneu_proto::FileBegin,
        _code: pneu_proto::NackCode,
        _destination: crate::ui::ReceiveDestination,
        _folder: &Path,
    ) -> Result<()> {
        anyhow::bail!("notifications are not yet supported on Windows")
    }
    async fn notify_text(&self, _text: &str) -> Result<()> {
        anyhow::bail!("notifications are not yet supported on Windows")
    }
    async fn notify_send_admission_failure(&self, _reason: &str) -> Result<()> {
        anyhow::bail!("notifications are not yet supported on Windows")
    }
    async fn notify_delivery(
        &self,
        _transfer: &crate::outbox::Transfer,
        _device_name: &str,
    ) -> Result<()> {
        anyhow::bail!("notifications are not yet supported on Windows")
    }
    async fn open(&self, _path: &Path) -> Result<()> {
        anyhow::bail!("opening files is not yet supported on Windows")
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
    async fn all_notification_entry_points_report_unsupported() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("received");
        std::fs::write(&path, "saved")?;
        let file = ReceivedFile::open(&path)?;
        let metadata = pneu_proto::FileBegin {
            transfer_id: pneu_proto::Id::new("01".repeat(16))?,
            file_name: "received".into(),
            mime: "application/octet-stream".into(),
            size: 5,
            sha256: pneu_proto::Hash::new("02".repeat(32))?,
            kind: pneu_proto::FileKind::File,
            created_at: 1,
            source_media_id: None,
            hlc: None,
            source_device: None,
        };
        let source = ArrivalSource {
            device: pneu_proto::Id::new("03".repeat(16))?,
            name: "sender".into(),
            several: false,
        };
        let transfer = crate::outbox::Transfer::received(source.device.clone(), &metadata);
        let actions = DesktopActions::new(PathBuf::new());
        for result in [
            actions.notify(&path, &metadata.mime).await,
            actions.notify_clipboard_image(&path, &metadata.mime).await,
            actions.notify_received(&file, &metadata.mime).await,
            actions.notify_copied_received(&file, &metadata.mime).await,
            actions.notify_clipboard_failure().await,
            actions.notify_image_failure(&file, &metadata, true).await,
            actions.notify_text("text").await,
            actions.notify_text_from("text", &source).await,
            actions.notify_text_failure().await,
            actions.notify_text_failure_from(&source).await,
            actions
                .notify_save_failure(
                    &metadata,
                    pneu_proto::NackCode::NotWritable,
                    crate::ui::ReceiveDestination::SaveFolder,
                    dir.path(),
                )
                .await,
            actions
                .notify_save_failure_from(
                    &metadata,
                    pneu_proto::NackCode::NotWritable,
                    crate::ui::ReceiveDestination::SaveFolder,
                    dir.path(),
                    &source,
                )
                .await,
            actions
                .notify_file_at(
                    &file,
                    &metadata,
                    true,
                    None,
                    dir.path(),
                    std::time::Instant::now(),
                )
                .await,
            actions.notify_send_admission_failure("test").await,
            actions.notify_delivery(&transfer, "sender").await,
        ] {
            assert_eq!(
                result.unwrap_err().to_string(),
                "notifications are not yet supported on Windows"
            );
        }
        // begin_* delegates must propagate the error before returning an
        // interaction; Core can then report the optional presentation failure.
        for result in [
            actions
                .begin_notify_copied_received(&file, &metadata.mime)
                .await,
            actions
                .begin_notify_image_failure(&file, &metadata, true)
                .await,
            actions
                .begin_notify_file_at(
                    &file,
                    &metadata,
                    true,
                    None,
                    dir.path(),
                    std::time::Instant::now(),
                )
                .await,
            actions.begin_notify_text_from("text", &source).await,
            actions.begin_notify_text_failure_from(&source).await,
            actions
                .begin_notify_save_failure_from(
                    &metadata,
                    pneu_proto::NackCode::NotWritable,
                    crate::ui::ReceiveDestination::SaveFolder,
                    dir.path(),
                    &source,
                )
                .await,
        ] {
            assert!(
                matches!(result, Err(error) if error.to_string() == "notifications are not yet supported on Windows")
            );
        }
        assert_eq!(std::fs::read(path)?, b"saved");
        Ok(())
    }
    #[tokio::test]
    async fn headless_actions_do_not_report_unsupported_effects_as_success() -> Result<()> {
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
        for result in [
            actions.open(Path::new("unused")).await,
            actions
                .notify(Path::new("unused"), "application/octet-stream")
                .await,
            actions.notify_clipboard_failure().await,
            actions.notify_text_failure().await,
            actions.notify_text("test").await,
            actions.notify_send_admission_failure("test").await,
        ] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("not yet supported on Windows")
            );
        }
        Ok(())
    }
}
