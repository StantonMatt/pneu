//! M2 will run the persistent AppKit/Slint child here, on its main thread.
pub(crate) use crate::ui::send_request::SendRequest;
pub fn run(_paths: crate::Paths) -> anyhow::Result<()> {
    super::runtime::require_ui()
}
