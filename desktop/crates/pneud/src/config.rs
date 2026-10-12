use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::Write,
    os::unix::fs::PermissionsExt,
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
            cache: base("XDG_CACHE_HOME", ".cache").join("pneu"),
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
        ensure!(
            !self.name.is_empty() && self.name.len() <= 128 && !self.name.contains('\0'),
            "invalid PC name"
        );
        ensure!(self.save_path.is_absolute(), "save_path must be absolute");
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
