#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    anyhow::bail!(
        "pneud is not yet supported on Windows: IPC, discovery and desktop services await porting"
    )
}
#[cfg(unix)]
include!("main_unix.rs");
