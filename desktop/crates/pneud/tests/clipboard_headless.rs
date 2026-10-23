#[cfg(not(target_os = "linux"))]
fn main() {}
#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::main()
}
#[cfg(target_os = "linux")]
#[path = "linux/clipboard_headless.rs"]
mod linux;
