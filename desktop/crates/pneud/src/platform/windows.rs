//! Windows locations and the per-user named-pipe namespace.
use super::*;
use anyhow::Context;
use windows_sys::Win32::UI::Shell::{FOLDERID_Downloads, FOLDERID_LocalAppData};
fn override_path(native: &str, xdg: &str, fallback: PathBuf) -> PathBuf {
    env::var_os(native)
        .map(PathBuf::from)
        .or_else(|| env::var_os(xdg).map(|p| PathBuf::from(p).join("pneu")))
        .unwrap_or(fallback)
}
fn local() -> Result<PathBuf> {
    // NT handle-relative I/O deliberately bypasses Win32 redirection. Resolve
    // the real package cache first so storage validation and SQLite agree on
    // identity, while keeping Downloads outside the package's lifetime.
    if crate::windows_package::packaged()? {
        let _apartment = crate::windows_package::Apartment::new()?;
        return Ok(PathBuf::from(
            ::windows::Storage::ApplicationData::Current()?
                .LocalCacheFolder()?
                .Path()?
                .to_string(),
        )
        .join("Local/Pneu"));
    }
    Ok(fs::known_folder(&FOLDERID_LocalAppData)?.join("Pneu"))
}
pub(crate) fn paths() -> Result<crate::Paths> {
    let local = local()?;
    let data = override_path("PNEU_DATA_DIR", "XDG_DATA_HOME", local.join("data"));
    let config = override_path("PNEU_CONFIG_DIR", "XDG_CONFIG_HOME", local.join("config"));
    let cache = cache_dir_from_env()?;
    let runtime_override = env::var_os("PNEU_RUNTIME_DIR")
        .or_else(|| env::var_os("XDG_RUNTIME_DIR"))
        .map(PathBuf::from);
    let runtime = runtime_override
        .as_ref()
        .cloned()
        .unwrap_or_else(|| local.join("run"));
    for path in [&data, &config, &cache, &runtime] {
        fs::validate_path(path).with_context(|| format!("invalid Pneu directory {path:?}"))?;
    }
    Ok(crate::Paths {
        data,
        config: config.join("config.toml"),
        cache,
        socket: crate::ipc::windows_pipe_name(runtime_override.as_deref())?,
    })
}
pub(crate) fn cache_dir_from_env() -> Result<PathBuf> {
    let path = override_path("PNEU_CACHE_DIR", "XDG_CACHE_HOME", local()?.join("cache"));
    fs::validate_path(&path).context("invalid cache directory")?;
    Ok(path)
}
pub(crate) fn received_files() -> PathBuf {
    fs::known_folder(&FOLDERID_Downloads)
        .expect("Windows Downloads known folder unavailable")
        .join("Pneu")
}
pub(crate) fn prepare_runtime(_socket: &Path) -> Result<()> {
    Ok(())
}
pub(crate) fn machine_hostname() -> String {
    env::var("COMPUTERNAME").unwrap_or_else(|_| "Pneu PC".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn environment_paths_reject_win32_aliases() -> Result<()> {
        const MARKER: &str = "PNEU_TEST_INVALID_WINDOWS_ENV";
        if env::var_os(MARKER).is_some() {
            assert!(paths().is_err());
            return Ok(());
        }
        for key in [
            "PNEU_DATA_DIR",
            "PNEU_CONFIG_DIR",
            "PNEU_CACHE_DIR",
            "PNEU_RUNTIME_DIR",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_RUNTIME_DIR",
        ] {
            for value in [
                r"C:\pneu-fixture\data.",
                r"C:\pneu-fixture\data ",
                r"C:\pneu-fixture\NUL",
                r"C:\pneu-fixture\a:b",
                r"\\?\C:\pneu-fixture",
            ] {
                let mut child = std::process::Command::new(env::current_exe()?);
                child
                    .args([
                        "--exact",
                        "platform::windows::tests::environment_paths_reject_win32_aliases",
                        "--test-threads=1",
                    ])
                    .env(MARKER, "1");
                for key in [
                    "PNEU_DATA_DIR",
                    "PNEU_CONFIG_DIR",
                    "PNEU_CACHE_DIR",
                    "PNEU_RUNTIME_DIR",
                    "XDG_DATA_HOME",
                    "XDG_CONFIG_HOME",
                    "XDG_CACHE_HOME",
                    "XDG_RUNTIME_DIR",
                ] {
                    child.env_remove(key);
                }
                let output = child.env(key, value).output()?;
                anyhow::ensure!(
                    output.status.success(),
                    "invalid {key}={value:?} was accepted: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        Ok(())
    }
    #[test]
    fn known_folder_defaults_and_environment_overrides() -> Result<()> {
        if let Ok(case) = env::var("PNEU_M2A_PATH_CASE") {
            let paths = paths()?;
            let base = local()?;
            let root = PathBuf::from(r"C:\pneu-m2a-path-fixture");
            let native = case == "native";
            let explicit = case == "explicit";
            for (actual, name) in [(&paths.data, "data"), (&paths.cache, "cache")] {
                let expected = if native {
                    base.join(name)
                } else if explicit {
                    root.join(name)
                } else {
                    root.join(name).join("pneu")
                };
                assert_eq!(*actual, expected);
            }
            let config = if native {
                base.join("config")
            } else if explicit {
                root.join("config")
            } else {
                root.join("config/pneu")
            };
            assert_eq!(paths.config, config.join("config.toml"));
            assert_eq!(
                received_files(),
                fs::known_folder(&FOLDERID_Downloads)?.join("Pneu")
            );
            prepare_runtime(&paths.socket)?;
            assert!(
                paths
                    .socket
                    .to_string_lossy()
                    .starts_with(r"\\.\pipe\pneu-S-")
            );
            return Ok(());
        }
        for case in ["native", "xdg", "explicit"] {
            let mut child = std::process::Command::new(env::current_exe()?);
            child
                .args([
                    "--exact",
                    "platform::windows::tests::known_folder_defaults_and_environment_overrides",
                ])
                .env("PNEU_M2A_PATH_CASE", case);
            for (native, xdg, name) in [
                ("PNEU_DATA_DIR", "XDG_DATA_HOME", "data"),
                ("PNEU_CONFIG_DIR", "XDG_CONFIG_HOME", "config"),
                ("PNEU_CACHE_DIR", "XDG_CACHE_HOME", "cache"),
                ("PNEU_RUNTIME_DIR", "XDG_RUNTIME_DIR", "run"),
            ] {
                child.env_remove(native).env_remove(xdg);
                if case != "native" {
                    child.env(xdg, format!(r"C:\pneu-m2a-path-fixture\{name}"));
                }
                if case == "explicit" {
                    child.env(native, format!(r"C:\pneu-m2a-path-fixture\{name}"));
                }
            }
            let output = child.output()?;
            anyhow::ensure!(
                output.status.success(),
                "path fixture failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }
}
