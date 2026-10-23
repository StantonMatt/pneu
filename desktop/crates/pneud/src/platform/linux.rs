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
    PathBuf::from(env::var_os("HOME").unwrap_or_else(|| ".".into())).join("Pictures/Pneu")
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
pub(crate) fn directory_path(file: &std::fs::File) -> Result<PathBuf> {
    use std::os::fd::AsRawFd;
    let path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    ensure!(
        !path.as_os_str().as_encoded_bytes().ends_with(b" (deleted)"),
        "folder was deleted"
    );
    Ok(path)
}
pub(crate) fn metadata_file(
    directory: &std::fs::File,
    name: impl rustix::path::Arg,
) -> Result<std::fs::File, rustix::io::Errno> {
    Ok(std::fs::File::from(rustix::fs::openat(
        directory,
        name,
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?))
}
