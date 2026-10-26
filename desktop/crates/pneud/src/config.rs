use crate::platform::fs as platform_fs;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
#[cfg(any(unix, test))]
use std::fs;
#[cfg(unix)]
use std::path::Component;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Paths {
    pub data: PathBuf,
    pub cache: PathBuf,
    pub config: PathBuf,
    pub socket: PathBuf,
}
impl Paths {
    pub fn from_env() -> Result<Self> {
        crate::platform::paths()
    }
    pub fn prepare_runtime(&self) -> Result<()> {
        crate::platform::prepare_runtime(&self.socket)
    }
    pub fn prepare(&self) -> Result<()> {
        #[cfg(windows)]
        for path in [&self.data, &self.config, &self.cache, &self.socket] {
            platform_fs::validate_path(path)?;
        }
        for path in [&self.data, self.config.parent().context("config parent")?] {
            private_dir(path)?;
        }
        if let Err(error) = private_dir(&self.cache) {
            eprintln!("pneud: cache directory unavailable: {error}");
        }
        crate::platform::prepare_runtime(&self.socket)?;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardMode {
    #[default]
    Automatic,
    Manual,
    Off,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub name: String,
    pub clipboard_mode: ClipboardMode,
    pub choose_devices_each_time: bool,
    pub queue_offline: bool,
    pub save_to_folder: bool,
    pub save_path: PathBuf,
    pub notification_preview: bool,
    pub copy_image_to_clipboard: bool,
    pub open_in_viewer: bool,
    pub clipboard_sync_to_phone: bool,
    pub clipboard_include_images: bool,
    pub port: u16,
    /// Cache only: completed files can expire; dedupe records never expire.
    pub cache_retention_days: u32,
    /// Reject new files above this local capacity policy (drain before NACK).
    pub max_file_bytes: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            name: machine_hostname(),
            clipboard_mode: ClipboardMode::Automatic,
            choose_devices_each_time: false,
            queue_offline: false,
            save_to_folder: true,
            save_path: crate::platform::received_files(),
            notification_preview: true,
            copy_image_to_clipboard: true,
            open_in_viewer: false,
            clipboard_sync_to_phone: true,
            clipboard_include_images: false,
            port: pneu_proto::DEFAULT_PORT,
            cache_retention_days: 1,
            max_file_bytes: pneu_proto::MAX_FILE_SIZE,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        self.validate_with_cache(&cache_dir_from_env()?)
    }
    pub(crate) fn validate_with_cache(&self, cache: &Path) -> Result<()> {
        ensure!(
            !self.name.is_empty() && self.name.len() <= 128 && !self.name.contains('\0'),
            "invalid PC name"
        );
        #[cfg(windows)]
        for path in [&self.save_path, cache] {
            platform_fs::validate_path(path)?;
        }
        ensure!(self.save_path.is_absolute(), "save_path must be absolute");
        ensure!(cache.is_absolute(), "cache directory must be absolute");
        #[cfg(windows)]
        let overlaps = platform_fs::configured_folder_overlaps_cache(&self.save_path, cache)?;
        #[cfg(unix)]
        let overlaps = save_folder_overlaps_cache(&self.save_path, cache);
        ensure!(
            !overlaps,
            "save folder must not be the daemon cache directory or a folder inside it"
        );
        ensure!(self.port != 0, "port must be nonzero");
        ensure!(
            self.max_file_bytes <= pneu_proto::MAX_FILE_SIZE,
            "invalid file quota"
        );
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        #[cfg(windows)]
        platform_fs::validate_path(path)?;
        #[cfg(windows)]
        let folder = crate::storage::Folder::open(path.parent().context("config parent")?, false)
            .context("cannot open config directory")?;
        #[cfg(windows)]
        let name = path.file_name().context("config name")?;
        #[cfg(unix)]
        if path.exists() {
            ensure!(
                fs::symlink_metadata(path)?.file_type().is_file(),
                "config must be a regular file, not a symlink"
            );
            ensure!(fs::metadata(path)?.len() <= 65536, "config is too large");
        }
        #[cfg(windows)]
        let bytes = platform_fs::private_bytes_at(&folder.file, name, 65536)?;
        #[cfg(unix)]
        let bytes = if path.exists() {
            Some(crate::storage::read_bounded(path, 65536)?)
        } else {
            None
        };
        let mut config: Self = if let Some(bytes) = bytes {
            let text = std::str::from_utf8(&bytes)?;
            let mut config: Self = toml::from_str(text)?;
            let fields: toml::Value = toml::from_str(text)?;
            if fields.get("clipboard_mode").is_none() {
                if !config.clipboard_sync_to_phone {
                    config.clipboard_mode = ClipboardMode::Off;
                }
            } else {
                config.clipboard_sync_to_phone = config.clipboard_mode != ClipboardMode::Off;
            }
            config
        } else {
            let c = Self::default();
            #[cfg(unix)]
            c.save(path)?;
            #[cfg(windows)]
            {
                c.validate()?;
                atomic_private_in(&folder, name, toml::to_string_pretty(&c)?.as_bytes())?;
            }
            c
        };
        #[cfg(unix)]
        platform_fs::set_private_file(path)?;
        if config.name == "Pneu PC" {
            config.name = machine_hostname();
            #[cfg(unix)]
            config.save(path)?;
            #[cfg(windows)]
            {
                config.validate()?;
                atomic_private_in(&folder, name, toml::to_string_pretty(&config)?.as_bytes())?;
            }
        }
        config.validate()?;
        Ok(config)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        atomic_private(path, toml::to_string_pretty(self)?.as_bytes())
    }
}
#[cfg(windows)]
pub(crate) fn save_folder_overlaps_cache(save: &Path, cache: &Path) -> bool {
    // Transfer preflight fails closed, allowing the caller's NotWritable
    // fallback. Placement independently checks the retained Folder again.
    platform_fs::folder_overlaps_cache(save, cache).unwrap_or(true)
}
// Unavailable storage remains recoverable through per-transfer errors/fallback.
// Only successful resolution of both paths can prove an unsafe destination.
#[cfg(unix)]
pub(crate) fn save_folder_overlaps_cache(save: &Path, cache: &Path) -> bool {
    match (resolve_folder(save, &mut 0), resolve_folder(cache, &mut 0)) {
        (Ok(save), Ok(cache)) => save.starts_with(cache),
        _ => false,
    }
}
use crate::platform::cache_dir_from_env;

// Resolve existing symlinks while retaining missing components: choosing a
// folder that has not been created yet must enforce the same cache boundary.
#[cfg(unix)]
fn resolve_folder(path: &Path, links: &mut usize) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut components = path.components();
    while let Some(component) = components.next() {
        match component {
            Component::RootDir | Component::Prefix(_) => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match fs::symlink_metadata(&resolved) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        ensure!(*links < 40, "too many folder symlinks");
                        *links += 1;
                        let target = fs::read_link(&resolved)?;
                        let redirected = resolved
                            .parent()
                            .context("symlink parent")?
                            .join(target)
                            .join(components.as_path());
                        return resolve_folder(&redirected, links);
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok(resolved)
}
use crate::platform::machine_hostname;
pub fn private_dir(path: &Path) -> Result<()> {
    let folder = crate::storage::Folder::open_private(path)?;
    platform_fs::set_private_directory(&folder.file)?;
    folder.file.sync_all()?;
    Ok(())
}
pub fn atomic_private(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(windows)]
    platform_fs::validate_path(path)?;
    let folder = crate::storage::Folder::open(path.parent().context("no parent")?, false)?;
    atomic_private_in(&folder, path.file_name().context("file name")?, bytes)
}
/// Write and replace one entry within an already retained, no-follow parent.
pub(crate) fn atomic_private_in(
    folder: &crate::storage::Folder,
    destination: &std::ffi::OsStr,
    bytes: &[u8],
) -> Result<()> {
    publish_private_in(folder, destination, bytes, true, || Ok(())).map(|_| ())
}
/// First-writer-wins publication for durable secrets. A racing creator loads
/// the winner instead of replacing a key another process may already use.
pub(crate) fn atomic_private_new(path: &Path, bytes: &[u8]) -> Result<bool> {
    #[cfg(windows)]
    platform_fs::validate_path(path)?;
    let folder = crate::storage::Folder::open(path.parent().context("no parent")?, false)?;
    publish_private_in(
        &folder,
        path.file_name().context("file name")?,
        bytes,
        false,
        || Ok(()),
    )
}
#[cfg(windows)]
pub(crate) fn atomic_private_new_in(
    folder: &crate::storage::Folder,
    name: &std::ffi::OsStr,
    bytes: &[u8],
) -> Result<bool> {
    publish_private_in(folder, name, bytes, false, || Ok(()))
}
fn publish_private_in(
    folder: &crate::storage::Folder,
    destination: &std::ffi::OsStr,
    bytes: &[u8],
    replace: bool,
    before_write: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    ensure!(
        Path::new(destination).file_name() == Some(destination),
        "invalid file name"
    );
    #[cfg(windows)]
    platform_fs::validate_name(destination)?;
    let tmp = format!(
        ".pneu-{}-{}-tmp",
        destination
            .to_string_lossy()
            .chars()
            .take(48)
            .collect::<String>(),
        crate::random_hex(16)
    );
    let name = tmp.as_str();
    let mut file = platform_fs::create_private_at(&folder.file, name, platform_fs::Access::Write)?;
    let result = (|| {
        crate::platform::private::created(&file)?;
        before_write()?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match platform_fs::rename(&folder.file, name, &folder.file, destination, replace) {
            Ok(()) => {}
            Err(error) if !replace && platform_fs::is_exists(&error) => return Ok(false),
            Err(error) => return Err(error),
        }
        folder.file.sync_all()?;
        Ok(true)
    })();
    if !matches!(result, Ok(true)) {
        let _ = crate::storage::remove_file(&folder.file, name, &file);
    }
    result
}
pub fn sync_parent(path: &Path) -> Result<()> {
    platform_fs::sync_parent(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(unix)]
    fn unresolvable_folder_errors_do_not_reject_settings() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("file");
        fs::write(&file, b"not a directory")?;
        Config {
            save_path: file.join("cache/save"),
            ..Config::default()
        }
        .validate_with_cache(&file.join("cache"))?;
        let looping = dir.path().join("loop");
        std::os::unix::fs::symlink("loop", &looping)?;
        Config {
            save_path: looping.join("save"),
            ..Config::default()
        }
        .validate_with_cache(&looping)?;
        Ok(())
    }
    #[test]
    #[cfg(unix)]
    fn save_folder_validation_rejects_cache_and_resolved_descendants() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = dir.path().join("cache");
        fs::create_dir(&cache)?;
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink("cache", &alias)?;
        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink("cache/new", &dangling)?;
        let cache_alias = dir.path().join("cache-alias");
        std::os::unix::fs::symlink(&cache, &cache_alias)?;
        for folder in [
            cache.clone(),
            cache.join("new/nested"),
            alias.clone(),
            alias.join("new/nested"),
            dangling.join("child"),
            dir.path().join("missing/../cache/new"),
        ] {
            for configured_cache in [&cache, &cache_alias] {
                let config = Config {
                    save_path: folder.clone(),
                    ..Config::default()
                };
                assert!(
                    config
                        .validate_with_cache(configured_cache)
                        .unwrap_err()
                        .to_string()
                        .contains("save folder must not be the daemon cache directory")
                );
            }
        }
        for folder in [
            dir.path().join("cache-sibling/new"),
            dir.path().to_owned(),
            alias.join("../saved"),
        ] {
            Config {
                save_path: folder,
                ..Config::default()
            }
            .validate_with_cache(&cache)?;
        }
        // Enforce the same boundary even before the cache root exists.
        Config {
            save_path: cache.join("../future/nested"),
            ..Config::default()
        }
        .validate_with_cache(&dir.path().join("future"))
        .expect_err("uncreated cache subtree must be rejected");
        // The public config validation uses the actual environment cache root.
        Config {
            save_path: cache_dir_from_env()?.join("new"),
            ..Config::default()
        }
        .validate()
        .expect_err("environment cache subtree must be rejected");
        Ok(())
    }
    #[test]
    fn default_name_is_the_machine_hostname() -> Result<()> {
        let hostname = machine_hostname();
        assert_eq!(Config::default().name, hostname);
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        assert_eq!(Config::load(&path)?.name, hostname);
        fs::write(&path, "port = 47822\n")?;
        assert_eq!(Config::load(&path)?.name, hostname);
        Ok(())
    }
    #[test]
    fn legacy_name_is_migrated_and_saved_without_changing_preferences() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        let legacy = Config {
            name: "Pneu PC".into(),
            notification_preview: false,
            save_path: dir.path().join("pictures"),
            port: 47822,
            ..Config::default()
        };
        legacy.save(&path)?;
        let loaded = Config::load(&path)?;
        assert_eq!(loaded.name, machine_hostname());
        assert!(!loaded.notification_preview);
        assert_eq!(loaded.save_path, legacy.save_path);
        assert_eq!(loaded.port, legacy.port);
        let saved: Config = toml::from_str(&fs::read_to_string(&path)?)?;
        assert_eq!(saved.name, loaded.name);
        assert!(platform_fs::is_private_file(&platform_fs::open_read(
            &path
        )?)?);
        Ok(())
    }
    #[test]
    fn custom_name_is_preserved_on_load() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        let custom = Config {
            name: "My Pneu PC".into(),
            ..Config::default()
        };
        custom.save(&path)?;
        let before = fs::read(&path)?;
        assert_eq!(Config::load(&path)?.name, custom.name);
        assert_eq!(fs::read(&path)?, before);
        Ok(())
    }
    #[test]
    fn failed_atomic_write_cleans_its_temporary_file_and_preserves_destination() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let destination = dir.path().join("config");
        fs::create_dir(&destination)?;
        fs::write(destination.join("keep"), b"foreign")?;
        assert!(atomic_private(&destination, b"new config").is_err());
        assert_eq!(fs::read(destination.join("keep"))?, b"foreign");
        assert_eq!(fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }
    #[test]
    #[cfg(unix)]
    fn fingerprint_write_failure_before_payload_and_interrupted_temp_recover_on_restart()
    -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let folder = crate::storage::Folder::open(dir.path(), false)?;
        let key = dir.path().join("clipboard-fingerprint.key");
        let result = publish_private_in(&folder, key.file_name().unwrap(), &[7; 32], false, || {
            Err(std::io::Error::from_raw_os_error(rustix::io::Errno::NOSPC.raw_os_error()).into())
        });
        assert!(result.is_err());
        assert!(!key.exists());
        assert_eq!(fs::read_dir(dir.path())?.count(), 0);
        // Simulate process death after creating the private staging file.
        let orphan = dir.path().join(format!(
            ".pneu-clipboard-fingerprint.key-{}-tmp",
            "a".repeat(32)
        ));
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&orphan)?;
        let unrelated = dir.path().join("keep");
        fs::write(&unrelated, b"keep")?;
        let db = dir.path().join("state.sqlite3");
        let store = crate::store::Store::open(&db)?;
        assert!(!orphan.exists());
        assert_eq!(fs::read(&unrelated)?, b"keep");
        let bytes = fs::read(&key)?;
        assert_eq!(bytes.len(), 32);
        drop(store);
        drop(crate::store::Store::open(&db)?);
        assert_eq!(fs::read(&key)?, bytes);
        Ok(())
    }
    #[test]
    fn atomic_new_concurrent_creators_keep_one_complete_private_winner() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("key");
        let results = std::thread::scope(|scope| {
            (1..=8u8)
                .map(|n| {
                    let path = &path;
                    scope.spawn(move || atomic_private_new(path, &[n; 32]))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|t| t.join().unwrap())
                .collect::<Result<Vec<_>>>()
        })?;
        assert_eq!(results.iter().filter(|r| **r).count(), 1);
        let bytes = fs::read(&path)?;
        assert_eq!(bytes.len(), 32);
        assert!(bytes.iter().all(|b| *b == bytes[0]));
        assert!(platform_fs::is_private_file(&platform_fs::open_read(
            &path
        )?)?);
        assert_eq!(fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }
}
