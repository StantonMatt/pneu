use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
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
        let home = PathBuf::from(env::var_os("HOME").context("HOME is unset")?);
        let base = |key: &str, fallback: &str| {
            env::var_os(key)
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
        };
        let runtime =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is unset")?);
        Ok(Self {
            data: base("XDG_DATA_HOME", ".local/share").join("pneu"),
            cache: cache_dir_from_env()?,
            config: base("XDG_CONFIG_HOME", ".config").join("pneu/config.toml"),
            socket: runtime.join("pneud.sock"),
        })
    }
    pub fn prepare(&self) -> Result<()> {
        for path in [&self.data, self.config.parent().context("config parent")?] {
            private_dir(path)?;
        }
        if let Err(error) = private_dir(&self.cache) {
            eprintln!("pneud: cache directory unavailable: {error}");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub name: String,
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
            save_to_folder: true,
            save_path: PathBuf::from(env::var_os("HOME").unwrap_or_else(|| ".".into()))
                .join("Pictures/Pneu"),
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
        ensure!(self.save_path.is_absolute(), "save_path must be absolute");
        ensure!(cache.is_absolute(), "cache directory must be absolute");
        ensure!(
            !save_folder_overlaps_cache(&self.save_path, cache),
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
        if path.exists() {
            ensure!(
                fs::symlink_metadata(path)?.file_type().is_file(),
                "config must be a regular file, not a symlink"
            );
            ensure!(fs::metadata(path)?.len() <= 65536, "config is too large");
        }
        let mut config: Self = if path.exists() {
            toml::from_str(std::str::from_utf8(&crate::storage::read_bounded(
                path, 65536,
            )?)?)?
        } else {
            let c = Self::default();
            c.save(path)?;
            c
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        if config.name == "Pneu PC" {
            config.name = machine_hostname();
            config.save(path)?;
        }
        config.validate()?;
        Ok(config)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        atomic_private(path, toml::to_string_pretty(self)?.as_bytes())
    }
}
// Unavailable storage remains recoverable through per-transfer errors/fallback.
// Only successful resolution of both paths can prove an unsafe destination.
pub(crate) fn save_folder_overlaps_cache(save: &Path, cache: &Path) -> bool {
    match (resolve_folder(save, &mut 0), resolve_folder(cache, &mut 0)) {
        (Ok(save), Ok(cache)) => save.starts_with(cache),
        _ => false,
    }
}
fn cache_dir_from_env() -> Result<PathBuf> {
    Ok(env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or(PathBuf::from(env::var_os("HOME").context("HOME is unset")?).join(".cache"))
        .join("pneu"))
}

// Resolve existing symlinks while retaining missing components: choosing a
// folder that has not been created yet must enforce the same cache boundary.
fn resolve_folder(path: &Path, links: &mut usize) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut components = path.components();
    while let Some(component) = components.next() {
        match component {
            Component::RootDir => resolved.push("/"),
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
            Component::Prefix(_) => anyhow::bail!("unsupported folder prefix"),
        }
    }
    Ok(resolved)
}
fn machine_hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}
pub fn private_dir(path: &Path) -> Result<()> {
    let folder = crate::storage::Folder::open_private(path)?;
    folder
        .file
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    folder.file.sync_all()?;
    Ok(())
}
pub fn atomic_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let folder = crate::storage::Folder::open(path.parent().context("no parent")?, false)?;
    atomic_private_in(&folder, path.file_name().context("file name")?, bytes)
}
/// Write and replace one entry within an already retained, no-follow parent.
pub(crate) fn atomic_private_in(
    folder: &crate::storage::Folder,
    destination: &std::ffi::OsStr,
    bytes: &[u8],
) -> Result<()> {
    ensure!(
        Path::new(destination).file_name() == Some(destination),
        "invalid file name"
    );
    let tmp = format!("pneu-{}-tmp", crate::random_hex(16));
    let name = tmp.as_str();
    let mut file = fs::File::from(rustix::fs::openat(
        &*folder.file,
        name,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )?);
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        rustix::fs::renameat(&*folder.file, name, &*folder.file, destination)?;
        folder.file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = crate::storage::remove_file(&folder.file, name, &file);
    }
    result
}
pub fn sync_parent(path: &Path) -> Result<()> {
    fs::File::open(path.parent().context("no parent")?)?.sync_all()?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
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
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
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
}
