//! Cross-host acceptance daemon: real Core, transport, store and IPC, with an
//! in-memory clipboard and test-only diagnostic socket. No desktop integration.
use anyhow::{Context, Result, ensure};
use pneud::{Config, Core, Paths, actions::PcActions};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[derive(Default, serde::Serialize)]
struct Clipboard {
    copies: usize,
    sha256: Option<String>,
}
#[derive(Default)]
struct Actions {
    clipboard: Mutex<Clipboard>,
}
#[async_trait::async_trait]
impl PcActions for Actions {
    async fn clipboard_text(&self, text: &str) -> Result<()> {
        let mut s = self.clipboard.lock().expect("test clipboard");
        s.copies += 1;
        s.sha256 = Some(hex::encode(Sha256::digest(text.as_bytes())));
        Ok(())
    }
    async fn clipboard_image(&self, _path: &Path, _mime: &str) -> Result<()> {
        anyhow::bail!("test clipboard accepts text only")
    }
    async fn notify(&self, _path: &Path, _mime: &str) -> Result<()> {
        Ok(())
    }
    async fn open(&self, _path: &Path) -> Result<()> {
        anyhow::bail!("desktop open disabled in test daemon")
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    ensure!(
        std::env::var("PNEU_HEADLESS_TEST").as_deref() == Ok("1"),
        "requires PNEU_HEADLESS_TEST=1"
    );
    for key in [
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        ensure!(
            Path::new(&std::env::var_os(key).with_context(|| format!("explicit {key} required"))?)
                .is_absolute(),
            "{key} must be absolute"
        );
    }
    let paths = Paths::from_env()?;
    let diagnostics = paths.socket.with_file_name("pneu-test.sock");
    if std::env::args().nth(1).as_deref() == Some("status") {
        let daemon = pneud::ipc::request(&paths.socket, pneud::ipc::Command::Status).await?;
        let mut socket = tokio::net::UnixStream::connect(diagnostics).await?;
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).await?;
        let clipboard: serde_json::Value = serde_json::from_slice(&bytes)?;
        println!(
            "{}",
            serde_json::json!({"daemon":daemon,"test_clipboard":clipboard})
        );
        return Ok(());
    }
    paths.prepare_runtime()?;
    pneud::config::private_dir(paths.socket.parent().context("socket parent")?)?;
    let (listener, _owner) = pneud::ipc::bind(&paths.socket)?;
    let (test_listener, _test_owner) = pneud::ipc::bind(&diagnostics)?;
    paths.prepare()?;
    let config = Config::load(&paths.config)?;
    ensure!(
        config.port != pneu_proto::DEFAULT_PORT,
        "test daemon must use a non-47821 port"
    );
    let tcp = tokio::net::TcpListener::bind(("::", config.port)).await?;
    let actions = Arc::new(Actions::default());
    let core = Core::new(paths, config, actions.clone()).await?;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let ipc = tokio::spawn(pneud::ipc::serve(core.clone(), listener, rx));
    let mut stopped = stop.subscribe();
    let diagnostic = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stopped.changed() => break,
                accepted = test_listener.accept() => {
                    let (mut socket, _) = accepted?;
                    ensure!(socket.peer_cred()?.uid() == rustix::process::getuid().as_raw(), "foreign test status owner");
                    let bytes = serde_json::to_vec(&*actions.clipboard.lock().expect("test clipboard"))?;
                    tokio::time::timeout(std::time::Duration::from_secs(2), socket.write_all(&bytes)).await??;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    let signals = core.clone();
    let signal = tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
        signals.request_shutdown();
        Ok::<_, anyhow::Error>(())
    });
    let serving = core.clone().serve(tcp, false).await;
    stop.send_replace(true);
    ipc.await??;
    diagnostic.await??;
    signal.abort();
    let _ = signal.await;
    core.shutdown_actions().await?;
    serving
}
