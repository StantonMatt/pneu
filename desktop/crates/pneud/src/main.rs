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
    if args.first().is_some_and(|s| s == "ui-window") {
        return pneud::ui::window::run(Paths::from_env()?);
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()?
        .block_on(async_main(args))
}
async fn async_main(args: Vec<String>) -> Result<()> {
    let paths = Paths::from_env()?;
    match args.first().map(String::as_str) {
        Some("window") => {
            ipc::request(
                &paths.socket,
                Command::ShowWindow {
                    choose_folder: false,
                },
            )
            .await?;
            Ok(())
        }
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
                "pneud [run|window|pair|close-pairing|status|reload|unpair DEVICE|clipboard DEVICE TEXT|discard-clipboard DEVICE]"
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
    async {
        paths.prepare()?;
        let config = Config::load(&paths.config)?;
        let actions = Arc::new(DesktopActions::new(paths.cache.clone()));
        let port = config.port;
        let core = Core::new(paths, config, actions.clone()).await?;
        let (stop, rx) = tokio::sync::watch::channel(false);
        let ipc = tokio::spawn(Box::pin(ipc::serve(core.clone(), ipc_listener, rx)));
        let ui = tokio::spawn(Box::pin(pneud::ui::runtime::run(core.clone(),actions.clone(),stop.subscribe())));
        let signals_core=core.clone();
        let signals=tokio::spawn(Box::pin(async move {
            let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! { _=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{} }
            signals_core.request_shutdown();Ok::<_,anyhow::Error>(())
        }));
        let mut notified=false;
        let serving=async {
            let listener=loop {
                match tokio::net::TcpListener::bind(("::",port)).await {
                    Ok(listener)=>{core.clear_port_error();break listener;},
                    Err(error) if error.kind()==std::io::ErrorKind::AddrInUse=>{
                        core.set_receive_error(Some(pneud::ui::ReceiveError::Port(port)));
                        if !notified {notified=true;let _=actions.notice(pneud::ui::notifications::Notice::port(port),Vec::new(),None,0,None).await;}
                        tokio::select! { _=core.wait_shutdown()=>return Ok(()),_=tokio::time::sleep(std::time::Duration::from_secs(30))=>{} }
                    }
                    Err(error)=>return Err(error.into()),
                }
            };
            core.clone().serve(listener,true).await
        }.await;
        let _=stop.send(true);
        let _=ipc.await;
        let _=ui.await;
        signals.abort();let _=signals.await;
        core.shutdown_actions().await?;
        serving
    }
    .await
}
