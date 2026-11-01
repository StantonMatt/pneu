use crate::platform::fs as platform_fs;
#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrivalSource {
    pub device: pneu_proto::Id,
    pub name: String,
    pub several: bool,
}
impl ArrivalSource {
    fn file_notice(
        &self,
        notice: crate::ui::notifications::Notice,
        clipboard: bool,
    ) -> crate::ui::notifications::Notice {
        if clipboard {
            notice.copied_from(&self.name)
        } else if self.several {
            notice.from_device(&self.name)
        } else {
            notice
        }
    }
}
#[derive(Debug)]
pub(crate) struct FileUnavailable;
impl std::fmt::Display for FileUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The file was moved or deleted")
    }
}
impl std::error::Error for FileUnavailable {}

/// An anchored received file. Reads and notification Open use the retained
/// descriptor; its display pathname is only a navigation/recovery hint.
#[derive(Clone, Debug)]
pub struct ReceivedFile {
    path: PathBuf,
    file: Arc<std::fs::File>,
    source: Option<ArrivalSource>,
}
impl ReceivedFile {
    pub(crate) fn from_file(path: PathBuf, file: std::fs::File) -> Result<Self> {
        anyhow::ensure!(file.metadata()?.is_file(), "received file must be regular");
        #[cfg(windows)]
        let file = platform_fs::received_reader(&file)?;
        Ok(Self {
            path,
            file: Arc::new(file),
            source: None,
        })
    }
    pub(crate) fn with_source(mut self, source: ArrivalSource) -> Self {
        self.source = Some(source);
        self
    }
    fn notice(
        &self,
        notice: crate::ui::notifications::Notice,
        clipboard: bool,
    ) -> crate::ui::notifications::Notice {
        self.source.as_ref().map_or_else(
            || notice.clone(),
            |source| source.file_notice(notice.clone(), clipboard),
        )
    }
    pub fn open(path: &Path) -> Result<Self> {
        let file = platform_fs::open_read(path)?;
        Self::from_file(path.to_owned(), file)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    #[cfg(target_os = "linux")]
    fn viewer_descriptor(&self) -> Result<std::fs::File> {
        platform_fs::viewer_descriptor(&self.file)
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn viewer_descriptor(&self) -> Result<std::fs::File> {
        Ok(self.file.try_clone()?)
    }
    pub fn read_bounded(&self, limit: usize) -> Result<Vec<u8>> {
        anyhow::ensure!(
            self.file.metadata()?.len() <= limit as u64,
            "input exceeds bounded read limit"
        );
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        while bytes.len() <= limit {
            let cap = buffer.len().min(limit + 1 - bytes.len());
            let n = platform_fs::read_at(&self.file, &mut buffer[..cap], bytes.len() as u64)?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        anyhow::ensure!(bytes.len() <= limit, "input grew beyond bounded read limit");
        Ok(bytes)
    }
    pub(crate) fn is_image(&self, mime: &str) -> Result<bool> {
        let mut header = [0; 32];
        let n = platform_fs::read_at(&self.file, &mut header, 0)?;
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

use platform_fs::clipboard_file;
#[cfg(all(test, unix))]
use platform_fs::unlinked_clipboard_file;

/// Data-bearing inputs stay anchored while preparation runs outside the actor.
#[derive(Clone, Debug)]
pub enum ClipboardInput {
    Text(String),
    Image(ReceivedFile, String),
    Bytes {
        bytes: Vec<u8>,
        mime: String,
        cache: PathBuf,
    },
    Formats {
        text: String,
        png: Vec<u8>,
        cache: PathBuf,
    },
}
/// A prepared helper is inert until the actor authorizes its ticket. Only the
/// driver owns this value; dropping it kills and schedules reaping of the child.
pub struct PreparedClipboard {
    pub hashes: Vec<[u8; 32]>,
    pub marker_confirmation: bool,
    input: ClipboardInput,
    #[cfg(target_os = "linux")]
    helper: Option<linux::ClipboardCandidate>,
    #[cfg(any(target_os = "macos", windows))]
    native: Option<crate::ui::clipboard::Offer>,
    #[cfg(test)]
    pub(crate) test_exited: bool,
}
impl PreparedClipboard {
    #[cfg(windows)]
    pub(crate) fn set_install_authority(
        &mut self,
        authority: impl Fn() -> bool + Send + Sync + 'static,
    ) {
        if let Some(native) = self.native.as_mut() {
            native.set_authority(authority);
        }
    }
    pub fn immediate(input: ClipboardInput) -> Result<Self> {
        use sha2::{Digest, Sha256};
        use std::io::Write;
        let (input, hashes) = match input {
            ClipboardInput::Text(text) => {
                let hashes = vec![Sha256::digest(text.as_bytes()).into()];
                (ClipboardInput::Text(text), hashes)
            }
            ClipboardInput::Image(file, mime) => {
                let hashes = vec![
                    Sha256::digest(file.read_bounded(crate::store::history::MAX_IMAGE)?).into(),
                ];
                (ClipboardInput::Image(file, mime), hashes)
            }
            ClipboardInput::Bytes { bytes, mime, cache } => {
                let hashes = vec![Sha256::digest(&bytes).into()];
                let mut file = clipboard_file(&cache)?;
                file.write_all(&bytes)?;
                (
                    ClipboardInput::Image(
                        ReceivedFile::from_file(cache.join("clipboard.png"), file)?,
                        mime,
                    ),
                    hashes,
                )
            }
            ClipboardInput::Formats { text, png, cache } => {
                let hashes = vec![
                    Sha256::digest(text.as_bytes()).into(),
                    Sha256::digest(&png).into(),
                ];
                let mut file = clipboard_file(&cache)?;
                file.write_all(&png)?;
                (
                    ClipboardInput::Image(
                        ReceivedFile::from_file(cache.join("clipboard.png"), file)?,
                        "image/png".into(),
                    ),
                    hashes,
                )
            }
        };
        Ok(Self {
            hashes,
            marker_confirmation: false,
            input,
            #[cfg(target_os = "linux")]
            helper: None,
            #[cfg(any(target_os = "macos", windows))]
            native: None,
            #[cfg(test)]
            test_exited: false,
        })
    }
    pub fn input(&self) -> &ClipboardInput {
        &self.input
    }
    pub(crate) async fn wait_exit(&mut self) {
        #[cfg(test)]
        if self.test_exited {
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(child) = self.helper.as_mut().and_then(|h| h.child.as_mut()) {
            let _ = child.wait().await;
            return;
        }
        std::future::pending::<()>().await;
    }
    pub(crate) async fn stop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if let Some(helper) = &mut self.helper {
                helper.stop().await;
            }
            self.helper.take();
        }
        #[cfg(windows)]
        if let Some(native) = &mut self.native {
            native.stop().await;
        }
        #[cfg(any(target_os = "macos", windows))]
        self.native.take();
    }
}
#[async_trait]
pub trait PcActions: Send + Sync {
    #[cfg(target_os = "macos")]
    async fn ui_test_notification(&self, _transient: bool) -> Result<u32> {
        anyhow::bail!("UI test notifications unavailable")
    }

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
    /// Preparation never changes the OS selection. Lightweight/mock backends
    /// use an immediate candidate; the real helper reports its offered hashes.
    async fn prepare_clipboard(
        &self,
        input: ClipboardInput,
        _ticket: u128,
    ) -> Result<PreparedClipboard> {
        tokio::task::spawn_blocking(move || PreparedClipboard::immediate(input)).await?
    }
    async fn prepare_clipboard_sensitive(
        &self,
        input: ClipboardInput,
        ticket: u128,
        _sensitive: bool,
    ) -> Result<PreparedClipboard> {
        self.prepare_clipboard(input, ticket).await
    }
    async fn install_clipboard(&self, prepared: &mut PreparedClipboard) -> Result<()> {
        match &prepared.input {
            ClipboardInput::Text(text) => self.clipboard_text(text).await,
            ClipboardInput::Image(file, mime) => self.clipboard_received(file, mime).await,
            _ => anyhow::bail!("unnormalized clipboard candidate"),
        }
    }
    /// Presentation bookkeeping after the actor confirms the OS selection.
    async fn record_clipboard_copy(&self, _file: &ReceivedFile) {}
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
    /// Issue while the caller holds its authority gate; await the returned
    /// acknowledgement only after releasing that gate. Synchronous backends
    /// retain their existing completion semantics.
    async fn begin_notify_copied_received(
        &self,
        file: &ReceivedFile,
        mime: &str,
    ) -> Result<crate::ui::portal::Interaction> {
        self.notify_copied_received(file, mime).await?;
        Ok(Box::pin(async { Ok(()) }))
    }
    async fn begin_notify_image_failure(
        &self,
        file: &ReceivedFile,
        metadata: &pneu_proto::FileBegin,
        saved: bool,
    ) -> Result<crate::ui::portal::Interaction> {
        self.notify_image_failure(file, metadata, saved).await?;
        Ok(Box::pin(async { Ok(()) }))
    }
    async fn begin_notify_file_at(
        &self,
        file: &ReceivedFile,
        metadata: &pneu_proto::FileBegin,
        saved: bool,
        save_error: Option<pneu_proto::NackCode>,
        folder: &Path,
        arrived: std::time::Instant,
    ) -> Result<crate::ui::portal::Interaction> {
        self.notify_file_at(file, metadata, saved, save_error, folder, arrived)
            .await?;
        Ok(Box::pin(async { Ok(()) }))
    }
    async fn begin_notify_text_from(
        &self,
        text: &str,
        source: &ArrivalSource,
    ) -> Result<crate::ui::portal::Interaction> {
        self.notify_text_from(text, source).await?;
        Ok(Box::pin(async { Ok(()) }))
    }
    async fn begin_notify_text_failure_from(
        &self,
        source: &ArrivalSource,
    ) -> Result<crate::ui::portal::Interaction> {
        self.notify_text_failure_from(source).await?;
        Ok(Box::pin(async { Ok(()) }))
    }
    async fn begin_notify_save_failure_from(
        &self,
        metadata: &pneu_proto::FileBegin,
        code: pneu_proto::NackCode,
        destination: crate::ui::ReceiveDestination,
        folder: &Path,
        source: &ArrivalSource,
    ) -> Result<crate::ui::portal::Interaction> {
        self.notify_save_failure_from(metadata, code, destination, folder, source)
            .await?;
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
    async fn notify_text_from(&self, text: &str, _source: &ArrivalSource) -> Result<()> {
        self.notify_text(text).await
    }
    async fn notify_text_failure_from(&self, _source: &ArrivalSource) -> Result<()> {
        self.notify_text_failure().await
    }
    async fn notify_save_failure_from(
        &self,
        metadata: &pneu_proto::FileBegin,
        code: pneu_proto::NackCode,
        destination: crate::ui::ReceiveDestination,
        folder: &Path,
        _source: &ArrivalSource,
    ) -> Result<()> {
        self.notify_save_failure(metadata, code, destination, folder)
            .await
    }
    async fn notify_ring_failure(&self, _phone: &str) -> Result<()> {
        Ok(())
    }
    async fn notify_send_admission_failure(&self, _reason: &str) -> Result<()> {
        Ok(())
    }
    async fn notify_delivery(
        &self,
        _transfer: &crate::outbox::Transfer,
        _device_name: &str,
    ) -> Result<()> {
        Ok(())
    }
    async fn drain_clipboard(&self) {}
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
pub(crate) fn decode_image(bytes: &[u8]) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    Ok(reader.decode()?)
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(all(test, target_os = "macos"))]
mod acl_tests {
    use super::*;
    #[test]
    fn anonymous_clipboard_backing_removes_inherited_acl_before_write() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let status = std::process::Command::new("/bin/chmod")
            .args([
                "+a",
                "group:everyone allow read,file_inherit,directory_inherit",
            ])
            .arg(&root)
            .status()?;
        anyhow::ensure!(status.success(), "cannot create inherited ACL fixture");
        let directory = std::fs::File::open(&root)?;
        let file = unlinked_clipboard_file(&directory)?;
        crate::platform::private::check(&file)?;
        assert_eq!(std::fs::read_dir(&root)?.count(), 0);
        assert!(clipboard_file(&root).is_err());
        Ok(())
    }
}
