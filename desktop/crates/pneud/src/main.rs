use anyhow::{Context, Result, bail};
use pneud::{
    Config, Core, Paths,
    actions::DesktopActions,
    ipc::{self, Command},
};
use std::sync::Arc;
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "clipboard-helper") {
        return pneud::actions::clipboard_helper(args.get(1).context("MIME argument")?.clone());
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(async_main(args))
}
async fn async_main(args: Vec<String>) -> Result<()> {
    let paths = Paths::from_env()?;
    match args.first().map(String::as_str) {
        Some("pair") => {
            let reply = ipc::request(&paths.socket, Command::Pair).await?;
            let uri = reply["uri"].as_str().context("pairing reply")?;
            println!("{uri}\nExpires: {} UTC milliseconds", reply["expires_at"]);
            let code = qrcode::QrCode::new(uri.as_bytes())?;
            println!(
                "{}",
                code.render::<qrcode::render::unicode::Dense1x2>()
                    .quiet_zone(true)
                    .build()
            );
            Ok(())
        }
        Some("status") => {
            println!(
                "{}",
                serde_json::to_string_pretty(&ipc::request(&paths.socket, Command::Status).await?)?
            );
            Ok(())
        }
        Some("close-pairing") => {
            ipc::request(&paths.socket, Command::ClosePairing).await?;
            Ok(())
        }
        Some("unpair") => {
            ipc::request(
                &paths.socket,
                Command::Unpair {
                    device: pneu_proto::Id::new(args.get(1).context("device ID")?.clone())?,
                },
            )
            .await?;
            Ok(())
        }
        Some("reload") => {
            let config = Config::load(&paths.config)?;
            ipc::request(&paths.socket, Command::Config { config }).await?;
            Ok(())
        }
        Some("clipboard") => {
            ipc::request(
                &paths.socket,
                Command::Clipboard {
                    device: pneu_proto::Id::new(args.get(1).context("device ID")?.clone())?,
                    text: args.get(2).context("clipboard text")?.clone(),
                },
            )
            .await?;
            Ok(())
        }
        Some("discard-clipboard") => {
            ipc::request(
                &paths.socket,
                Command::DiscardClipboard {
                    device: pneu_proto::Id::new(args.get(1).context("device ID")?.clone())?,
                },
            )
            .await?;
            Ok(())
        }
        Some("--help" | "help") => {
            println!(
                "pneud [run|pair|close-pairing|status|reload|unpair DEVICE|clipboard DEVICE TEXT|discard-clipboard DEVICE]"
            );
            Ok(())
        }
        None | Some("run") => run(paths).await,
        _ => bail!("unknown subcommand; use pneud --help"),
    }
}
async fn run(paths: Paths) -> Result<()> {
    // Claim IPC before loading/recovering the data store, so a second daemon
    // cannot recover staging files owned by the running instance.
    let (ipc_listener, _instance_lock) = ipc::bind(&paths.socket)?;
    let socket = paths.socket.clone();
    let result = async {
        paths.prepare()?;
        let config = Config::load(&paths.config)?;
        let listener = tokio::net::TcpListener::bind(("::", config.port)).await?;
        let actions = Arc::new(DesktopActions::new(paths.cache.clone()));
        let core = Core::new(paths, config, actions).await?;
        let (stop, rx) = tokio::sync::watch::channel(false);
        let ipc = tokio::spawn(ipc::serve(core.clone(), ipc_listener, rx));
        let mut events = core.subscribe();
        let event_log = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(pneud::Event::Error { message, .. }) => eprintln!("pneud: {message}"),
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
        let server = core.clone().serve(listener, true);
        tokio::pin!(server);
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let serving = tokio::select! {
            result = &mut server => result,
            _ = tokio::signal::ctrl_c() => { core.request_shutdown(); server.await },
            _ = terminate.recv() => { core.request_shutdown(); server.await },
        };
        let _ = stop.send(true);
        let _ = ipc.await;
        event_log.abort();
        let _ = event_log.await;
        core.shutdown_actions().await?;
        serving
    }
    .await;
    let _ = std::fs::remove_file(socket);
    result
}
