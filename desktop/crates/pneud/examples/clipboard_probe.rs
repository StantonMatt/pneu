//! Manual Wayland check: cargo run -p pneud --example clipboard_probe -- FILE MIME
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::io::Read;
use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};
fn digest(mut input: impl Read) -> Result<(u64, String)> {
    let mut bytes = [0; 32768];
    let mut hash = Sha256::new();
    let mut count = 0;
    loop {
        let n = input.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
        count += n as u64;
    }
    Ok((count, hex::encode(hash.finalize())))
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().context("expected file")?;
    let requested = args.get(1).map(String::as_str).unwrap_or("image/png");
    let (clipboard, mime) = get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        MimeType::Specific(requested),
    )?;
    let actual = digest(clipboard)?;
    let expected = digest(std::fs::File::open(path)?)?;
    ensure!(actual == expected, "clipboard differs from expected file");
    println!(
        "clipboard verified: MIME={mime} bytes={} sha256={}",
        actual.0, actual.1
    );
    Ok(())
}
