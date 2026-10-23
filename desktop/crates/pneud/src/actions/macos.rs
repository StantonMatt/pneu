//! Headless M1 desktop backend. M2 replaces UI stubs with UI-child IPC while
//! keeping descriptor-bound ReceivedFile and actor-owned clipboard admission.
use super::*;
use crate::ui::clipboard::Offer;

pub struct DesktopActions {
    jobs: Arc<crate::work::Work>,
}
impl DesktopActions {
    pub fn new(_cache: PathBuf) -> Self {
        Self {
            jobs: Arc::new(crate::work::Work::default()),
        }
    }
    pub async fn notice(
        &self,
        _notice: crate::ui::notifications::Notice,
        _files: Vec<ReceivedFile>,
        _url: Option<String>,
        _replaces: u32,
        _preview: Option<&ReceivedFile>,
    ) -> Result<u32> {
        Self::notification();
        Ok(0)
    }
    fn notification() {
        eprintln!("pneud: notifications unavailable on this platform yet (macOS M1)");
    }
}
#[async_trait]
impl PcActions for DesktopActions {
    async fn shutdown(&self) -> Result<()> {
        self.jobs.drain().await;
        Ok(())
    }
    async fn clipboard_text(&self, _text: &str) -> Result<()> {
        anyhow::bail!("macOS clipboard writes require an actor install ticket")
    }
    async fn clipboard_image(&self, _path: &Path, _mime: &str) -> Result<()> {
        anyhow::bail!("macOS clipboard writes require an actor install ticket")
    }
    async fn prepare_clipboard(
        &self,
        input: ClipboardInput,
        ticket: u128,
    ) -> Result<PreparedClipboard> {
        crate::work::blocking(self.jobs.clone(), move || {
            let native = Offer::prepare(&input, ticket)?;
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
        // Deliberately synchronous within this poll: there must be no detached
        // task capable of writing after actor cancellation reports Stopped.
        // AppKit retains the item bytes. No clipboard-serving child is needed.
        prepared
            .native
            .as_ref()
            .context("missing native clipboard offer")?
            .install()
    }
    async fn notify(&self, _path: &Path, _mime: &str) -> Result<()> {
        Self::notification();
        Ok(())
    }
    async fn notify_text(&self, _text: &str) -> Result<()> {
        Self::notification();
        Ok(())
    }
    async fn notify_text_failure(&self) -> Result<()> {
        Self::notification();
        Ok(())
    }
    async fn notify_clipboard_failure(&self) -> Result<()> {
        Self::notification();
        Ok(())
    }
    async fn notify_save_failure(
        &self,
        _metadata: &pneu_proto::FileBegin,
        _code: pneu_proto::NackCode,
        _destination: crate::ui::ReceiveDestination,
        _folder: &Path,
    ) -> Result<()> {
        Self::notification();
        Ok(())
    }
    async fn notify_send_admission_failure(&self, _reason: &str) -> Result<()> {
        Self::notification();
        Ok(())
    }
    async fn notify_delivery(
        &self,
        _transfer: &crate::outbox::Transfer,
        _device_name: &str,
    ) -> Result<()> {
        Self::notification();
        Ok(())
    }
    async fn open(&self, _path: &Path) -> Result<()> {
        anyhow::bail!("open/reveal unavailable on this platform yet (macOS M1)")
    }
}
pub fn clipboard_helper(_mime: String, _ticket: u128) -> Result<()> {
    anyhow::bail!("clipboard helper unavailable on macOS: the daemon writes NSPasteboard directly")
}
