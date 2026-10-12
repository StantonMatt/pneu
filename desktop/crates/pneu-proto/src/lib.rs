//! Pneu v1 syntax codec. See `docs/PROTOCOL.md` for authorization, TLS and state rules.
//!
//! Callers must serialize concurrent writers, apply deadlines and close a stream
//! after any error or cancellation mid-frame. This crate does not flush writes.
mod frame;
mod message;
mod pairing;

pub use frame::{decode_frame, encode_frame, read_frame, write_frame};
pub use message::*;
pub use pairing::{PairingUri, validate_address};

use sha2::{Digest as _, Sha256};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 47821;
pub const MDNS_SERVICE_TYPE: &str = "_pneu._tcp";
pub const CHUNK_SIZE: usize = 256 * 1024;
pub const MAX_CONTROL_PAYLOAD: usize = 64 * 1024;
/// Length prefix value, excluding the four-byte prefix.
pub const MAX_FRAME_SIZE: usize = 1 + 16 + 8 + CHUNK_SIZE;
pub const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024 * 1024;
pub const MAX_TIMESTAMP: u64 = 9_007_199_254_740_991;
pub const MAX_CLIPBOARD_BYTES: usize = 16 * 1024;
pub const MAX_PAIRING_URI_BYTES: usize = 8192;
pub const MAX_CANDIDATE_ADDRESSES: usize = 8;
pub const MAX_PENDING_CLIPBOARDS: usize = 1;
pub const PAIRING_TOKEN_TTL_SECS: u64 = 300;
pub const TCP_CONNECT_TIMEOUT_SECS: u64 = 5;
pub const TLS_HANDSHAKE_TIMEOUT_SECS: u64 = 10;
pub const HELLO_TIMEOUT_SECS: u64 = 10;
pub const PAIRING_TIMEOUT_SECS: u64 = 10;
pub const PING_IDLE_SECS: u64 = 60;
pub const PONG_TIMEOUT_SECS: u64 = 30;
pub const PEER_IDLE_TIMEOUT_SECS: u64 = 180;
pub const TRANSFER_PROGRESS_TIMEOUT_SECS: u64 = 60;
pub const RECEIPT_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame length {0} is invalid")]
    InvalidLength(usize),
    #[error("unknown message type 0x{0:02x}")]
    UnknownType(u8),
    #[error("invalid protocol value: {0}")]
    InvalidValue(String),
    #[error("invalid pairing URI: {0}")]
    InvalidUri(String),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

/// Hash exact DER certificate bytes. Does not parse/validate the certificate.
pub fn certificate_fingerprint(der: &[u8]) -> Hash {
    Hash::new(hex::encode(Sha256::digest(der))).expect("SHA-256 is always 32 bytes")
}

pub(crate) fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(ProtocolError::InvalidValue(message.into()))
    }
}

pub(crate) fn string_limit(value: &str, min: usize, max: usize, name: &str) -> Result<()> {
    require(
        (min..=max).contains(&value.len()) && !value.contains('\0'),
        &format!("{name}: expected {min}..={max} UTF-8 bytes without NUL"),
    )
}
