//! Private clipboard fingerprints: HMAC-SHA256 over a representation's SHA-256.
//! Prehashing lets the bounded watcher/helper report representations without
//! handing plaintext to the actor. No unkeyed digest is persisted by the actor.
use crate::platform::fs as platform_fs;
use anyhow::{Context, Result, ensure};
use ring::hmac;
use std::{io::Read, path::Path};

#[derive(Clone)]
pub(crate) struct FingerprintKey([u8; 32]);
impl Default for FingerprintKey {
    fn default() -> Self {
        Self(rand::random())
    }
}
impl FingerprintKey {
    pub(crate) fn load_or_create(path: &Path) -> Result<Self> {
        let file = platform_fs::open_secret(path);
        let mut file = match file {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::default();
                if crate::config::atomic_private_new(path, &key.0)? {
                    return Ok(key);
                }
                return Self::load_or_create(path);
            }
            Err(e) => return Err(e).context("open clipboard fingerprint key"),
        };
        crate::platform::private::check(&file)?;
        let meta = file.metadata()?;
        ensure!(
            platform_fs::is_private_file(&meta),
            "clipboard fingerprint key must be an owned 0600 regular file"
        );
        if meta.len() != 32 {
            // Pre-release direct writes could leave a truncated final key.
            // Store holds its exclusive ownership lock; replace the corrupt
            // secret atomically. The database key tag retires only keyed echo
            // fingerprints and text receipts, including after a recovery crash.
            let key = Self::default();
            crate::config::atomic_private(path, &key.0)?;
            return Ok(key);
        }
        let mut key = [0; 32];
        file.read_exact(&mut key)?;
        Ok(Self(key))
    }
    /// Called only under Store's exclusive ownership lock, before creation.
    /// Never sweep another concurrent startup's in-progress key write.
    pub(crate) fn cleanup_temporary(path: &Path) -> Result<()> {
        let parent = path.parent().context("key parent")?;
        let folder = crate::storage::Folder::open(parent, false)?;
        let prefix = format!(
            ".pneu-{}-",
            path.file_name().context("key name")?.to_string_lossy()
        );
        for entry in platform_fs::entries(&folder.file)? {
            let name = entry?;
            let name = name.as_os_str();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let Some(random) = name_str
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_suffix("-tmp"))
            else {
                continue;
            };
            if random.len() != 32 || !random.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let file = match platform_fs::open_read_at(&folder.file, name) {
                Ok(file) => file,
                Err(error) if platform_fs::is_link(&error) || platform_fs::is_missing(&error) => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let meta = file.metadata()?;
            if platform_fs::is_private_file(&meta) {
                crate::platform::private::check(&file)?;
                crate::storage::remove_file(&folder.file, name, &file)?;
            }
        }
        folder.file.sync_all()?;
        Ok(())
    }
    pub(crate) fn digest(&self, digest: &[u8; 32]) -> [u8; 32] {
        hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &self.0), digest)
            .as_ref()
            .try_into()
            .expect("HMAC-SHA256")
    }
    pub(crate) fn hashes(&self, hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
        hashes.iter().map(|h| self.digest(h)).collect()
    }
}
