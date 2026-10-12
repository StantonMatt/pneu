use crate::{MAX_CLIPBOARD_BYTES, MAX_FILE_SIZE, MAX_TIMESTAMP, Result, require, string_limit};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;

macro_rules! hex_type {
    ($name:ident, $digits:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                require(
                    value.len() == $digits
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    concat!(stringify!($name), ": invalid lowercase hex"),
                )?;
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = crate::ProtocolError;
            fn try_from(value: String) -> Result<Self> {
                Self::new(value)
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
hex_type!(Id, 32);
hex_type!(Hash, 64);
hex_type!(Nonce, 16);

impl Id {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(hex::encode(bytes))
    }
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut bytes = [0; 16];
        hex::decode_to_slice(&self.0, &mut bytes).expect("Id constructor validates hex");
        bytes
    }
}

fn non_null<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub device_id: Id,
    pub name: String,
    pub capabilities: Vec<String>,
    #[serde(
        default,
        deserialize_with = "non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub paused: Option<bool>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub token: Hash,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResult {
    pub accepted: bool,
    #[serde(
        default,
        deserialize_with = "non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub code: Option<PairCode>,
    #[serde(
        default,
        deserialize_with = "non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub message: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairCode {
    InvalidToken,
    ExpiredToken,
    PairingClosed,
    AlreadyPaired,
    StorageFailed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub nonce: Nonce,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub paused: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unpair {}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Screenshot,
    Photo,
    Recording,
    File,
    ClipboardImage,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileBegin {
    pub transfer_id: Id,
    pub file_name: String,
    pub mime: String,
    pub size: u64,
    pub sha256: Hash,
    pub kind: FileKind,
    pub created_at: u64,
    #[serde(
        default,
        deserialize_with = "non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub source_media_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunk {
    pub transfer_id: Id,
    pub offset: u64,
    /// In decoded JSON fixtures this is hex; on the wire these bytes are raw.
    #[serde(with = "hex_bytes")]
    pub data: Vec<u8>,
}
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        hex::decode(text).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEnd {
    pub transfer_id: Id,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub transfer_id: Id,
    pub deduplicated: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NackCode {
    HashMismatch,
    SizeMismatch,
    StorageFailed,
    DiskFull,
    NotWritable,
    UnsupportedKind,
    IdConflict,
    ClipboardUnavailable,
}
impl NackCode {
    pub fn retryable(self) -> bool {
        !matches!(self, Self::UnsupportedKind | Self::IdConflict)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nack {
    pub transfer_id: Id,
    pub code: NackCode,
    pub retryable: bool,
    #[serde(
        default,
        deserialize_with = "non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub message: Option<String>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardOrigin {
    #[default]
    Clipboard,
    Share,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clipboard {
    pub transfer_id: Id,
    pub text: String,
    pub created_at: u64,
    #[serde(default)]
    pub origin: ClipboardOrigin,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    VersionMismatch,
    Unauthorized,
    UnexpectedMessage,
    MalformedMessage,
    FrameTooLarge,
    UnknownMessage,
    Timeout,
    Busy,
    ConnectionReplaced,
    Closing,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMessage {
    pub code: ErrorCode,
    #[serde(
        default,
        deserialize_with = "non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub message: Option<String>,
    #[serde(
        default,
        deserialize_with = "non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub supported_version: Option<u32>,
}

/// JSON representation is for fixtures/debugging only; use the frame codec on wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    PairRequest(PairRequest),
    PairResult(PairResult),
    Ping(Ping),
    Pong(Ping),
    Status(Status),
    Unpair(Unpair),
    FileBegin(FileBegin),
    FileChunk(FileChunk),
    FileEnd(FileEnd),
    Ack(Ack),
    Nack(Nack),
    Clipboard(Clipboard),
    Error(ErrorMessage),
}

impl Message {
    pub fn type_id(&self) -> u8 {
        match self {
            Self::Hello(_) => 0x01,
            Self::PairRequest(_) => 0x02,
            Self::PairResult(_) => 0x03,
            Self::Ping(_) => 0x04,
            Self::Pong(_) => 0x05,
            Self::Status(_) => 0x06,
            Self::Unpair(_) => 0x07,
            Self::FileBegin(_) => 0x10,
            Self::FileChunk(_) => 0x11,
            Self::FileEnd(_) => 0x12,
            Self::Ack(_) => 0x13,
            Self::Nack(_) => 0x14,
            Self::Clipboard(_) => 0x20,
            Self::Error(_) => 0x7f,
        }
    }

    /// Checks stateless v1 constraints; callers must separately enforce session state.
    pub fn validate(&self) -> Result<()> {
        let diagnostic = |text: &Option<String>| -> Result<()> {
            if let Some(text) = text {
                string_limit(text, 0, 512, "message")?;
            }
            Ok(())
        };
        match self {
            Self::Hello(m) => {
                require(m.version > 0, "version must be positive")?;
                string_limit(&m.name, 1, 128, "name")?;
                require(m.capabilities.len() <= 16, "too many capabilities")?;
                let mut seen = HashSet::new();
                for capability in &m.capabilities {
                    string_limit(capability, 1, 64, "capability")?;
                    require(seen.insert(capability.as_str()), "duplicate capability")?;
                }
                require(
                    !seen.contains("clipboard_image") || seen.contains("files"),
                    "clipboard_image requires files",
                )?;
            }
            Self::PairResult(m) => {
                require(
                    if m.accepted {
                        m.code.is_none() && m.message.is_none()
                    } else {
                        m.code.is_some()
                    },
                    "invalid pairing result fields",
                )?;
                diagnostic(&m.message)?;
            }
            Self::FileBegin(m) => {
                string_limit(&m.file_name, 1, 255, "file_name")?;
                require(
                    m.file_name != "." && m.file_name != ".." && !m.file_name.contains(['/', '\\']),
                    "unsafe filename",
                )?;
                string_limit(&m.mime, 1, 255, "mime")?;
                require(
                    m.kind != FileKind::ClipboardImage
                        || (m.mime.starts_with("image/") && m.mime.len() > 6),
                    "clipboard_image requires an image MIME",
                )?;
                require(m.size <= MAX_FILE_SIZE, "file exceeds size limit")?;
                require(m.created_at <= MAX_TIMESTAMP, "invalid timestamp")?;
                if let Some(id) = &m.source_media_id {
                    string_limit(id, 1, 256, "source_media_id")?;
                }
            }
            Self::FileChunk(m) => {
                require(
                    !m.data.is_empty() && m.data.len() <= crate::CHUNK_SIZE,
                    "invalid chunk size",
                )?;
                require(
                    m.offset <= MAX_FILE_SIZE && m.data.len() as u64 <= MAX_FILE_SIZE - m.offset,
                    "chunk exceeds file size limit",
                )?;
            }
            Self::Nack(m) => {
                require(
                    m.retryable == m.code.retryable(),
                    "invalid retryable value for NACK code",
                )?;
                diagnostic(&m.message)?;
            }
            Self::Clipboard(m) => {
                string_limit(&m.text, 0, MAX_CLIPBOARD_BYTES, "clipboard text")?;
                require(m.created_at <= MAX_TIMESTAMP, "invalid timestamp")?;
            }
            Self::Error(m) => {
                require(
                    if m.code == ErrorCode::VersionMismatch {
                        m.supported_version.is_some_and(|v| v > 0)
                    } else {
                        m.supported_version.is_none()
                    },
                    "invalid supported_version for error",
                )?;
                diagnostic(&m.message)?;
            }
            Self::PairRequest(_)
            | Self::Ping(_)
            | Self::Pong(_)
            | Self::Status(_)
            | Self::Unpair(_)
            | Self::FileEnd(_)
            | Self::Ack(_) => {}
        }
        Ok(())
    }
}
