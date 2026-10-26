//! Shared clipboard payload model; no native watcher.
pub const MAX_IMAGE: usize = 10 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Content {
    Text(String),
    Image(Vec<u8>),
    Formats(String, Vec<u8>),
}
