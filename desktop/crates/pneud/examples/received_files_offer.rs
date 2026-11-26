//! Diagnostic used only inside a private compositor by clipboard_files_chrome.py.
#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use pneud::actions::{ClipboardInput, DesktopActions, PcActions, ReceivedFile};
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "clipboard-helper") {
        return pneud::actions::clipboard_helper(args[2].clone(), args[3].parse()?);
    }
    anyhow::ensure!(
        std::env::var_os("PNEU_PRIVATE_CLIPBOARD_PROBE").is_some(),
        "run using the isolated clipboard_files_chrome.py harness"
    );
    tokio::runtime::Runtime::new()?.block_on(async {
        let mode = args
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("expected files|image"))?;
        let files = args[2..]
            .iter()
            .map(|p| ReceivedFile::open(std::path::Path::new(p)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let input = match mode.as_str() {
            "files" => ClipboardInput::Files(files),
            "image" if files.len() == 1 => {
                ClipboardInput::Image(files[0].clone(), "image/jpeg".into())
            }
            _ => anyhow::bail!("expected files PATH... | image JPEG_PATH"),
        };
        let cache = std::path::PathBuf::from(std::env::var("PROBE_CACHE")?);
        std::fs::create_dir_all(&cache)?;
        let actions = DesktopActions::new(cache);
        let mut prepared = actions.prepare_clipboard(input, 1).await?;
        actions.install_clipboard(&mut prepared).await?;
        println!("OFFERED");
        // The owning process and its helper are stopped by the private harness.
        tokio::signal::ctrl_c().await?;
        drop(prepared);
        actions.drain_clipboard().await;
        Ok(())
    })
}
#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("Linux only")
}
