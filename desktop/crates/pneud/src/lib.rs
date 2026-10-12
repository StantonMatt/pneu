pub mod actions;
pub mod client;
pub mod config;
pub mod core;
mod discovery;
pub mod identity;
pub mod ipc;
mod storage;
mod store;
pub mod tls;
mod transport;
pub mod ui;
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
