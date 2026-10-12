//! Local smoke-test utility: snapshot text without logging it, or offer a text
//! selection (including a password-manager hint) for the watcher acceptance check.
use anyhow::{Context, Result};
use std::io::Read;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("clear") => {
            wl_clipboard_rs::copy::clear(
                wl_clipboard_rs::copy::ClipboardType::Regular,
                wl_clipboard_rs::copy::Seat::All,
            )?;
        }
        Some("snapshot") => {
            let (input, _) = wl_clipboard_rs::paste::get_contents(
                wl_clipboard_rs::paste::ClipboardType::Regular,
                wl_clipboard_rs::paste::Seat::Unspecified,
                wl_clipboard_rs::paste::MimeType::Text,
            )?;
            let mut bytes = Vec::new();
            input.take(1024 * 1024).read_to_end(&mut bytes)?;
            std::fs::write(args.get(1).context("snapshot path")?, bytes)?;
        }
        Some("offer") => {
            use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
            let mut sources = vec![MimeSource {
                source: Source::Bytes(args.get(1).context("text")?.as_bytes().into()),
                mime_type: MimeType::Specific("text/plain;charset=utf-8".into()),
            }];
            if args.get(2).is_some_and(|s| s == "secret") {
                sources.push(MimeSource {
                    source: Source::Bytes(b"secret".to_vec().into()),
                    mime_type: MimeType::Specific("x-kde-passwordManagerHint".into()),
                });
            }
            let mut options = Options::new();
            options.foreground(true);
            options.prepare_copy_multi(sources)?.serve()?;
        }
        _ => anyhow::bail!("clipboard_snapshot snapshot PATH | offer TEXT [secret]"),
    }
    Ok(())
}
