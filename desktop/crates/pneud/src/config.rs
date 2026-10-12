use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
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
    pub port: u16,
    /// Cache only: completed files can expire; dedupe records never expire.
    pub cache_retention_days: u32,
    /// Reject new files above this local capacity policy (drain before NACK).
    pub max_file_bytes: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            name: "Pneu PC".into(),
            save_to_folder: true,
            save_path: PathBuf::from(env::var_os("HOME").unwrap_or_else(|| ".".into()))
                .join("Pictures/Pneu"),
            notification_preview: true,
            copy_image_to_clipboard: true,
            open_in_viewer: false,
            clipboard_sync_to_phone: true,
            port: pneu_proto::DEFAULT_PORT,
            cache_retention_days: 7,
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
        let config = if path.exists() {
            toml::from_str(std::str::from_utf8(&crate::storage::read_bounded(
                path, 65536,
            )?)?)?
        } else {
            let c = Self::default();
            c.save(path)?;
            c
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        config.validate()?;
        Ok(config)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        atomic_private(path, toml::to_string_pretty(self)?.as_bytes())
    }
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
    let tmp = path.with_extension(format!("{}-tmp", crate::random_hex(16)));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
pub fn sync_parent(path: &Path) -> Result<()> {
    fs::File::open(path.parent().context("no parent")?)?.sync_all()?;
    Ok(())
}
