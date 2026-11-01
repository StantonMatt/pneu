/// Extension capability names. Defining a codec does not advertise support.
pub const CAP_FILE_RESUME: &str = "file_resume_v1";
pub const CAP_FOLDER: &str = "folder_v1";
pub const CAP_BATTERY: &str = "battery_v1";
pub const CAP_RING: &str = "ring_v1";
pub const CAP_NOTIFICATIONS: &str = "notifications_v1";
pub const FOLDER_MIME: &str = "application/vnd.pneu.folder";
pub const MAX_MANIFEST_BYTES: usize = 8_388_608;
pub const MAX_MANIFEST_ENTRIES: usize = 10_000;
pub const MAX_MANIFEST_JSON_DEPTH: usize = 64;
pub const MAX_FOLDER_DEPTH: usize = 32;
pub const MAX_FOLDER_PATH_BYTES: usize = 4096;
pub const MAX_FOLDER_COMPONENT_BYTES: usize = 255;
pub const MAX_NOTIFICATION_ICON_BYTES: usize = 8192;

object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct Folder { pub manifest_len: u64 }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct FileReady { pub transfer_id: Id, pub offset: u64 }
}
impl FileReady {
    /// The codec checks the global ceiling; the session must check the active BEGIN size.
    pub fn validate_offset(&self, begin_size: u64) -> Result<()> {
        require(
            begin_size <= MAX_FILE_SIZE && self.offset <= begin_size,
            "invalid READY offset",
        )
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct Cancel { pub transfer_id: Id }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct BatteryState {
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub level: Option<u8>,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub charging: Option<bool>,
    }
}
string_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum RingAction { Start => "start", Stop => "stop" }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct Ring { pub action: RingAction }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct RingState { pub ringing: bool }
}
string_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum NotificationAction { Clear => "clear", Remove => "remove", Upsert => "upsert" }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct NotificationEvent {
        pub action: NotificationAction,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub key: Option<Id>,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub app_package: Option<String>,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub app_label: Option<String>,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub time: Option<u64>,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub title: Option<String>,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub text: Option<String>,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub icon_png: Option<String>,
    }
}
impl NotificationEvent {
    pub fn validate(&self) -> Result<()> {
        let has_upsert = self.app_package.is_some()
            || self.app_label.is_some()
            || self.time.is_some()
            || self.title.is_some()
            || self.text.is_some()
            || self.icon_png.is_some();
        match self.action {
            NotificationAction::Clear => require(self.key.is_none() && !has_upsert, "clear fields"),
            NotificationAction::Remove => {
                require(self.key.is_some() && !has_upsert, "remove fields")
            }
            NotificationAction::Upsert => {
                require(
                    self.key.is_some() && self.time.is_some_and(|t| t <= MAX_TIMESTAMP),
                    "upsert key/time",
                )?;
                string_limit(
                    self.app_package.as_deref().unwrap_or(""),
                    1,
                    255,
                    "app_package",
                )?;
                string_limit(self.app_label.as_deref().unwrap_or(""), 1, 128, "app_label")?;
                if let Some(v) = &self.title {
                    string_limit(v, 0, 1024, "title")?;
                }
                if let Some(v) = &self.text {
                    string_limit(v, 0, 8192, "text")?;
                }
                if let Some(v) = &self.icon_png {
                    require(
                        (2..=MAX_NOTIFICATION_ICON_BYTES * 2).contains(&v.len())
                            && v.len() % 2 == 0
                            && v.bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                        "icon hex/length",
                    )?;
                    // Bounded invalid PNG data is permitted to fall back to a generic icon.
                    // Pixel/decompression limits belong to the renderer, never the wire parser.
                }
                Ok(())
            }
        }
    }
}
string_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum FolderEntryType { File => "file", Directory => "directory" }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct FolderEntry {
        pub path: String,
        #[serde(rename = "type")]
        pub entry_type: FolderEntryType,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub size: Option<u64>,
        #[serde(default, deserialize_with = "non_null", skip_serializing_if = "Option::is_none")]
        pub sha256: Option<Hash>,
    }
}
object_struct! {
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct FolderManifest {
        #[serde(deserialize_with = "bounded_manifest_entries")]
        pub entries: Vec<FolderEntry>
    }
}
// Validate tokens before Serde's typed pass: IgnoredAny deliberately does not
// validate Unicode in discarded strings. This scan allocates nothing and counts
// every value (including scalars), with the root at depth 1. Serde subsequently
// checks the complete JSON grammar and known-field schema.
fn validate_manifest_tokens(bytes: &[u8]) -> Result<()> {
    require(std::str::from_utf8(bytes).is_ok(), "manifest UTF-8")?;
    let mut i = 0;
    let mut containers = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' | b':' | b',' => i += 1,
            b'{' | b'[' => {
                containers += 1;
                require(containers <= MAX_MANIFEST_JSON_DEPTH, "manifest JSON depth")?;
                i += 1;
            }
            b'}' | b']' => {
                containers = containers.checked_sub(1).ok_or_else(|| {
                    crate::ProtocolError::InvalidValue("manifest JSON nesting".into())
                })?;
                i += 1;
            }
            b'"' => {
                i += 1;
                loop {
                    let b = *bytes.get(i).ok_or_else(|| {
                        crate::ProtocolError::InvalidValue("unterminated manifest string".into())
                    })?;
                    i += 1;
                    match b {
                        b'"' => break,
                        b'\\' => {
                            let escape = *bytes.get(i).ok_or_else(|| {
                                crate::ProtocolError::InvalidValue("manifest escape".into())
                            })?;
                            i += 1;
                            if escape == b'u' {
                                let code = manifest_hex_quad(bytes, &mut i)?;
                                if (0xd800..=0xdbff).contains(&code) {
                                    require(bytes.get(i..i + 2) == Some(b"\\u"), "lone surrogate")?;
                                    i += 2;
                                    let low = manifest_hex_quad(bytes, &mut i)?;
                                    require((0xdc00..=0xdfff).contains(&low), "lone surrogate")?;
                                } else {
                                    require(!(0xdc00..=0xdfff).contains(&code), "lone surrogate")?;
                                }
                            } else {
                                require(b"\"\\/bfnrt".contains(&escape), "manifest escape")?;
                            }
                        }
                        0..=31 => return require(false, "manifest string control"),
                        _ => {}
                    }
                }
                // Member names do not add depth. A string value does, even in
                // an otherwise unknown object. Grammar is checked by Serde.
                let next = bytes[i..].iter().find(|b| !b.is_ascii_whitespace());
                if next != Some(&b':') {
                    require(containers < MAX_MANIFEST_JSON_DEPTH, "manifest JSON depth")?;
                }
            }
            _ => {
                require(containers < MAX_MANIFEST_JSON_DEPTH, "manifest JSON depth")?;
                // A primitive token ends at JSON whitespace or punctuation.
                i += 1;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && !b"{}[]:,\"".contains(&bytes[i])
                {
                    i += 1;
                }
            }
        }
    }
    Ok(())
}
fn manifest_hex_quad(bytes: &[u8], i: &mut usize) -> Result<u16> {
    let mut code = 0;
    for _ in 0..4 {
        let digit = bytes
            .get(*i)
            .and_then(|b| (*b as char).to_digit(16))
            .ok_or_else(|| crate::ProtocolError::InvalidValue("manifest Unicode escape".into()))?;
        code = code * 16 + digit as u16;
        *i += 1;
    }
    Ok(code)
}
fn bounded_manifest_entries<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<FolderEntry>, D::Error> {
    struct Entries;
    impl<'de> serde::de::Visitor<'de> for Entries {
        type Value = Vec<FolderEntry>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most 10000 folder entries")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut entries = Vec::new();
            while entries.len() < MAX_MANIFEST_ENTRIES {
                let Some(entry) = seq.next_element()? else {
                    return Ok(entries);
                };
                entries.push(entry);
            }
            // The seed rejects before deserializing even one byte of entry 10001.
            struct Excess;
            impl<'de> serde::de::DeserializeSeed<'de> for Excess {
                type Value = ();
                fn deserialize<T: Deserializer<'de>>(
                    self,
                    _: T,
                ) -> std::result::Result<(), T::Error> {
                    Err(serde::de::Error::custom("manifest entry count"))
                }
            }
            seq.next_element_seed(Excess)?;
            Ok(entries)
        }
    }
    deserializer.deserialize_seq(Entries)
}
/// Structural checks only: no filesystem access, platform mapping, or percent decoding.
pub fn validate_folder_path(path: &str) -> Result<()> {
    string_limit(path, 1, MAX_FOLDER_PATH_BYTES, "folder path")?;
    require(!path.contains('\\'), "backslash in folder path")?;
    let parts: Vec<_> = path.split('/').collect();
    require(parts.len() <= MAX_FOLDER_DEPTH, "folder depth")?;
    for part in parts {
        string_limit(part, 1, MAX_FOLDER_COMPONENT_BYTES, "folder component")?;
        require(part != "." && part != "..", "relative folder escape")?;
        let b = part.as_bytes();
        require(
            !(b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'),
            "drive-shaped component",
        )?;
    }
    Ok(())
}
impl FolderManifest {
    /// Validate exact stream bytes against BEGIN before any destination allocation.
    pub fn parse(bytes: &[u8], begin: &FileBegin) -> Result<Self> {
        let folder = begin
            .folder
            .as_ref()
            .ok_or_else(|| crate::ProtocolError::InvalidValue("missing folder".into()))?;
        Message::FileBegin(begin.clone()).validate()?;
        require(
            bytes.len() <= MAX_MANIFEST_BYTES && bytes.len() as u64 == folder.manifest_len,
            "manifest byte length",
        )?;
        validate_manifest_tokens(bytes)?;
        let manifest: Self = serde_json::from_slice(bytes)?;
        manifest.validate(folder.manifest_len, begin.size)?;
        Ok(manifest)
    }
    pub fn validate(&self, manifest_len: u64, stream_size: u64) -> Result<()> {
        require(
            (1..=MAX_MANIFEST_BYTES as u64).contains(&manifest_len)
                && manifest_len <= stream_size
                && stream_size <= MAX_FILE_SIZE,
            "manifest/stream size",
        )?;
        require(
            self.entries.len() <= MAX_MANIFEST_ENTRIES,
            "manifest entry count",
        )?;
        let mut directories = HashSet::new();
        let mut paths = HashSet::new();
        let mut total = manifest_len;
        for entry in &self.entries {
            validate_folder_path(&entry.path)?;
            require(paths.insert(entry.path.as_str()), "duplicate folder path")?;
            if let Some((parent, _)) = entry.path.rsplit_once('/') {
                require(
                    directories.contains(parent),
                    "missing preceding ancestor directory",
                )?;
            }
            match entry.entry_type {
                FolderEntryType::Directory => {
                    require(
                        entry.size.is_none() && entry.sha256.is_none(),
                        "directory size/hash",
                    )?;
                    directories.insert(entry.path.as_str());
                }
                FolderEntryType::File => {
                    let size = entry
                        .size
                        .filter(|s| *s <= MAX_FILE_SIZE)
                        .ok_or_else(|| crate::ProtocolError::InvalidValue("file size".into()))?;
                    require(entry.sha256.is_some(), "file hash")?;
                    require(size != 0 || entry.sha256.as_ref().is_some_and(|h| h.as_str() ==
                        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"), "empty file hash")?;
                    total = total
                        .checked_add(size)
                        .filter(|t| *t <= MAX_FILE_SIZE)
                        .ok_or_else(|| {
                            crate::ProtocolError::InvalidValue("folder size overflow".into())
                        })?;
                }
            }
        }
        require(total == stream_size, "folder stream sum")
    }
}
