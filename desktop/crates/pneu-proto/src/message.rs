use crate::{MAX_CLIPBOARD_BYTES, MAX_FILE_SIZE, MAX_TIMESTAMP, Result, require, string_limit};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;

// Serde's derived struct visitor also accepts positional sequences. Route wire
// objects through MapAccess, keeping field attributes/defaults in one place.
macro_rules! object_struct {
    (
        $(#[$attr:meta])* pub struct $name:ident {
            $($(#[$field_attr:meta])* pub $field:ident: $ty:ty),* $(,)?
        }
    ) => {
        $(#[$attr])*
        pub struct $name {
            $($(#[$field_attr])* pub $field: $ty),*
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                #[derive(Deserialize)]
                struct Fields {
                    $($(#[$field_attr])* $field: $ty),*
                }
                let Fields { $($field),* } = deserialize_object(deserializer)?;
                Ok(Self { $($field),* })
            }
        }
    };
}

fn deserialize_object<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct ObjectVisitor<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for ObjectVisitor<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a JSON object")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            map: A,
        ) -> std::result::Result<T, A::Error> {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_map(ObjectVisitor(std::marker::PhantomData))
}

// Derived unit enums also accept externally tagged objects. Decode through
// String so every wire enum has the same string-only JSON representation.
macro_rules! string_enum {
    (
        $(#[$attr:meta])* pub enum $name:ident {
            $($(#[$variant_attr:meta])* $variant:ident => $wire:literal),* $(,)?
        }
    ) => {
        $(#[$attr])*
        #[serde(try_from = "String")]
        pub enum $name {
            $($(#[$variant_attr])* #[serde(rename = $wire)] $variant),*
        }

        impl TryFrom<String> for $name {
            type Error = crate::ProtocolError;

            fn try_from(value: String) -> Result<Self> {
                match value.as_str() {
                    $($wire => Ok(Self::$variant),)*
                    _ => Err(crate::ProtocolError::InvalidValue(
                        concat!("invalid ", stringify!($name)).into(),
                    )),
                }
            }
        }
    };
}

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

object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
        #[serde(
            default,
            deserialize_with = "non_null",
            skip_serializing_if = "Option::is_none"
        )]
        pub role: Option<DeviceRole>,
        #[serde(
            default,
            deserialize_with = "non_null",
            skip_serializing_if = "Option::is_none"
        )]
        pub addresses: Option<Vec<String>>,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub setup: Option<SetupHello>,
    }
}
string_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum DeviceRole {
        Phone => "phone",
        Computer => "computer",
    }
}
string_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum SetupScope {
        Enabled => "enabled",
        Disabled => "disabled",
        Ask => "ask",
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct SetupHello {
        pub scope: SetupScope,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub setup_id: Option<Id>,
    }
}
string_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum MemberStatus {
        Member => "member",
        Removed => "removed",
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct MemberReach {
        pub addresses: Vec<String>,
        pub relay: Vec<serde_json::Value>,
        pub hlc: u64,
        pub writer_device: Id,
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct MemberReadmits {
        pub hlc: u64,
        pub writer_device: Id,
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct Member {
        pub device_id: Id,
        pub fingerprint: Hash,
        pub role: DeviceRole,
        pub name: String,
        pub status: MemberStatus,
        pub hlc: u64,
        pub writer_device: Id,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub reach: Option<MemberReach>,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub readmits: Option<MemberReadmits>,
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct Members {
        pub transfer_id: Id,
        pub setup_id: Id,
        pub members: Vec<Member>,
    }
}
impl Members {
    /// Semantic receipt identity ignores entry order, retaining address order.
    pub fn canonical(mut self) -> Self {
        self.members.sort_by(|a, b| {
            (a.device_id.as_str(), a.fingerprint.as_str())
                .cmp(&(b.device_id.as_str(), b.fingerprint.as_str()))
        });
        self
    }
    pub fn validate(&self) -> Result<()> {
        require(
            (1..=64).contains(&self.members.len()),
            "expected 1..=64 members",
        )?;
        let mut keys = HashSet::new();
        let mut active_ids = HashSet::new();
        let mut active_pins = HashSet::new();
        let mut incarnations = std::collections::HashMap::new();
        let mut active = 0;
        for member in &self.members {
            string_limit(&member.name, 1, 128, "member name")?;
            require(
                member.readmits.is_none() || member.status == MemberStatus::Member,
                "removed member has readmits",
            )?;
            require(
                keys.insert((&member.device_id, &member.fingerprint)),
                "duplicate member key",
            )?;
            let count = incarnations.entry(&member.device_id).or_insert(0);
            *count += 1;
            require(*count <= 4, "too many fingerprints for member ID")?;
            if member.status == MemberStatus::Member {
                active += 1;
                require(
                    active_ids.insert(&member.device_id),
                    "multiple active pins for member ID",
                )?;
                require(
                    active_pins.insert(&member.fingerprint),
                    "multiple active IDs for member pin",
                )?;
            }
            if let Some(reach) = &member.reach {
                require(
                    member.status == MemberStatus::Member,
                    "removed member has reach",
                )?;
                require(
                    reach.writer_device == member.device_id,
                    "reach writer must be subject",
                )?;
                require(reach.relay.is_empty(), "relay is reserved")?;
                require(
                    member.role != DeviceRole::Phone || reach.addresses.is_empty(),
                    "phone has addresses",
                )?;
                require(
                    reach.addresses.len() <= crate::MAX_CANDIDATE_ADDRESSES,
                    "too many reach addresses",
                )?;
                let mut seen = HashSet::new();
                for address in &reach.addresses {
                    crate::validate_address(address)?;
                    require(seen.insert(address), "duplicate reach address")?;
                }
            }
        }
        require(active <= 17, "too many active members")
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct PairRequest {
        pub token: Hash,
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
}
string_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum PairCode {
        InvalidToken => "invalid_token",
        ExpiredToken => "expired_token",
        PairingClosed => "pairing_closed",
        AlreadyPaired => "already_paired",
        StorageFailed => "storage_failed",
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct Ping {
        pub nonce: Nonce,
    }
}
object_struct! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    pub struct Status {
        pub paused: bool,
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct Unpair {}
}
string_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum FileKind {
        Screenshot => "screenshot",
        Photo => "photo",
        Recording => "recording",
        File => "file",
        ClipboardImage => "clipboard_image",
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
        #[serde(
            default,
            deserialize_with = "non_null",
            skip_serializing_if = "Option::is_none"
        )]
        pub hlc: Option<u64>,
        #[serde(
            default,
            deserialize_with = "non_null",
            skip_serializing_if = "Option::is_none"
        )]
        pub source_device: Option<Id>,
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct FileChunk {
        pub transfer_id: Id,
        pub offset: u64,
        /// In decoded JSON fixtures this is hex; on the wire these bytes are raw.
        #[serde(with = "hex_bytes")]
        pub data: Vec<u8>,
    }
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
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct FileEnd {
        pub transfer_id: Id,
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct Ack {
        pub transfer_id: Id,
        pub deduplicated: bool,
    }
}
string_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum NackCode {
        HashMismatch => "hash_mismatch",
        SizeMismatch => "size_mismatch",
        StorageFailed => "storage_failed",
        DiskFull => "disk_full",
        NotWritable => "not_writable",
        UnsupportedKind => "unsupported_kind",
        IdConflict => "id_conflict",
        ClipboardUnavailable => "clipboard_unavailable",
    }
}
impl NackCode {
    pub fn retryable(self) -> bool {
        !matches!(self, Self::UnsupportedKind | Self::IdConflict)
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
}
string_enum! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub enum ClipboardOrigin {
        #[default]
        Clipboard => "clipboard",
        Share => "share",
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct Clipboard {
        pub transfer_id: Id,
        pub text: String,
        pub created_at: u64,
        #[serde(default)]
        pub origin: ClipboardOrigin,
        #[serde(
            default,
            deserialize_with = "non_null",
            skip_serializing_if = "Option::is_none"
        )]
        pub hlc: Option<u64>,
        #[serde(
            default,
            deserialize_with = "non_null",
            skip_serializing_if = "Option::is_none"
        )]
        pub source_device: Option<Id>,
    }
}

object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct PeerSettings {
        pub transfer_id: Id,
        pub settings: Vec<PeerSetting>,
    }
}

object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct PeerSetting {
        /// `<from-device-id>/<to-device-id>/clipboard|automatic_media`.
        pub key: String,
        pub enabled: bool,
        pub hlc: u64,
        pub writer_device: Id,
    }
}

impl PeerSetting {
    pub fn validate(&self) -> Result<()> {
        let parts: Vec<_> = self.key.split('/').collect();
        require(parts.len() == 3, "invalid settings key")?;
        Id::new(parts[0])?;
        Id::new(parts[1])?;
        require(
            parts[0] != parts[1],
            "settings direction must join distinct devices",
        )?;
        require(
            matches!(parts[2], "clipboard" | "automatic_media"),
            "invalid settings feature",
        )?;
        require(
            self.writer_device.as_str() == parts[0] || self.writer_device.as_str() == parts[1],
            "settings writer must belong to pair",
        )
    }
}
string_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum ErrorCode {
        VersionMismatch => "version_mismatch",
        Unauthorized => "unauthorized",
        UnexpectedMessage => "unexpected_message",
        MalformedMessage => "malformed_message",
        FrameTooLarge => "frame_too_large",
        UnknownMessage => "unknown_message",
        Timeout => "timeout",
        Busy => "busy",
        ConnectionReplaced => "connection_replaced",
        Closing => "closing",
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
    PeerSettings(PeerSettings),
    Members(Members),
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
            Self::PeerSettings(_) => 0x21,
            Self::Members(_) => 0x22,
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
                require(
                    !seen.contains("setup")
                        || (seen.contains("mesh") && m.role.is_some() && m.setup.is_some()),
                    "setup requires mesh, role and setup object",
                )?;
                if let Some(setup) = &m.setup {
                    require(
                        setup.scope != SetupScope::Disabled || setup.setup_id.is_none(),
                        "disabled setup has ID",
                    )?;
                }
                if let Some(addresses) = &m.addresses {
                    require(
                        addresses.len() <= crate::MAX_CANDIDATE_ADDRESSES,
                        "too many addresses",
                    )?;
                    let mut seen = HashSet::new();
                    for address in addresses {
                        crate::validate_address(address)?;
                        require(seen.insert(address), "duplicate address")?;
                    }
                }
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
                require(
                    m.hlc.is_some() == m.source_device.is_some(),
                    "incomplete clipboard clock",
                )?;
                require(
                    m.hlc.is_none() || (m.mime.starts_with("image/") && m.mime.len() > 6),
                    "clipboard clock requires an image MIME",
                )?;
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
                require(
                    m.hlc.is_some() == m.source_device.is_some(),
                    "incomplete clipboard clock",
                )?;
            }
            Self::Members(m) => m.validate()?,
            Self::PeerSettings(m) => {
                require(
                    (1..=3).contains(&m.settings.len()),
                    "expected 1..=3 settings",
                )?;
                let mut seen = HashSet::new();
                for setting in &m.settings {
                    setting.validate()?;
                    require(seen.insert(&setting.key), "duplicate settings key")?;
                }
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
