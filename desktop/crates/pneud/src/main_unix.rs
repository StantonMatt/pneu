use anyhow::{Context, Result, bail};
use pneud::{
    Config, Core, Paths,
    actions::DesktopActions,
    ipc::{self, Command},
};
use std::sync::Arc;
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if args.first().is_some_and(|a| a.eq_ignore_ascii_case("-Embedding")) {
        // COM starts a background server even in console-subsystem dev builds.
        // Detach this process only; never hide a caller's shared console window.
        unsafe { windows_sys::Win32::System::Console::FreeConsole(); }
    }
    #[cfg(target_os = "macos")]
    if args.is_empty() {
        pneud::macos_login::enter_bundle()?;
    }
    #[cfg(target_os = "macos")]
    if args.first().is_some_and(|s| s == "pasteboard-read") {
        anyhow::ensure!(args.len() == 3, "pasteboard-read expects count and mode");
        anyhow::ensure!(
            matches!(args[2].as_str(), "marker" | "payload"),
            "invalid reader mode"
        );
        return pneud::ui::clipboard::read_helper(args[1].parse()?, args[2] == "marker");
    }
    #[cfg(windows)]
    if args.first().is_some_and(|s| s == "clipboard-read") {
        anyhow::ensure!(args.len() == 4, "clipboard-read expects sequence, mode and images");
        anyhow::ensure!(matches!(args[2].as_str(), "marker" | "payload"), "invalid reader mode");
        return pneud::ui::clipboard::read_helper(args[1].parse()?, args[2] == "marker", args[3].parse()?);
    }
    #[cfg(target_os = "macos")]
    if args.first().is_some_and(|s| s == "login-item") {
        return pneud::macos_login::cli(&args[1..]);
    }
    #[cfg(unix)]
    if args.first().is_some_and(|s| s == "uninstall-owned") {
        return pneud::macos_removal::cli(&args[1..]);
    }
    #[cfg(unix)]
    if args.first().is_some_and(|s| s == "clipboard-helper") {
        return pneud::actions::clipboard_helper(
            args.get(1).context("MIME argument")?.clone(),
            args.get(2).context("install ticket")?.parse()?,
        );
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
    #[cfg(windows)]
    let args = if args.first().is_some_and(|a| a.eq_ignore_ascii_case("-Embedding")) { vec!["-Embedding".into()] } else { args };
    let paths = Paths::from_env()?;
    match args.first().map(String::as_str) {
        Some("send") => {
            let reply = ipc::request(&paths.socket, ipc::desktop_send_command(&args[1..])).await?;
            println!("{}", serde_json::to_string_pretty(&reply)?);
            Ok(())
        }
        Some("operation-status") => {
            let operation = pneu_proto::Id::new(args.get(1).context("operation ID")?.clone())?;
            let reply = ipc::request(&paths.socket, Command::OperationStatus { operation }).await?;
            println!("{}", serde_json::to_string_pretty(&reply)?);
            Ok(())
        }
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
        Some("join") => {
            anyhow::ensure!(
                args.len() == 1,
                "join reads the invitation from stdin; no arguments allowed"
            );
            let mut uri = String::new();
            use std::io::Read;
            std::io::stdin().take(8194).read_to_string(&mut uri)?;
            anyhow::ensure!(uri.len() <= 8193, "pairing input too large");
            println!(
                "{}",
                ipc::request(&paths.socket, Command::Join { uri }).await?
            );
            Ok(())
        }
        Some("join-confirm" | "join-status" | "join-cancel") => {
            anyhow::ensure!(args.len() == 2, "expected attempt ID");
            let attempt = args[1].clone();
            let command = match args[0].as_str() {
                "join-confirm" => Command::JoinConfirm { attempt },
                "join-status" => Command::JoinStatus { attempt },
                _ => Command::JoinCancel { attempt },
            };
            println!("{}", ipc::request(&paths.socket, command).await?);
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
                "pneud [send [--to DEVICE] FILE...|run|window|pair|join (stdin)|join-confirm ATTEMPT|join-status ATTEMPT|join-cancel ATTEMPT|close-pairing|status|reload|unpair DEVICE|clipboard DEVICE TEXT|discard-clipboard DEVICE]"
            );
            #[cfg(target_os = "macos")]
            println!("pneud login-item register|unregister|status (from Pneu.app)");
            Ok(())
        }
        #[cfg(target_os = "macos")]
        None => {
            let result = pneud::macos_login::launch(&paths).await;
            if let Err(error) = &result {
                pneud::macos_login::show_startup_error(error);
            }
            result
        }
        #[cfg(target_os = "linux")]
        None => run(paths, false).await,
        #[cfg(windows)]
        None => {
            let show = GUI_LAUNCHER && !pneud::windows_package::startup_activation()?;
            if show && ipc::request(&paths.socket, Command::ShowWindow { choose_folder: false }).await.is_ok() {
                return Ok(());
            }
            // A launch can race the first instance binding its pipe. Retry only
            // a duplicate-instance failure, never start another recovery pass.
            match run(paths.clone(), show).await {
                Err(error) if show && duplicate_instance(&error) => {
                    for _ in 0..50 {
                        if ipc::request(&paths.socket, Command::ShowWindow { choose_folder: false }).await.is_ok() { return Ok(()); }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Err(error)
                }
                result => result,
            }
        },
        #[cfg(windows)]
        Some("-Embedding") => {
            // COM may race a normal launch; a duplicate cold server exits cleanly.
            match run(paths, false).await {
                Err(error) if error.chain().any(|e| e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)) => Ok(()),
                result => result,
            }
        }
        Some("run") => run(paths, false).await,
        _ => bail!("unknown subcommand; use pneud --help"),
    }
}
#[cfg(windows)]
fn duplicate_instance(error: &anyhow::Error) -> bool {
    error.chain().any(|e| e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock))
}
async fn run(paths: Paths, show_window: bool) -> Result<()> {
    #[cfg(not(windows))]
    let _ = show_window;
    // Claim IPC before loading/recovering the data store, so a second daemon
    // cannot recover staging files owned by the running instance.
    paths.prepare_runtime()?;
    #[cfg(unix)]
    let (ipc_listener, _instance_lock) = ipc::bind(&paths.socket)?;
    #[cfg(windows)]
    let (ipc_listener, _instance_lock) = ipc::bind(&paths.socket, &paths.data)?;
    #[cfg(windows)]
    let mut session = None;
    let result = async {
        paths.prepare()?;
        let config = Config::load(&paths.config)?;
        let actions = Arc::new(DesktopActions::new(paths.cache.clone()));
        #[cfg(windows)]
        if let Err(error) = actions.start_notifications().await { eprintln!("pneud: notifications unavailable: {error:#}"); }
        let port = config.port;
        let core = Core::new(paths, config, actions.clone()).await?;
        #[cfg(windows)]
        {
            match pneud::windows_package::Session::start(&core).await {
                Ok(started) => session = Some(started),
                Err(error) => {
                    core.finish_shutdown().await?;
                    return Err(error);
                }
            }
        }
        #[cfg(windows)]
        if show_window { core.request_window(false); }
        let (stop, rx) = tokio::sync::watch::channel(false);
        let mut ipc = tokio::spawn(Box::pin(ipc::serve(core.clone(), ipc_listener, rx)));
        let ui = tokio::spawn(Box::pin(pneud::ui::runtime::run(core.clone(),actions.clone(),stop.subscribe())));
        let signals_core=core.clone();
        let signals=tokio::spawn(Box::pin(async move {
            #[cfg(unix)]
            {
                let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
                tokio::select! { _=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{} }
            }
            #[cfg(windows)]
            tokio::signal::ctrl_c().await?;
            signals_core.request_shutdown();Ok::<_,anyhow::Error>(())
        }));
        let mut notified=false;
        let serving=async {
            let listener=loop {
                match pneud::net::listen(port).await {
                    Ok(listener)=>{core.clear_port_error();break listener;},
                    Err(error) if error.kind()==std::io::ErrorKind::AddrInUse=>{
                        core.set_receive_error(Some(pneud::ui::ReceiveError::Port(port)));
                        if !notified {
                            notified=true;
                            let _=actions.notice(pneud::ui::notifications::Notice::port(port),Vec::new(),None,0,None).await;
                            #[cfg(windows)]
                            eprintln!("pneud: receive port {port} is already in use");
                        }
                        tokio::select! { _=core.wait_shutdown()=>return Ok(()),_=tokio::time::sleep(std::time::Duration::from_secs(30))=>{} }
                    }
                    Err(error)=>return Err(error.into()),
                }
            };
            core.clone().serve(listener,true).await
        };
        tokio::pin!(serving);
        let mut ipc_finished=false;
        let serving=tokio::select! {
            biased;
            _=core.wait_shutdown()=>serving.await,
            result=&mut ipc=>{
                ipc_finished=true;
                core.request_shutdown();
                let _=serving.await;
                Err(match result {
                    Ok(Ok(()))=>anyhow::anyhow!("IPC server stopped unexpectedly"),
                    Ok(Err(error))=>error.context("IPC server failed"),
                    Err(error)=>anyhow::Error::new(error).context("IPC server task failed"),
                })
            },
            result=&mut serving=>result,
        };
        let _=stop.send(true);
        if !ipc_finished { let _=ipc.await; }
        let _=ui.await;
        signals.abort();let _=signals.await;
        // IPC/UI can submit Store jobs after serve's drain, or while the
        // receive port is occupied. Join them first, then drain every exit.
        core.finish_shutdown().await?;
        serving
    }
    .await;
    #[cfg(windows)]
    {
        // End-session is acknowledged only after Core/store destruction and
        // release of the instance lock; Restart Manager can now replace us.
        drop(_instance_lock);
        drop(session);
    }
    result
}
