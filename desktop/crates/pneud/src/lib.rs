#[cfg(all(test, windows))]
extern crate self as pneud;
pub mod actions;
pub mod client;
mod clipboard_fingerprint;
pub mod config;
pub mod core;
mod discovery;
mod file_recipients;
pub mod identity;
pub mod ipc;
#[cfg(unix)]
pub mod macos_removal;
pub mod net;
mod platform;
mod storage;
mod store;
pub use store::outbox;
#[cfg(test)]
mod test_support;
pub mod tls;
mod transport;
pub mod ui;
#[cfg(windows)]
pub mod windows_package;
mod work;
pub use config::{Config, Paths};
pub use core::{ClipboardTarget, Core, Event};
pub fn random_hex(bytes: usize) -> String {
    use rand::TryRngCore;
    let mut value = vec![0; bytes];
    rand::rngs::OsRng
        .try_fill_bytes(&mut value)
        .expect("OS CSPRNG unavailable");
    hex::encode(value)
}
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}

#[cfg(target_os = "macos")]
pub use platform::macos_login;

#[cfg(target_os = "linux")]
pub mod linux_launch;
