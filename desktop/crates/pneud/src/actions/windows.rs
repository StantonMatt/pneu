//! Headless actions until the Windows desktop integrations are available.
use super::*;

pub struct DesktopActions;
impl DesktopActions {
    pub fn new(_cache: PathBuf) -> Self {
        Self
    }
}
#[async_trait]
impl PcActions for DesktopActions {
    async fn clipboard_text(&self, _text: &str) -> Result<()> {
        anyhow::bail!("clipboard is not yet supported on Windows")
    }
    async fn clipboard_image(&self, _path: &Path, _mime: &str) -> Result<()> {
        anyhow::bail!("clipboard is not yet supported on Windows")
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
        for result in [
            actions.clipboard_text("test").await,
            actions
                .clipboard_image(Path::new("unused"), "image/png")
                .await,
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
