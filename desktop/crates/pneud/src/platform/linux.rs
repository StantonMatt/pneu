use super::*;

pub(crate) fn paths() -> Result<crate::Paths> {
    let home = PathBuf::from(env::var_os("HOME").context("HOME is unset")?);
    let base = |key: &str, fallback: &str| {
        env::var_os(key)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(fallback))
    };
    let runtime =
        PathBuf::from(env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is unset")?);
    Ok(crate::Paths {
        data: base("XDG_DATA_HOME", ".local/share").join("pneu"),
        cache: cache_dir_from_env()?,
        config: base("XDG_CONFIG_HOME", ".config").join("pneu/config.toml"),
        socket: runtime.join("pneud.sock"),
    })
}
pub(crate) fn cache_dir_from_env() -> Result<PathBuf> {
    Ok(env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or(PathBuf::from(env::var_os("HOME").context("HOME is unset")?).join(".cache"))
        .join("pneu"))
}

pub(crate) fn received_files() -> PathBuf {
    dirs::picture_dir()
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("HOME").unwrap_or_else(|| ".".into())).join("Pictures")
        })
        .join("Pneu")
}
pub(crate) fn prepare_runtime(_socket: &Path) -> Result<()> {
    Ok(())
}
pub(crate) fn machine_hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use std::{fs, process::Command};

    #[test]
    fn received_files_follow_xdg_and_preserve_saved_settings() -> Result<()> {
        if let Some(expected) = env::var_os("PNEU_TEST_PICTURES_EXPECTED") {
            let expected = PathBuf::from(expected);
            assert_eq!(received_files(), expected);
            assert_eq!(Config::default().save_path, expected);
            assert_eq!(toml::from_str::<Config>("")?.save_path, expected);

            // Exercise first-run persistence and an existing explicit preference.
            let home = PathBuf::from(env::var_os("HOME").unwrap());
            let config_path = home.join("config.toml");
            assert_eq!(Config::load(&config_path)?.save_path, expected);
            for saved_path in [home.join("Pictures/Pneu"), home.join("My received files")] {
                let config = Config {
                    save_path: saved_path.clone(),
                    ..Config::default()
                };
                config.save(&config_path)?;
                let before = fs::read(&config_path)?;
                assert_eq!(Config::load(&config_path)?.save_path, saved_path);
                assert_eq!(fs::read(&config_path)?, before);
            }

            if let Ok(relative) = expected.strip_prefix(&home) {
                assert_eq!(
                    crate::ui::display_path(&expected),
                    format!("~/{}", relative.display())
                );
            }
            return Ok(());
        }

        for (contents, pictures) in [
            (Some("XDG_PICTURES_DIR=\"$HOME/Imágenes\"\n"), "Imágenes"),
            (
                Some("  XDG_PICTURES_DIR = \"$HOME/Mis imágenes\"  \n"),
                "Mis imágenes",
            ),
            (
                Some("XDG_PICTURES_DIR=\"$HOME/Fotos \\\"familia\\\"\"\n"),
                "Fotos \"familia\"",
            ),
            (None, "Pictures"),
            (Some("XDG_DOWNLOAD_DIR=\"$HOME/Descargas\"\n"), "Pictures"),
            (Some("XDG_PICTURES_DIR=\"relative\"\n"), "Pictures"),
            (Some("XDG_PICTURES_DIR=$HOME/Imágenes\n"), "Pictures"),
            (Some("XDG_PICTURES_DIR=\"$HOME/Imágenes\n"), "Pictures"),
            (Some("XDG_PICTURES_DIR=\"\"\n"), "Pictures"),
            (Some("XDG_PICTURES_DIR=\"$HOME\"\n"), "Pictures"),
            (Some("XDG_PICTURES_DIR=\"$HOME/\"\n"), "Pictures"),
            (
                Some("XDG_PICTURES_DIR=\"/media/Imágenes\"\n"),
                "/media/Imágenes",
            ),
        ] {
            // A child process isolates environment changes from parallel tests.
            for override_config in [false, true] {
                let root = tempfile::tempdir()?;
                let home = root.path().join("home");
                let config = if override_config {
                    root.path().join("custom-config")
                } else {
                    home.join(".config")
                };
                fs::create_dir_all(&home)?;
                fs::create_dir_all(&config)?;
                if let Some(contents) = contents {
                    fs::write(config.join("user-dirs.dirs"), contents)?;
                }
                let expected = home.join(pictures).join("Pneu");
                let mut command = Command::new(env::current_exe()?);
                command
                    .args([
                        "--exact",
                        "platform::linux::tests::received_files_follow_xdg_and_preserve_saved_settings",
                    ])
                    .env("HOME", &home)
                    .env("XDG_CACHE_HOME", root.path().join("cache"))
                    .env_remove("XDG_CONFIG_HOME")
                    .env("PNEU_TEST_PICTURES_EXPECTED", expected)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                if override_config {
                    command.env("XDG_CONFIG_HOME", &config);
                }
                let child = command.spawn()?;
                eprintln!("pictures fixture PID {}", child.id());
                let output = child.wait_with_output()?;
                anyhow::ensure!(
                    output.status.success(),
                    "pictures fixture {contents:?}, XDG_CONFIG_HOME override {override_config}: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        Ok(())
    }
}
