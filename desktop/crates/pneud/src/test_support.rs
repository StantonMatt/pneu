//! Disposable fixture roots derived from the test process environment.
use std::path::PathBuf;

pub(crate) fn scratch() -> anyhow::Result<tempfile::TempDir> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .unwrap_or_else(std::env::temp_dir);
    let base = cache.join("agent-scratch/pneu/tests");
    std::fs::create_dir_all(&base)?;
    let dir = tempfile::tempdir_in(base)?;
    #[cfg(windows)]
    {
        // Replace only this task's empty disposable directory so the backend
        // supplies its DACL at creation, rather than repairing an exposed ACL.
        std::fs::remove_dir(dir.path())?;
        pneud::config::private_dir(dir.path())?;
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}
