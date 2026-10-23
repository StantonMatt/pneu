use super::*;
use std::{
    ffi::{CStr, OsStr},
    fs::File,
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
};

// PNEU_* overrides are intentionally explicit, absolute paths. They let tests
// isolate every persistent/runtime path without changing HOME or using /tmp's
// symlink on Darwin. XDG overrides remain supported for existing test tools.
fn override_path(key: &str, xdg: &str, fallback: PathBuf) -> PathBuf {
    env::var_os(key).map(PathBuf::from).unwrap_or_else(|| {
        env::var_os(xdg)
            .map(|p| PathBuf::from(p).join("pneu"))
            .unwrap_or(fallback)
    })
}
pub(crate) fn paths() -> Result<crate::Paths> {
    let home = PathBuf::from(env::var_os("HOME").context("HOME is unset")?);
    let support = home.join("Library/Application Support/Pneu");
    let data = override_path("PNEU_DATA_DIR", "XDG_DATA_HOME", support.clone());
    let config = override_path("PNEU_CONFIG_DIR", "XDG_CONFIG_HOME", support.clone());
    let runtime = env::var_os("PNEU_RUNTIME_DIR")
        .or_else(|| env::var_os("XDG_RUNTIME_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| support.join("run"));
    paths_at(data, config, cache_dir_from_env()?, runtime)
}
fn paths_at(
    data: PathBuf,
    config: PathBuf,
    cache: PathBuf,
    runtime: PathBuf,
) -> Result<crate::Paths> {
    let socket = runtime.join("pneud.sock");
    ensure!(
        [&data, &config, &cache, &runtime]
            .iter()
            .all(|p| p.is_absolute()),
        "Pneu paths must be absolute"
    );
    // sockaddr_un.sun_path is 104 bytes on Darwin, including the trailing NUL.
    ensure!(
        socket.as_os_str().as_bytes().len() < 104,
        "runtime socket path exceeds macOS's 103-byte limit; set PNEU_RUNTIME_DIR to a shorter private absolute directory"
    );
    Ok(crate::Paths {
        data,
        config: config.join("config.toml"),
        cache,
        socket,
    })
}
pub(crate) fn cache_dir_from_env() -> Result<PathBuf> {
    let home = PathBuf::from(env::var_os("HOME").context("HOME is unset")?);
    Ok(override_path(
        "PNEU_CACHE_DIR",
        "XDG_CACHE_HOME",
        home.join("Library/Caches/Pneu"),
    ))
}
pub(crate) fn received_files() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_else(|| ".".into())).join("Downloads/Pneu")
}
pub(crate) fn prepare_runtime(socket: &Path) -> Result<()> {
    crate::config::private_dir(socket.parent().context("runtime parent")?)
}
pub(crate) fn machine_hostname() -> String {
    // ComputerName is the user's friendly name; HostName may be absent or a DNS
    // label. scutil is system-provided and does not access a GUI/session bus.
    std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .unwrap_or_else(|| {
            rustix::system::uname()
                .nodename()
                .to_string_lossy()
                .into_owned()
        })
}
pub(crate) fn directory_path(file: &File) -> Result<PathBuf> {
    let mut bytes = [0u8; libc::PATH_MAX as usize];
    // F_GETPATH is display/recovery only. Mutations continue to use openat fds.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) };
    ensure!(result != -1, std::io::Error::last_os_error());
    let path = PathBuf::from(OsStr::from_bytes(
        unsafe { CStr::from_ptr(bytes.as_ptr().cast()) }.to_bytes(),
    ));
    let reopened = crate::storage::Folder::open(&path, false)?;
    use std::os::unix::fs::MetadataExt;
    let own = file.metadata()?;
    let named = reopened.file.metadata()?;
    ensure!(
        (own.dev(), own.ino()) == (named.dev(), named.ino()),
        "folder was deleted or replaced"
    );
    Ok(path)
}
pub(crate) fn metadata_file(
    directory: &File,
    name: impl rustix::path::Arg,
) -> Result<File, rustix::io::Errno> {
    // O_SYMLINK opens the final symlink itself; never its target. O_EVTONLY
    // avoids data access/side effects. Darwin cannot open socket vnodes this
    // way; unexpected sockets in a private staging tree fail cleanup closed.
    let flags = rustix::fs::OFlags::from_bits_retain(
        (libc::O_EVTONLY | libc::O_SYMLINK | libc::O_CLOEXEC) as _,
    );
    Ok(File::from(rustix::fs::openat(
        directory,
        name,
        flags,
        rustix::fs::Mode::empty(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_are_native_absolute_and_socket_length_is_checked() -> Result<()> {
        let root = PathBuf::from("/Users/test/Library/Application Support/Pneu");
        let p = paths_at(
            root.clone(),
            root.clone(),
            PathBuf::from("/Users/test/Library/Caches/Pneu"),
            root.join("run"),
        )?;
        assert_eq!(p.config, root.join("config.toml"));
        assert_eq!(p.socket, root.join("run/pneud.sock"));
        assert!(
            paths_at(
                root.clone(),
                root.clone(),
                root.clone(),
                PathBuf::from(format!("/{}", "x".repeat(104)))
            )
            .is_err()
        );
        assert!(paths_at(PathBuf::from("relative"), root.clone(), root.clone(), root).is_err());
        Ok(())
    }
    #[test]
    fn native_environment_defaults_and_overrides() -> Result<()> {
        let home = PathBuf::from("/Users/pneu-m1-test");
        if let Ok(case) = env::var("PNEU_TEST_NATIVE_PATHS") {
            let paths = paths()?;
            let (data, config, cache, runtime) = match case.as_str() {
                "native" => (
                    home.join("Library/Application Support/Pneu"),
                    home.join("Library/Application Support/Pneu"),
                    home.join("Library/Caches/Pneu"),
                    home.join("Library/Application Support/Pneu/run"),
                ),
                "xdg" => (
                    PathBuf::from("/test/data/pneu"),
                    PathBuf::from("/test/config/pneu"),
                    PathBuf::from("/test/cache/pneu"),
                    PathBuf::from("/test/run"),
                ),
                "explicit" => (
                    PathBuf::from("/private/data"),
                    PathBuf::from("/private/config"),
                    PathBuf::from("/private/cache"),
                    PathBuf::from("/private/run"),
                ),
                _ => unreachable!(),
            };
            assert_eq!(paths.data, data);
            assert_eq!(paths.config, config.join("config.toml"));
            assert_eq!(paths.cache, cache);
            assert_eq!(paths.socket, runtime.join("pneud.sock"));
            assert_eq!(received_files(), home.join("Downloads/Pneu"));
            return Ok(());
        }
        for case in ["native", "xdg", "explicit"] {
            let mut child = std::process::Command::new(env::current_exe()?);
            child
                .args([
                    "--exact",
                    "platform::macos::tests::native_environment_defaults_and_overrides",
                ])
                .env("HOME", &home)
                .env("PNEU_TEST_NATIVE_PATHS", case);
            for (native, xdg, name) in [
                ("PNEU_DATA_DIR", "XDG_DATA_HOME", "data"),
                ("PNEU_CONFIG_DIR", "XDG_CONFIG_HOME", "config"),
                ("PNEU_CACHE_DIR", "XDG_CACHE_HOME", "cache"),
                ("PNEU_RUNTIME_DIR", "XDG_RUNTIME_DIR", "run"),
            ] {
                child.env_remove(native).env_remove(xdg);
                if case != "native" {
                    child.env(xdg, format!("/test/{name}"));
                }
                if case == "explicit" {
                    child.env(native, format!("/private/{name}"));
                }
            }
            let output = child.output()?;
            ensure!(
                output.status.success(),
                "path fixture failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }
    #[test]
    fn directory_anchor_survives_rename_and_rejects_links() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let a = root.join("a");
        let b = root.join("b");
        let folder = crate::storage::Folder::open_private(&a)?;
        std::fs::rename(&a, &b)?;
        std::os::unix::fs::symlink(&b, &a)?;
        assert_eq!(folder.path()?, b);
        assert!(crate::storage::Folder::open(&a, false).is_err());
        std::fs::write(b.join("file"), b"private")?;
        assert!(crate::storage::open_regular_file(&a.join("file")).is_err());
        Ok(())
    }
}
