use anyhow::{Context, Result, bail};
use pneu_proto::{ClipboardOrigin, FileKind, PairingUri};
use pneud::client::Phone;
use std::path::PathBuf;
#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let state = if args.first().is_some_and(|s| s == "--state") {
        let directory = PathBuf::from(args.get(1).context("state directory")?);
        args.drain(..2);
        directory
    } else {
        pneud::Paths::from_env()?.data.join("simulator")
    };
    let phone = Phone::new(state)?;
    match args.first().map(String::as_str) {
        Some("pair") => {
            let uri = PairingUri::parse(args.get(1).context("pairing URI")?)?;
            let _ = phone.pair(&uri).await?;
            println!("Paired with {} ({})", uri.name, uri.fingerprint);
        }
        Some("send-file") => {
            let path = PathBuf::from(args.get(1).context("file path")?);
            let kind = match args.get(2).map(String::as_str).unwrap_or("file") {
                "screenshot" => FileKind::Screenshot,
                "photo" => FileKind::Photo,
                "recording" => FileKind::Recording,
                "file" => FileKind::File,
                "clipboard_image" => FileKind::ClipboardImage,
                _ => bail!("invalid kind"),
            };
            let mime = args.get(3).cloned().unwrap_or_else(|| {
                match path.extension().and_then(|s| s.to_str()) {
                    Some("png") => "image/png",
                    Some("jpg" | "jpeg") => "image/jpeg",
                    Some("webp") => "image/webp",
                    _ => "application/octet-stream",
                }
                .into()
            });
            let metadata = phone.file_metadata(&path, kind, mime).await?;
            let mut connection = phone.reconnect().await?;
            let ack = connection.send_file(metadata, &path).await?;
            println!("ACK {} deduplicated={}", ack.transfer_id, ack.deduplicated);
            connection.close().await?;
        }
        Some("clipboard") => {
            let origin = match args.get(2).map(String::as_str).unwrap_or("clipboard") {
                "clipboard" => ClipboardOrigin::Clipboard,
                "share" | "origin=share" => ClipboardOrigin::Share,
                _ => bail!("invalid clipboard origin"),
            };
            let text = phone.text(args.get(1).context("text")?.clone(), origin)?;
            let mut connection = phone.reconnect().await?;
            let ack = connection.send_text(text).await?;
            println!("ACK {} deduplicated={}", ack.transfer_id, ack.deduplicated);
            connection.close().await?;
        }
        Some("status") => {
            let paused = match args.get(1).map(String::as_str) {
                Some("true" | "paused" | "pause") => true,
                Some("false" | "resumed" | "resume") => false,
                _ => bail!("status needs true/false (or pause/resume)"),
            };
            let mut connection = phone.reconnect().await?;
            connection.send_status(paused).await?;
            connection
                .listen(args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(120))
                .await?;
        }
        Some("unpair") => phone.unpair().await?,
        Some("listen") => {
            phone
                .reconnect()
                .await?
                .listen(args.get(1).map(|s| s.parse()).transpose()?.unwrap_or(120))
                .await?;
        }
        _ => println!(
            "pneu-sim [--state DIR] pair URI | send-file PATH [screenshot|photo|recording|file|clipboard_image] [MIME] | clipboard TEXT [clipboard|origin=share] | status true|false [SECONDS] | unpair | listen [SECONDS]"
        ),
    }
    Ok(())
}
