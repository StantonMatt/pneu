//! Opt-in SMS device fixture. Requires a private D-Bus/Wayland environment and
//! an explicit scratch root. Never discovers devices or opens a desktop window.
#[cfg(not(target_os = "linux"))]
fn main() {}
#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::main()
}
#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Result, ensure};
    use pneud::{Config, Core, Paths, actions::DesktopActions};
    use std::{
        collections::HashMap,
        io::Write,
        path::PathBuf,
        sync::{Arc, Mutex},
    };
    struct Notifications {
        root: PathBuf,
        next: Mutex<u32>,
    }
    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Notifications {
        #[allow(clippy::too_many_arguments)]
        async fn notify(
            &self,
            _app: &str,
            replaces: u32,
            _icon: &str,
            summary: &str,
            body: &str,
            actions: Vec<String>,
            hints: HashMap<String, zbus::zvariant::OwnedValue>,
            _expires: i32,
        ) -> u32 {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            let id = if replaces == 0 { *next } else { replaces };
            // Only synthetic emulator fixtures belong in this isolated recorder.
            let notice = serde_json::json!({"id":id,"summary":summary,"body":body,"actions":actions,"category":hints.get("category").and_then(|v|<&str>::try_from(v).ok())});
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.root.join("notifications.jsonl"))
                .unwrap();
            writeln!(f, "{notice}").unwrap();
            id
        }
        fn get_capabilities(&self) -> Vec<&str> {
            vec!["body", "actions", "body-markup"]
        }
        fn get_server_information(&self) -> (&str, &str, &str, &str) {
            ("SMS isolated recorder", "Pneu", "1", "1.2")
        }
        fn close_notification(&self, _id: u32) {}
    }
    pub fn main() -> Result<()> {
        let args: Vec<_> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("clipboard-helper") {
            return pneud::actions::clipboard_helper(args[2].clone(), args[3].parse()?);
        }
        ensure!(
            std::env::var("PNEU_SMS_ISOLATED").as_deref() == Ok("1"),
            "requires an isolated test session"
        );
        ensure!(
            std::env::var("WAYLAND_DISPLAY").as_deref() == Ok("pneu-sms-test"),
            "private Wayland socket required"
        );
        if args.get(1).map(String::as_str) == Some("--read") {
            use std::io::Read;
            use wl_clipboard_rs::paste::{self, ClipboardType, MimeType, Seat};
            let types = paste::get_mime_types(ClipboardType::Regular, Seat::Unspecified)
                .unwrap_or_default();
            let mut values = HashMap::new();
            for mime in ["text/plain;charset=utf-8", "x-kde-passwordManagerHint"] {
                if types.contains(mime) {
                    let (mut file, _) = paste::get_contents(
                        ClipboardType::Regular,
                        Seat::Unspecified,
                        MimeType::Specific(mime),
                    )?;
                    let mut value = String::new();
                    file.read_to_string(&mut value)?;
                    values.insert(mime, value);
                }
            }
            println!("{}", serde_json::json!({"types":types,"values":values}));
            return Ok(());
        }
        if args.get(1).map(String::as_str) == Some("--offer") {
            use wl_clipboard_rs::copy::{MimeType, Options, Source};
            let mut options = Options::new();
            options.foreground(true);
            options.copy(
                Source::Bytes(args[2].as_bytes().to_vec().into()),
                MimeType::Specific("text/plain;charset=utf-8".into()),
            )?;
            return Ok(());
        }
        let root = PathBuf::from(
            args.get(1)
                .ok_or_else(|| anyhow::anyhow!("explicit scratch root required"))?,
        );
        ensure!(root.is_absolute(), "absolute scratch path required");
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(run(root))
    }
    async fn run(root: PathBuf) -> Result<()> {
        std::fs::create_dir_all(&root)?;
        let _notifications = zbus::connection::Builder::session()?
            .name("org.freedesktop.Notifications")?
            .serve_at(
                "/org/freedesktop/Notifications",
                Notifications {
                    root: root.clone(),
                    next: Mutex::new(0),
                },
            )?
            .build()
            .await?;
        let preferred: u16 = std::env::var("PNEU_SMS_TEST_PORT")
            .ok()
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(0);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", preferred)).await?;
        let port = listener.local_addr()?.port();
        let paths = Paths {
            data: root.join("data"),
            cache: root.join("cache"),
            config: root.join("config/config.toml"),
            socket: root.join("pneud.sock"),
        };
        let actions = Arc::new(DesktopActions::new(paths.cache.clone()));
        let core = Core::new(
            paths,
            Config {
                name: "SMS test computer".into(),
                port,
                save_path: root.join("saved"),
                notification_preview: false,
                ..Config::default()
            },
            actions,
        )
        .await?;
        let watcher = pneud::ui::clipboard::Watch::start(core.clone())?;
        let server = tokio::spawn(core.clone().serve(listener, false));
        let invitation = core
            .start_pairing_with_addresses(vec![format!("10.0.2.2:{port}")])
            .await?;
        std::fs::write(root.join("invitation.txt"), invitation.build()?)?;
        std::fs::write(root.join("pid"), std::process::id().to_string())?;
        println!("READY loopback port={port}");
        loop {
            std::fs::write(
                root.join("snapshot.json"),
                serde_json::to_vec_pretty(&core.ui_snapshot())?,
            )?;
            if root.join("STOP").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        core.request_shutdown();
        drop(watcher);
        core.shutdown_actions().await?;
        server.abort();
        let _ = server.await;
        Ok(())
    }
}
