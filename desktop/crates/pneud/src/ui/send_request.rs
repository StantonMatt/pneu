use serde::{Deserialize, Serialize};
use std::path::PathBuf;
/// A bounded daemon-owned handoff to the window's existing picker. Authorities
/// are captured at the initiating entry point, never rebuilt by the window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SendRequest {
    pub id: String,
    pub paths: Option<Vec<PathBuf>>,
    pub targets: Vec<crate::outbox::Authority>,
    pub clipboard: bool,
    pub targeted: bool,
    pub error: Option<String>,
}
