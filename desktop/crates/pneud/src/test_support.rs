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
    Ok(tempfile::tempdir_in(base)?)
}
