// Opt-in real-helper regression. Always starts its own isolated compositor.

use anyhow::{Result, ensure};
use pneu_proto::*;
use pneud::{
    Config, Core, Paths,
    actions::DesktopActions,
    client::{Phone, PhoneConnection},
};
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::io::AsyncWriteExt;
#[path = "../../src/test_support.rs"]
mod test_support;
pub(super) fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("clipboard-helper") => {
            pneud::actions::clipboard_helper(args[2].clone(), args[3].parse()?)
        }
        Some("--probe") => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(run()),
        Some("--marked") => {
            use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
            let sources = [
                ("text/plain;charset=utf-8", args[3].as_bytes()),
                (args[2].as_str(), b"1".as_slice()),
                (pneud::ui::clipboard::ORIGIN_MIME, b"1".as_slice()),
            ]
            .into_iter()
            .map(|(mime, bytes)| MimeSource {
                source: Source::Bytes(bytes.to_vec().into()),
                mime_type: MimeType::Specific(mime.into()),
            })
            .collect();
            let mut options = Options::new();
            options.foreground(true);
            options.prepare_copy_multi(sources)?.serve()?;
            Ok(())
        }
        Some("--secret") => {
            use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
            let sources = vec![
                MimeSource {
                    source: Source::Bytes(b"synthetic-secret-only-9173".to_vec().into()),
                    mime_type: MimeType::Specific("text/plain;charset=utf-8".into()),
                },
                MimeSource {
                    source: Source::Bytes(b"secret".to_vec().into()),
                    mime_type: MimeType::Specific("x-kde-passwordManagerHint".into()),
                },
            ];
            let mut options = Options::new();
            options.foreground(true);
            options.prepare_copy_multi(sources)?.serve()?;
            Ok(())
        }
        Some("--run-headless") => headless(),
        _ => {
            println!(
                "clipboard_headless: opt in with cargo test -p pneud --test clipboard_headless -- --run-headless"
            );
            Ok(())
        }
    }
}
// A private compositor and probe each get an owned process group. Even a failed
// assertion kills/reaps their helpers and private DBus/portal descendants.
struct Group(std::process::Child);
impl Drop for Group {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGTERM);
        }
        for _ in 0..100 {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}
fn headless() -> Result<()> {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    let dir = test_support::scratch()?;
    let root = dir.path();
    let runtime = root.join("runtime");
    std::fs::create_dir(&runtime)?;
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
        .save(root.join("fixture.png"))?;
    let isolate = |cmd: &mut std::process::Command| {
        for key in [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "SESSION_MANAGER",
            "DBUS_SESSION_BUS_ADDRESS",
            "DBUS_SESSION_BUS_PID",
            "XDG_SESSION_DESKTOP",
            "XDG_CURRENT_DESKTOP",
        ] {
            cmd.env_remove(key);
        }
        cmd.env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", root.join("kwin-config"))
            .env("XDG_CACHE_HOME", root.join("kwin-cache"))
            .env("XDG_DATA_HOME", root.join("kwin-data"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("KWIN_COMPOSE", "Q")
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("PROBE_ROOT", root)
            .process_group(0);
    };
    let start_compositor = || -> Result<Group> {
        let mut cmd = std::process::Command::new("dbus-run-session");
        cmd.args([
            "--",
            "kwin_wayland",
            "--virtual",
            "--socket",
            "pneu-actor-test",
            "--width",
            "800",
            "--height",
            "600",
            "--no-lockscreen",
            "--no-global-shortcuts",
            "--no-kactivities",
        ]);
        isolate(&mut cmd);
        let log = root.join("kwin.log");
        let out = std::fs::File::create(&log)?;
        cmd.stdout(out.try_clone()?).stderr(out);
        let mut compositor = Group(cmd.spawn()?);
        for _ in 0..100 {
            if runtime.join("pneu-actor-test").exists() {
                break;
            }
            ensure!(
                compositor.0.try_wait()?.is_none(),
                "headless KWin exited: {}",
                std::fs::read_to_string(&log)?
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        ensure!(
            runtime.join("pneu-actor-test").exists(),
            "headless Wayland socket unavailable"
        );
        Ok(compositor)
    };
    let mut compositor = Some(start_compositor()?);
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("--probe");
    isolate(&mut cmd);
    cmd.env("WAYLAND_DISPLAY", "pneu-actor-test");
    let mut probe = Group(cmd.spawn()?);
    let start = std::time::Instant::now();
    loop {
        if root.join("restart-compositor").exists() {
            drop(compositor.take());
            compositor = Some(start_compositor()?);
            std::fs::remove_file(root.join("restart-compositor"))?;
            std::fs::write(root.join("compositor-ready"), b"")?;
        }
        if let Some(status) = probe.0.try_wait()? {
            ensure!(
                status.success(),
                "headless clipboard probe failed: {status}"
            );
            break;
        }
        ensure!(
            start.elapsed() < Duration::from_secs(90),
            "headless clipboard probe timed out"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(probe);
    drop(compositor);
    println!("CLEAN private probe and compositor groups stopped/reaped");
    Ok(())
}
async fn delay(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}
async fn offered(mime: &str) -> Result<Vec<u8>> {
    let output = tokio::process::Command::new("wl-paste")
        .args(["--no-newline", "--type", mime])
        .output()
        .await?;
    ensure!(output.status.success(), "wl-paste failed");
    Ok(output.stdout)
}
async fn converged(expected: &[u8]) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        let current = offered("text/plain;charset=utf-8").await?;
        if current == expected {
            if start.elapsed() > Duration::from_millis(30) {
                println!(
                    "restoration settled after {} ms",
                    start.elapsed().as_millis()
                );
            }
            return Ok(());
        }
        ensure!(
            start.elapsed() < Duration::from_secs(3),
            "clipboard failed to converge after restoration: expected={:?} current={:?}",
            String::from_utf8_lossy(expected),
            String::from_utf8_lossy(&current)
        );
        delay(10).await;
    }
}
async fn copy(bytes: &[u8], mime: &str) -> Result<tokio::process::Child> {
    let mut p = tokio::process::Command::new("wl-copy")
        .args(["--foreground", "--type", mime])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = p.stdin.take().unwrap();
    input.write_all(bytes).await?;
    drop(input);
    delay(60).await;
    Ok(p)
}
fn key(db: &rusqlite::Connection) -> Result<String> {
    Ok(db.query_row(
        "SELECT hlc||'/'||source_device FROM clipboard_clock WHERE singleton=1",
        [],
        |r| r.get(0),
    )?)
}
async fn settled_without_forwarding(
    db: &rusqlite::Connection,
    before: &str,
    b: &mut PhoneConnection,
    local_copy: bool,
) -> Result<()> {
    delay(650).await;
    let after = key(db)?;
    if local_copy {
        let hlc = |key: &str| -> Result<u64> {
            Ok(key
                .split_once('/')
                .ok_or_else(|| anyhow::anyhow!("missing clock source"))?
                .0
                .parse()?)
        };
        ensure!(
            hlc(&after)? > hlc(before)?,
            "local re-copy did not fence old remote operations"
        );
    } else {
        ensure!(after == before, "proven marker echo changed register");
    }
    ensure!(
        b.read(Duration::from_millis(200)).await.is_err(),
        "unexpected forwarded content"
    );
    Ok(())
}
async fn local_seen(b: &mut PhoneConnection, expected: &str) -> Result<()> {
    let m = b.read(Duration::from_secs(3)).await?;
    ensure!(
        matches!(&m,Message::Clipboard(c) if c.text==expected),
        "local copy mismatch"
    );
    b.handle(m).await?;
    ensure!(
        b.read(Duration::from_millis(120)).await.is_err(),
        "duplicated local copy"
    );
    Ok(())
}
async fn manual_selection_seen(b: &mut MeshTls, mime: &str, expected: &[u8]) -> Result<()> {
    let first = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut *b)).await??;
    if mime != "image/png" {
        let Some(Message::Clipboard(text)) = first else {
            anyhow::bail!("manual text missing");
        };
        ensure!(text.text.as_bytes() == expected, "wrong startup text");
        write_frame(
            b,
            &Message::Ack(Ack {
                transfer_id: text.transfer_id,
                deduplicated: false,
            }),
        )
        .await?;
        return Ok(());
    }
    let Some(Message::FileBegin(begin)) = first else {
        anyhow::bail!("manual clipboard image missing FILE_BEGIN");
    };
    ensure!(begin.kind == FileKind::ClipboardImage, "wrong image kind");
    let mut bytes = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(3), read_frame(&mut *b)).await?? {
            Some(Message::FileChunk(chunk)) => {
                ensure!(
                    chunk.transfer_id == begin.transfer_id && chunk.offset == bytes.len() as u64,
                    "wrong image chunk"
                );
                bytes.extend(chunk.data);
            }
            Some(Message::FileEnd(end)) => {
                ensure!(end.transfer_id == begin.transfer_id, "wrong image end");
                break;
            }
            other => anyhow::bail!("unexpected image frame: {other:?}"),
        }
    }
    ensure!(bytes == expected, "manual image bytes changed");
    write_frame(
        b,
        &Message::Ack(Ack {
            transfer_id: begin.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    Ok(())
}
async fn wait_image(db: &rusqlite::Connection, before: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while key(db)? == before {
            delay(10).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}
type MeshTls = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;
async fn mesh_reply(tls: &mut MeshTls, id: &Id) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match read_frame(tls)
                .await?
                .ok_or_else(|| anyhow::anyhow!("mesh connection closed"))?
            {
                Message::Ack(ack) if &ack.transfer_id == id => return Ok(()),
                Message::Clipboard(clip) => {
                    write_frame(
                        tls,
                        &Message::Ack(Ack {
                            transfer_id: clip.transfer_id,
                            deduplicated: false,
                        }),
                    )
                    .await?
                }
                Message::Ping(ping) => write_frame(tls, &Message::Pong(ping)).await?,
                message => anyhow::bail!("unexpected mesh reply: {message:?}"),
            }
        }
    })
    .await?
}
async fn mesh_connect(core: &Core, phone: &Phone) -> Result<MeshTls> {
    let config = pneud::tls::client_config(&phone.identity, core.identity.fingerprint.clone())?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", core.config().port)).await?;
    let mut tls = connector
        .connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)
        .await?;
    write_frame(
        &mut tls,
        &Message::Hello(Hello {
            setup: None,
            version: 1,
            device_id: phone.identity.device_id.clone(),
            name: "Headless mesh sender".into(),
            capabilities: vec![
                "mesh".into(),
                "clipboard_text".into(),
                "clipboard_image".into(),
                "files".into(),
            ],
            role: Some(DeviceRole::Phone),
            addresses: None,
            paused: None,
        }),
    )
    .await?;
    ensure!(
        matches!(read_frame(&mut tls).await?, Some(Message::Hello(_))),
        "missing mesh HELLO"
    );
    let Some(Message::PeerSettings(snapshot)) = read_frame(&mut tls).await? else {
        anyhow::bail!("missing mesh settings");
    };
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: snapshot.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    let response = PeerSettings {
        transfer_id: Id::from_bytes(rand::random()),
        settings: snapshot.settings,
    };
    write_frame(&mut tls, &Message::PeerSettings(response.clone())).await?;
    mesh_reply(&mut tls, &response.transfer_id).await?;
    Ok(tls)
}
async fn mesh_send(tls: &mut MeshTls, text: Clipboard) -> Result<()> {
    let id = text.transfer_id.clone();
    write_frame(tls, &Message::Clipboard(text)).await?;
    mesh_reply(tls, &id).await
}
async fn run() -> Result<()> {
    let root = std::env::var("PROBE_ROOT")?;
    let root = Path::new(&root);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    println!("loopback port={} mDNS=false", listener.local_addr()?.port());
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
            port: listener.local_addr()?.port(),
            save_path: root.join("saved"),
            save_to_folder: false,
            notification_preview: false,
            copy_image_to_clipboard: true,
            ..Config::default()
        },
        actions,
    )
    .await?;
    let mut watch = pneud::ui::clipboard::Watch::start(core.clone())?;
    let serve = tokio::spawn(core.clone().serve(listener, false));
    delay(100).await;
    let a = Phone::new(root.join("sender"))?;
    let b = Phone::new(root.join("observer"))?;
    let uri = core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", core.config().port)])
        .await?;
    let mut ac = a.pair(&uri).await?;
    delay(1100).await;
    let uri = core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", core.config().port)])
        .await?;
    let mut bc = b.pair(&uri).await?;
    let db = rusqlite::Connection::open(root.join("data/state.sqlite3"))?;
    let png = root.join("fixture.png");
    let png_bytes = std::fs::read(&png)?;
    // The stock simulator advertises text only. Use its mesh connection for
    // this matrix so the recipient really negotiates clipboard images too.
    bc.close().await?;
    let mut startup_mesh = mesh_connect(&core, &b).await?;
    // Consume the initial selection, then prove a new PNG offer is sent with
    // the untouched default image policy to a peer that supports images.
    assert!(core.config().clipboard_include_images);
    let mut default_text = copy(b"default image policy", "text/plain;charset=utf-8").await?;
    delay(650).await;
    // The initial observation may be manual-only, depending on whether the
    // empty compositor advertised a selection before this first copy.
    match tokio::time::timeout(Duration::from_millis(120), read_frame(&mut startup_mesh)).await {
        Err(_) => {}
        Ok(Ok(Some(Message::Clipboard(text)))) => {
            ensure!(
                text.text == "default image policy",
                "unexpected warmup text"
            );
            write_frame(
                &mut startup_mesh,
                &Message::Ack(Ack {
                    transfer_id: text.transfer_id,
                    deduplicated: false,
                }),
            )
            .await?;
        }
        other => anyhow::bail!("unexpected default-policy warmup frame: {other:?}"),
    }
    if let Ok(message) = ac.read(Duration::from_millis(120)).await {
        ensure!(
            matches!(&message, Message::Clipboard(text) if text.text == "default image policy"),
            "unexpected default-policy warmup frame"
        );
        ac.handle(message).await?;
    }
    let mut default_image = copy(&png_bytes, "image/png").await?;
    manual_selection_seen(&mut startup_mesh, "image/png", &png_bytes).await?;
    let _ = default_text.kill().await;
    let _ = default_text.wait().await;
    let _ = default_image.kill().await;
    let _ = default_image.wait().await;
    println!(
        "PASS default Include images: image/png offer -> clipboard_image FILE_BEGIN/chunks/end -> paired peer ACK"
    );
    for mode in ["automatic", "manual", "off"] {
        for (mime, bytes) in [
            ("text/plain;charset=utf-8", b"startup selection".as_slice()),
            ("image/png", png_bytes.as_slice()),
        ] {
            watch.shutdown().await;
            core.set_setting("clipboard_mode".into(), mode.into())
                .await?;
            let mut startup_owner = copy(bytes, mime).await?;
            watch = pneud::ui::clipboard::Watch::start(core.clone())?;
            delay(650).await;
            ensure!(
                tokio::time::timeout(Duration::from_millis(120), read_frame(&mut startup_mesh))
                    .await
                    .is_err(),
                "startup selection auto-published in {mode}"
            );
            ensure!(
                ac.read(Duration::from_millis(120)).await.is_err(),
                "startup selection sent to source in {mode}"
            );
            let targets = || pneud::outbox::Targets::Devices(vec![b.identity.device_id.clone()]);
            if mode == "off" {
                ensure!(
                    core.share_clipboard(targets()).await.is_err(),
                    "Off allowed a share"
                );
            }
            core.set_setting("clipboard_mode".into(), "manual".into())
                .await?;
            core.share_clipboard(targets()).await?;
            manual_selection_seen(&mut startup_mesh, mime, bytes).await?;
            core.set_setting("clipboard_mode".into(), "automatic".into())
                .await?;
            ensure!(
                tokio::time::timeout(Duration::from_millis(120), read_frame(&mut startup_mesh))
                    .await
                    .is_err(),
                "mode change published old selection"
            );
            let _ = startup_owner.kill().await;
            let _ = startup_owner.wait().await;
        }
    }
    println!(
        "PASS real watcher first text/PNG: no automatic publication in any mode, explicit share after restart and Off-to-Manual works"
    );
    startup_mesh.shutdown().await?;
    drop(startup_mesh);
    bc = b.reconnect().await?;
    core.set_setting("clipboard_mode".into(), "manual".into())
        .await?;
    let mut before_restart = copy(b"before compositor restart", "text/plain;charset=utf-8").await?;
    delay(400).await;
    core.share_clipboard(pneud::outbox::Targets::Devices(vec![
        b.identity.device_id.clone(),
    ]))
    .await?;
    local_seen(&mut bc, "before compositor restart").await?;
    std::fs::write(root.join("restart-compositor"), b"")?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.join("compositor-ready").exists() {
            delay(25).await;
        }
    })
    .await?;
    tokio::time::timeout(Duration::from_secs(3), watch.shutdown()).await?;
    ensure!(
        core.share_clipboard(pneud::outbox::Targets::Devices(vec![
            b.identity.device_id.clone()
        ]))
        .await
        .is_err(),
        "disconnected watcher retained a shareable selection"
    );
    let _ = before_restart.kill().await;
    let _ = before_restart.wait().await;
    let mut after_restart = copy(b"after compositor restart", "text/plain;charset=utf-8").await?;
    core.set_setting("clipboard_mode".into(), "automatic".into())
        .await?;
    watch = pneud::ui::clipboard::Watch::start(core.clone())?;
    delay(650).await;
    ensure!(
        bc.read(Duration::from_millis(120)).await.is_err(),
        "recovered compositor selection auto-published"
    );
    core.set_setting("clipboard_mode".into(), "manual".into())
        .await?;
    core.share_clipboard(pneud::outbox::Targets::Devices(vec![
        b.identity.device_id.clone(),
    ]))
    .await?;
    local_seen(&mut bc, "after compositor restart").await?;
    core.set_setting("clipboard_mode".into(), "automatic".into())
        .await?;
    let _ = after_restart.kill().await;
    let _ = after_restart.wait().await;
    println!(
        "PASS private compositor restart: stale share invalidated, reconnected first selection is manual-only"
    );
    let text = a.text("remote-short-1234".into(), ClipboardOrigin::Clipboard)?;
    let ack = ac.send_text(text.clone()).await?;
    ensure!(!ack.deduplicated, "first remote text deduplicated");
    ensure!(
        offered("text/plain;charset=utf-8").await? == text.text.as_bytes(),
        "text mismatch"
    );
    let types = tokio::process::Command::new("wl-paste")
        .arg("--list-types")
        .output()
        .await?;
    ensure!(types.status.success(), "list clipboard types failed");
    let types = String::from_utf8(types.stdout)?;
    let retired_marker = types
        .lines()
        .find(|m| m.starts_with("application/x-pneu-install-"))
        .ok_or_else(|| anyhow::anyhow!("missing install marker"))?
        .to_owned();
    let before = key(&db)?;
    let mut owner = copy(text.text.as_bytes(), "text/plain;charset=utf-8").await?;
    settled_without_forwarding(&db, &before, &mut bc, true).await?;
    ensure!(
        offered("text/plain;charset=utf-8").await? == text.text.as_bytes(),
        "text re-copy was overwritten"
    );
    ensure!(
        owner.try_wait()?.is_none(),
        "Pneu replaced the local text owner"
    );
    println!("PASS text re-copy: local fence, original owner retained, no forwarding");
    let replay = ac.send_text(text.clone()).await?;
    ensure!(replay.deduplicated, "text replay not deduplicated");
    let newer = a.text("newer-remote-winner".into(), ClipboardOrigin::Clipboard)?;
    ensure!(
        !ac.send_text(newer.clone()).await?.deduplicated,
        "new winner deduplicated"
    );
    let before = key(&db)?;
    let mut marked = tokio::process::Command::new(std::env::current_exe()?)
        .args(["--marked", &retired_marker, &text.text])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    delay(100).await;
    converged(newer.text.as_bytes()).await?;
    settled_without_forwarding(&db, &before, &mut bc, false).await?;
    ensure!(
        ac.send_text(newer).await?.deduplicated,
        "restored winner replay was not deduplicated"
    );
    let _ = marked.kill().await;
    let _ = marked.wait().await;
    println!(
        "PASS retired install-marker + origin reoffer restores winner; no origination/forward"
    );
    // Re-copy older received bytes while a different remote clip is current.
    // This is a human selection, unlike the proven marker reoffer above.
    let before = key(&db)?;
    let _ = owner.kill().await;
    let _ = owner.wait().await;
    owner = copy(text.text.as_bytes(), "text/plain;charset=utf-8").await?;
    settled_without_forwarding(&db, &before, &mut bc, true).await?;
    ensure!(
        offered("text/plain;charset=utf-8").await? == text.text.as_bytes(),
        "older received re-copy was repaired away"
    );
    ensure!(
        owner.try_wait()?.is_none(),
        "old remote repair replaced the human selection"
    );
    println!("PASS older received text re-copy stays put over a different remote winner");
    let before = key(&db)?;
    let meta = a
        .file_metadata(&png, FileKind::ClipboardImage, "image/png".into())
        .await?;
    ac.send_file(meta.clone(), &png).await?;
    wait_image(&db, &before).await?;
    ensure!(offered("image/png").await? == png_bytes, "PNG mismatch");
    let before = key(&db)?;
    let _ = owner.kill().await;
    let _ = owner.wait().await;
    owner = copy(&png_bytes, "image/png").await?;
    settled_without_forwarding(&db, &before, &mut bc, true).await?;
    ensure!(
        offered("image/png").await? == png_bytes,
        "PNG re-copy was overwritten"
    );
    ensure!(
        owner.try_wait()?.is_none(),
        "Pneu replaced the local PNG owner"
    );
    println!("PASS PNG re-copy: local fence, original owner retained, no forwarding");
    ac.send_file(meta, &png).await?;
    // Fresh local observations must publish once after remote installs and echoes.
    let _ = owner.kill().await;
    let _ = owner.wait().await;
    owner = copy(b"local-after-echo", "text/plain;charset=utf-8").await?;
    local_seen(&mut bc, "local-after-echo").await?;
    println!("PASS local copy after echoes published exactly once");
    for i in 0..12 {
        let remote = a.text(
            format!("alternating-remote-{i}"),
            ClipboardOrigin::Clipboard,
        )?;
        ac.send_text(remote).await?;
        let local = format!("alternating-local-{i}");
        let _ = owner.kill().await;
        let _ = owner.wait().await;
        owner = copy(local.as_bytes(), "text/plain;charset=utf-8").await?;
        local_seen(&mut bc, &local).await?;
        converged(local.as_bytes()).await?;
    }
    println!(
        "PASS 12 alternating remote/local installs: final clipboard and one outgoing effect per local copy"
    );

    // A legacy request is stamped at its eventual effect boundary: if its
    // socket admission follows the local copy, it legitimately wins. Mesh
    // stamps let this race assert the newer human action in either arrival order.
    ac.close().await?;
    let mut mesh = mesh_connect(&core, &a).await?;
    for i in 0..20 {
        let mut remote = a.text(format!("race-remote-{i}"), ClipboardOrigin::Clipboard)?;
        remote.hlc = Some(pneud::now_ms());
        remote.source_device = Some(a.identity.device_id.clone());
        let local = format!("race-local-{i}");
        let send = mesh_send(&mut mesh, remote);
        let human = async {
            delay(1).await;
            copy(local.as_bytes(), "text/plain;charset=utf-8").await
        };
        let (sent, new_owner) = tokio::join!(send, human);
        sent?;
        let mut new_owner = new_owner?;
        let _ = owner.kill().await;
        let _ = owner.wait().await;
        std::mem::swap(&mut owner, &mut new_owner);
        local_seen(&mut bc, &local).await?;
        converged(local.as_bytes()).await?;
    }
    println!(
        "PASS 20 remote-install/local-copy races separated by 1 ms, one outgoing local effect each"
    );
    // Synthetic KDE secret: watcher must not publish or hash its bytes.
    let mut secret = tokio::process::Command::new(std::env::current_exe()?)
        .arg("--secret")
        .kill_on_drop(true)
        .spawn()?;
    delay(700).await;
    ensure!(
        bc.read(Duration::from_millis(200)).await.is_err(),
        "secret broadcast"
    );
    let rows: Vec<String> = {
        let mut q = db.prepare(
            "SELECT meta FROM clipboard_fingerprints UNION ALL SELECT meta FROM clipboard_recent",
        )?;
        q.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    ensure!(
        rows.iter()
            .all(|r| !r.contains("synthetic-secret-only-9173")),
        "plaintext persisted"
    );
    println!("PASS KDE secret suppressed; received history/receipts contain no plaintext");
    let _ = secret.kill().await;
    let _ = secret.wait().await;
    let _ = owner.kill().await;
    let _ = owner.wait().await;
    mesh.shutdown().await?;
    drop(mesh);
    bc.close().await?;
    watch.shutdown().await;
    core.request_shutdown();
    serve.await??;
    core.shutdown_actions().await?;
    println!("CLEAN shutdown complete");
    Ok(())
}
