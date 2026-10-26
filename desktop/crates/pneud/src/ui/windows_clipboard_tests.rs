//! Opt-in tests: these replace only the disposable VM console's clipboard.
use super::*;
use crate::{
    Config, Core, Paths,
    actions::{ClipboardInput, DesktopActions, PcActions, PreparedClipboard, ReceivedFile},
    client::Phone,
};
use pneu_proto::{ClipboardOrigin, FileKind, Message};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

// Core calls record_clipboard_copy only after the actor's applied outcome,
// which in this backend requires the watcher's install-ticket handshake.
struct RecordingActions {
    native: DesktopActions,
    copied: AtomicBool,
}
#[async_trait::async_trait]
impl PcActions for RecordingActions {
    async fn clipboard_text(&self, text: &str) -> Result<()> {
        self.native.clipboard_text(text).await
    }
    async fn clipboard_image(&self, path: &Path, mime: &str) -> Result<()> {
        self.native.clipboard_image(path, mime).await
    }
    async fn notify(&self, path: &Path, mime: &str) -> Result<()> {
        self.native.notify(path, mime).await
    }
    async fn open(&self, path: &Path) -> Result<()> {
        self.native.open(path).await
    }
    async fn prepare_clipboard(
        &self,
        input: ClipboardInput,
        ticket: u128,
    ) -> Result<PreparedClipboard> {
        self.native.prepare_clipboard(input, ticket).await
    }
    async fn install_clipboard(&self, prepared: &mut PreparedClipboard) -> Result<()> {
        self.native.install_clipboard(prepared).await
    }
    async fn record_clipboard_copy(&self, file: &ReceivedFile) {
        self.native.record_clipboard_copy(file).await;
        self.copied.store(true, Ordering::SeqCst);
    }
    async fn drain_clipboard(&self) {
        self.native.drain_clipboard().await;
    }
    async fn shutdown(&self) -> Result<()> {
        self.native.shutdown().await
    }
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn fixture(mode: &str, directory: &Path) -> Result<Process> {
    std::fs::create_dir_all(directory)?;
    let child = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "ui::clipboard::tests::clipboard_provider_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("PNEU_M3_FIXTURE", mode)
        .env("PNEU_M3_FIXTURE_DIR", directory)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    eprintln!("fixture mode={mode} PID={}", child.id());
    let mut process = Process(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !directory.join("ready").exists() {
        ensure!(process.0.try_wait()?.is_none(), "fixture exited");
        ensure!(Instant::now() < deadline, "fixture startup timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(process)
}
unsafe extern "system" fn delayed(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_RENDERFORMAT {
        let directory = PathBuf::from(std::env::var_os("PNEU_M3_FIXTURE_DIR").unwrap());
        let mode = std::env::var("PNEU_M3_FIXTURE").unwrap();
        let _ = std::fs::write(directory.join("rendered"), b"render requested");
        let _ = std::fs::write(directory.join(format!("rendered-{w}")), b"requested");
        if mode.starts_with("privacy-on-render-") {
            let name = match mode.as_str() {
                "privacy-on-render-ignore" => IGNORE,
                "privacy-on-render-history" => HISTORY,
                _ => EXCLUDE,
            };
            let result = put(format(name).unwrap(), &0u32.to_le_bytes());
            let _ = std::fs::write(
                directory.join("privacy-added"),
                format!("{}", result.is_ok()),
            );
            let bytes: Vec<_> = wide("must not escape rendering")
                .iter()
                .flat_map(|c| c.to_le_bytes())
                .collect();
            let _ = put(w as u32, &bytes);
        } else if mode == "marker-then-block" && w as u32 == format(INSTALL).unwrap() {
            std::thread::sleep(Duration::from_millis(200));
            let _ = put(w as u32, format!("{:032x}", 999).as_bytes());
        } else {
            std::thread::sleep(Duration::from_secs(60));
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}
#[test]
#[ignore = "internal opt-in clipboard source subprocess"]
fn clipboard_provider_fixture() -> Result<()> {
    let mode = std::env::var("PNEU_M3_FIXTURE").context("fixture subprocess only")?;
    let directory = PathBuf::from(std::env::var_os("PNEU_M3_FIXTURE_DIR").unwrap());
    let window = Window::new()?;
    let clipboard = Clipboard::open(window.0)?;
    if mode == "hold" || mode == "hold-until-release" {
        std::fs::write(directory.join("ready"), b"ready")?;
        if mode == "hold" {
            std::thread::sleep(Duration::from_millis(125));
        } else {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !directory.join("release").exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        return Ok(());
    }
    unsafe {
        SetWindowLongPtrW(window.0, GWLP_WNDPROC, delayed as *const () as isize);
    }
    ensure!(unsafe { EmptyClipboard() } != 0, "empty fixture clipboard");
    unsafe {
        SetClipboardData(TEXT, null_mut());
    }
    match mode.as_str() {
        "sensitive" => put(format(EXCLUDE)?, &1u32.to_le_bytes())?,
        "sensitive-ignore" => put(format(IGNORE)?, &1u32.to_le_bytes())?,
        "sensitive-history" => put(format(HISTORY)?, &0u32.to_le_bytes())?,
        "delayed-history" => unsafe {
            SetClipboardData(format(HISTORY)?, null_mut());
        },
        "marker-then-block" => unsafe {
            SetClipboardData(format(INSTALL)?, null_mut());
        },
        mode if mode.starts_with("privacy-on-render-") => unsafe {
            SetClipboardData(format("PNG")?, null_mut());
        },
        _ => {}
    }
    drop(clipboard);
    std::fs::write(directory.join("ready"), b"ready")?;
    let mut message = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } > 0 {
        unsafe {
            DispatchMessageW(&message);
        }
    }
    Ok(())
}
fn clipboard_text() -> Result<String> {
    let _clipboard = Clipboard::open(null_mut())?;
    let bytes = get(TEXT, 2 * (pneu_proto::MAX_CLIPBOARD_BYTES + 1))?;
    let chars: Vec<_> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .take_while(|c| *c != 0)
        .collect();
    Ok(String::from_utf16(&chars)?)
}
fn image() -> image::DynamicImage {
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(2, 2, |x, y| {
        image::Rgba([x as u8 * 80, y as u8 * 90, 140, 255])
    }))
}
#[test]
#[ignore = "requires disposable interactive Windows console clipboard"]
fn native_clipboard_writer_policy_and_contention() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let offer = Offer::prepare(&ClipboardInput::Text("Pneu M3 🦀".into()), 42)?;
    let holder = fixture("hold", &dir.path().join("holder"))?;
    let started = Instant::now();
    offer.install()?;
    ensure!(
        started.elapsed() >= Duration::from_millis(40),
        "contention was not exercised"
    );
    drop(holder);
    assert_eq!(clipboard_text()?, "Pneu M3 🦀");
    {
        let _clipboard = Clipboard::open(null_mut())?;
        assert_eq!(ticket(&get(format(INSTALL)?, 32)?), Some(42));
        assert_eq!(get(format(CLOUD)?, 4)?, 0u32.to_le_bytes());
        assert!(!metadata()?.formats.contains(&format(HISTORY)?));
    }
    let image = image();
    let png = png(&image)?;
    Offer::prepare(
        &ClipboardInput::Formats {
            text: "mixed".into(),
            png: png.clone(),
            cache: dir.path().into(),
        },
        43,
    )?
    .install()?;
    {
        let _clipboard = Clipboard::open(null_mut())?;
        assert_eq!(get(format("PNG")?, MAX_IMAGE)?, png);
        assert_eq!(
            image::load_from_memory(&dib_to_png(&get(DIBV5, MAX_DIB)?)?)?.to_rgba8(),
            image.to_rgba8()
        );
    }
    // Read native images without an origin marker, including DIB-only input.
    for format_id in [format("PNG")?, DIBV5] {
        let window = Window::new()?;
        {
            let _clipboard = Clipboard::open(window.0)?;
            ensure!(unsafe { EmptyClipboard() } != 0, "clear image fixture");
            let bytes = if format_id == DIBV5 {
                image_to_dib(&image)?
            } else {
                png.clone()
            };
            put(format_id, &bytes)?;
            put(format(CLOUD)?, &0u32.to_le_bytes())?;
        }
        let sequence = unsafe { GetClipboardSequenceNumber() };
        let snapshot = read_snapshot(sequence, false, true)?;
        let Some(Content::Image(bytes)) = snapshot.content else {
            anyhow::bail!("missing native image snapshot");
        };
        assert_eq!(
            image::load_from_memory(&bytes)?.to_rgba8(),
            image.to_rgba8()
        );
        assert!(read_snapshot(sequence, false, false)?.content.is_none());
    }
    for mode in ["sensitive", "sensitive-ignore", "sensitive-history"] {
        let path = dir.path().join(mode);
        let source = fixture(mode, &path)?;
        let result = read_snapshot(unsafe { GetClipboardSequenceNumber() }, false, true)?;
        assert!(result.content.is_none());
        assert!(
            !path.join("rendered").exists(),
            "sensitive payload was requested"
        );
        drop(source);
    }
    eprintln!(
        "PASS native writer: unicode, ticket, cloud/history, PNG/DIBV5, contention, sensitive delayed payload"
    );
    Ok(())
}
#[test]
#[ignore = "requires disposable interactive Windows console clipboard"]
fn native_clipboard_kills_delayed_reader() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let source = fixture("delayed", dir.path())?;
    let sequence = unsafe { GetClipboardSequenceNumber() };
    let mut reader = Reader::spawn(
        command(sequence, false, true)?,
        Duration::from_secs(5),
        OUTPUT_CAP,
        parse_output,
    )?;
    let started = Instant::now();
    loop {
        if let Some(result) = reader.poll() {
            assert!(result.is_err());
            break;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(8),
            "reader deadline stalled"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(reader);
    ensure!(
        started.elapsed() < Duration::from_secs(8),
        "reader reap stalled"
    );
    assert!(
        dir.path().join("rendered").exists(),
        "source was not asked to render"
    );
    drop(source);
    eprintln!(
        "PASS delayed provider: helper timed out and was killed/reaped in {:?}",
        started.elapsed()
    );
    Ok(())
}
async fn next_text(wire: &mut crate::client::PhoneConnection) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let message = wire
            .read(deadline.saturating_duration_since(Instant::now()))
            .await?;
        if let Message::Clipboard(text) = &message {
            let value = text.text.clone();
            wire.handle(message).await?;
            return Ok(value);
        }
        wire.handle(message).await?;
    }
}
async fn no_clipboard(wire: &mut crate::client::PhoneConnection) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(750);
    while Instant::now() < deadline {
        match wire
            .read(deadline.saturating_duration_since(Instant::now()))
            .await
        {
            Ok(message) => {
                ensure!(
                    !matches!(message, Message::Clipboard(_) | Message::FileBegin(_)),
                    "private/origin content escaped"
                );
                wire.handle(message).await?;
            }
            Err(_) => break,
        }
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires disposable interactive Windows console clipboard and PNEU_CLIPBOARD_TEST_EXE"]
async fn native_clipboard_watcher_actor_roundtrip() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let paths = Paths {
        data: dir.path().join("data"),
        cache: dir.path().join("cache"),
        config: dir.path().join("config/config.toml"),
        socket: dir.path().join("socket"),
    };
    let actions = Arc::new(RecordingActions {
        native: DesktopActions::new(paths.cache.clone()),
        copied: AtomicBool::new(false),
    });
    let core = Core::new(
        paths,
        Config {
            save_path: dir.path().join("saved"),
            clipboard_include_images: true,
            ..Config::default()
        },
        actions.clone(),
    )
    .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?.to_string();
    let server = tokio::spawn(core.clone().serve(listener, false));
    let startup = Offer::prepare(
        &ClipboardInput::Text("startup marker stays unread".into()),
        9999,
    )?;
    startup.install()?;
    let mut watcher = Some(Watch::start(core.clone())?);
    let result = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let phone = Phone::new(dir.path().join("phone"))?;
        let invitation = core.start_pairing_with_addresses(vec![address]).await?;
        let mut wire = phone.pair(&invitation).await?;
        wire.write(&Message::Ping(pneu_proto::Ping {
            nonce: pneu_proto::Nonce::new("f".repeat(16))?,
        }))
        .await?;
        loop {
            let m = wire.read(Duration::from_secs(5)).await?;
            if matches!(m, Message::Pong(_)) {
                break;
            }
            wire.handle(m).await?;
        }
        no_clipboard(&mut wire).await?;
        assert!(core.share_clipboard(crate::outbox::Targets::All("all".into())).await.is_err(), "startup marker payload became shareable");
        drop(startup);
        let status = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                "Set-Clipboard -Value 'Pneu M3 external process'",
            ])
            .status()?;
        ensure!(status.success(), "Set-Clipboard failed");
        assert_eq!(next_text(&mut wire).await?, "Pneu M3 external process");
        eprintln!("PASS external Set-Clipboard -> watcher -> actor -> paired test peer");
        let incoming = phone.text("Pneu M3 received 🦀".into(), ClipboardOrigin::Clipboard)?;
        let ack = tokio::time::timeout(Duration::from_secs(8), wire.send_text(incoming)).await??;
        assert!(!ack.deduplicated);
        assert_eq!(clipboard_text()?, "Pneu M3 received 🦀");
        let sequence = unsafe { GetClipboardSequenceNumber() };
        assert!(read_snapshot(sequence, true, false)?.marker.is_some());
        eprintln!("PASS phone -> actor -> text install -> watcher ticket confirmation -> ACK");
        let expected = image();
        let file = dir.path().join("incoming.png");
        std::fs::write(&file, alternate_png(&expected)?)?;
        let begin = phone
            .file_metadata(&file, FileKind::ClipboardImage, "image/png".into())
            .await?;
        tokio::time::timeout(Duration::from_secs(8), wire.send_file(begin, &file)).await??;
        let deadline=Instant::now()+Duration::from_secs(8);
        while !actions.copied.load(Ordering::SeqCst) {
            ensure!(Instant::now()<deadline,"image actor did not confirm installation");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        {
            let _clipboard = Clipboard::open(null_mut())?;
            assert_eq!(
                image::load_from_memory(&get(format("PNG")?, MAX_IMAGE)?)?.to_rgba8(),
                expected.to_rgba8()
            );
        }
        eprintln!(
            "PASS file ACK -> actor PNG/DIBV5 image install -> watcher ticket confirmation -> copy bookkeeping"
        );
        // The received PNG uses different encoding from our DIB conversion.
        // Re-offering only the DIB must retain the received image's provenance.
        {
            let window = Window::new()?;
            let _clipboard = Clipboard::open(window.0)?;
            ensure!(unsafe { EmptyClipboard() } != 0, "clear for DIB re-offer");
            put(DIBV5, &image_to_dib(&expected)?)?;
        }
        no_clipboard(&mut wire).await?;
        eprintln!("PASS markerless DIB-only re-offer is an echo");
        let unknown = Offer::prepare(
            &ClipboardInput::Text("unknown ticket is local content".into()), 999,
        )?;
        unknown.install()?;
        assert_eq!(next_text(&mut wire).await?, "unknown ticket is local content");
        drop(unknown);
        // A copied marker with a foreign HWND must be ignored as metadata.
        {
            let window = Window::new()?;
            let _clipboard = Clipboard::open(window.0)?;
            ensure!(unsafe { EmptyClipboard() } != 0, "clear for foreign marker");
            put(format(INSTALL)?, format!("{:032x}", 1).as_bytes())?;
            let bytes: Vec<_> = wide("foreign marker is local content").iter().flat_map(|c| c.to_le_bytes()).collect();
            put(TEXT, &bytes)?;
            assert!(!writer_owns_clipboard());
        }
        assert_eq!(next_text(&mut wire).await?, "foreign marker is local content");
        let secret = fixture("sensitive", &dir.path().join("secret"))?;
        no_clipboard(&mut wire).await?;
        assert!(!dir.path().join("secret/rendered").exists());
        drop(secret);
        eprintln!("PASS unknown/foreign tickets read local payload; sensitive delayed content stays unread");
        set_text("cached before watcher loss")?;
        assert_eq!(next_text(&mut wire).await?, "cached before watcher loss");
        core.set_setting("clipboard_mode".into(), "manual".into()).await?;
        core.share_clipboard(crate::outbox::Targets::All("all".into())).await?;
        assert_eq!(next_text(&mut wire).await?, "cached before watcher loss");
        let stopped = watcher.take().unwrap();
        unsafe { PostThreadMessageW(stopped.id, WM_QUIT, 0, 0); }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !stopped.is_finished() {
            ensure!(Instant::now() < deadline, "forced watcher exit stalled");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stopped.shutdown().await;
        core.clipboard_test_idle().await;
        assert!(core.share_clipboard(crate::outbox::Targets::All("all".into())).await.is_err(), "watcher loss left cached share authority");
        set_text("change during watcher outage")?;
        watcher = Some(Watch::start(core.clone())?);
        tokio::time::sleep(Duration::from_millis(150)).await;
        core.clipboard_test_idle().await;
        assert!(core.share_clipboard(crate::outbox::Targets::All("all".into())).await.is_err(), "restart baseline became shareable");
        no_clipboard(&mut wire).await?;
        core.set_setting("clipboard_mode".into(), "automatic".into()).await?;
        set_text("fresh copy after restart")?;
        assert_eq!(next_text(&mut wire).await?, "fresh copy after restart");
        eprintln!("PASS unexpected watcher loss revokes cached shares; outage content stays baseline; fresh copies recover");
        wire.close().await?;
        Ok(())
    }
    .await;
    if let Some(watcher) = watcher {
        watcher.shutdown().await;
    }
    core.request_shutdown();
    tokio::time::timeout(Duration::from_secs(5), server).await???;
    core.shutdown_actions().await?;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires disposable interactive Windows console clipboard and PNEU_CLIPBOARD_TEST_EXE"]
async fn native_clipboard_shutdown_cancels_delayed_reader() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let paths = Paths {
        data: dir.path().join("data"),
        cache: dir.path().join("cache"),
        config: dir.path().join("config/config.toml"),
        socket: dir.path().join("socket"),
    };
    let actions = Arc::new(DesktopActions::new(paths.cache.clone()));
    let core = Core::new(
        paths,
        Config {
            save_path: dir.path().join("saved"),
            ..Config::default()
        },
        actions,
    )
    .await?;
    let watcher = Watch::start(core.clone())?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let source = fixture("delayed", &dir.path().join("blocked"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !dir.path().join("blocked/rendered").exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let requested = dir.path().join("blocked/rendered").exists();
    let started = Instant::now();
    watcher.shutdown().await;
    let elapsed = started.elapsed();
    drop(source);
    core.shutdown_actions().await?;
    ensure!(requested, "watcher did not start delayed provider read");
    ensure!(
        elapsed < Duration::from_secs(1),
        "watcher shutdown stalled: {elapsed:?}"
    );
    eprintln!("PASS watcher shutdown canceled/reaped blocked helper in {elapsed:?}");
    Ok(())
}

fn alternate_png(image: &image::DynamicImage) -> Result<Vec<u8>> {
    use image::{
        ImageEncoder,
        codecs::png::{CompressionType, FilterType, PngEncoder},
    };
    let rgba = image.to_rgba8();
    let mut bytes = Vec::new();
    PngEncoder::new_with_quality(&mut bytes, CompressionType::Best, FilterType::Paeth)
        .write_image(
            &rgba,
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )?;
    assert_ne!(bytes, dib_to_png(&image_to_dib(image)?)?);
    Ok(bytes)
}
fn set_text(text: &str) -> Result<()> {
    let window = Window::new()?;
    let _clipboard = Clipboard::open(window.0)?;
    ensure!(unsafe { EmptyClipboard() } != 0, "clear text fixture");
    let bytes: Vec<_> = wide(text).iter().flat_map(|c| c.to_le_bytes()).collect();
    put(TEXT, &bytes)
}
fn wait_reader<T: Send + 'static>(reader: &mut Reader<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(result) = reader.poll() {
            return result;
        }
        ensure!(Instant::now() < deadline, "reader stalled");
        std::thread::sleep(Duration::from_millis(5));
    }
}
async fn test_core(path: &Path) -> Result<Arc<Core>> {
    Core::new(
        Paths {
            data: path.join("data"),
            cache: path.join("cache"),
            config: path.join("config/config.toml"),
            socket: path.join("socket"),
        },
        Config {
            save_path: path.join("saved"),
            ..Config::default()
        },
        Arc::new(DesktopActions::new(path.join("cache"))),
    )
    .await
}
fn new_pending(core: &Arc<Core>, sequence: u32, marker: bool, timeout: Duration) -> Pending {
    let (_, token) = core.observe_watched_clipboard_kind(false, false, false);
    let canceled = Arc::new(AtomicBool::new(false));
    core.register_clipboard_read(token, canceled.clone());
    Pending {
        reader: None,
        core: Arc::downgrade(core),
        token,
        sequence,
        canceled,
        marker,
        initial: false,
        deadline: Instant::now() + timeout,
        completed: false,
    }
}
#[test]
#[ignore = "internal reader job subprocess"]
fn clipboard_reader_job_fixture() -> Result<()> {
    use std::io::Read;
    use windows_sys::Win32::System::JobObjects::*;
    let mut go = [0];
    std::io::stdin().read_exact(&mut go)?;
    assert_eq!(&go, b"g");
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe {
            QueryInformationJobObject(
                null_mut(),
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
                null_mut(),
            )
        } != 0,
        "query assigned job"
    );
    assert_eq!(limits.ProcessMemoryLimit, 384 * 1024 * 1024);
    assert_ne!(
        limits.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        0
    );
    let memory = unsafe {
        VirtualAlloc(
            null(),
            512 * 1024 * 1024,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        )
    };
    if !memory.is_null() {
        unsafe {
            VirtualFree(memory, 0, MEM_RELEASE);
        }
    }
    ensure!(memory.is_null(), "reader exceeded process memory limit");
    println!("JOB_OK: go received, job assigned, memory cap enforced");
    Ok(())
}
#[test]
#[ignore = "requires disposable interactive Windows console clipboard and PNEU_CLIPBOARD_TEST_EXE"]
fn native_clipboard_reader_job_and_go_gate() -> Result<()> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args([
        "--exact",
        "ui::clipboard::tests::clipboard_reader_job_fixture",
        "--ignored",
        "--nocapture",
    ]);
    let mut reader = Reader::spawn(cmd, Duration::from_secs(5), 8192, |b| {
        Ok(String::from_utf8(b)?)
    })?;
    assert!(wait_reader(&mut reader)?.contains("JOB_OK"));
    drop(reader);
    let dir = crate::test_support::scratch()?;
    let source = fixture("delayed", dir.path())?;
    let mut child = command(unsafe { GetClipboardSequenceNumber() }, false, true)?
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    eprintln!("ungated helper PID={}", child.id());
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, child.id()) };
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !dir.path().join("rendered").exists(),
        "helper touched clipboard before go"
    );
    drop(child.stdin.take());
    child.wait()?;
    assert_eq!(unsafe { WaitForSingleObject(process, 0) }, WAIT_OBJECT_0);
    unsafe {
        CloseHandle(process);
    }
    drop(source);
    eprintln!("PASS helper go gate and 384 MiB Job Object cap");
    Ok(())
}
#[test]
#[ignore = "requires disposable interactive Windows console clipboard"]
fn native_clipboard_atomic_failures_and_fallbacks() -> Result<()> {
    let window = Window::new()?;
    let input = ClipboardInput::Formats {
        text: "atomic".into(),
        png: png(&image())?,
        cache: PathBuf::new(),
    };
    let offer = Offer::prepare(&input, 42)?;
    let cancel = AtomicBool::new(false);
    WRITE_FAULT.set((None, 0));
    install_payload(window.0, &offer.payload, 42, &cancel, &|| true)?;
    let steps = WRITE_FAULT.get().1;
    for step in 0..steps {
        WRITE_FAULT.set((None, 0));
        set_text("unchanged before destructive step")?;
        WRITE_FAULT.set((Some(step), 0));
        let result = install_payload(window.0, &offer.payload, 42, &cancel, &|| true);
        WRITE_FAULT.set((None, 0));
        assert!(result.is_err(), "fault {step} did not fire");
        let _clipboard = Clipboard::open(null_mut())?;
        assert_eq!(
            unsafe { IsClipboardFormatAvailable(format(INSTALL)?) },
            0,
            "failed transaction {step} can be confirmed"
        );
        if step >= offer.payload.len() * 2 + 5 {
            assert!(
                metadata()?.formats.is_empty(),
                "partial payload survived fault {step}"
            );
        } else {
            assert!(
                get(TEXT, 1024).is_ok(),
                "preallocation failure destroyed selection"
            );
        }
    }
    // Authority can change after acquisition without waiting for the async
    // driver's next poll: the final boundary check must still reject the write.
    set_text("authority boundary survives")?;
    let checks = std::sync::atomic::AtomicUsize::new(0);
    assert!(
        install_payload(window.0, &offer.payload, 42, &cancel, &|| checks
            .fetch_add(1, Ordering::SeqCst)
            == 0)
        .is_err()
    );
    assert_eq!(clipboard_text()?, "authority boundary survives");
    // Failing PNG fetch/decoding/cap, then failing DIBV5, must still try DIB.
    let dib = image_to_dib(&image())?;
    for bad_png in [
        vec![0; 64],
        b"\x89PNG\r\n\x1a\ntruncated".to_vec(),
        vec![0; MAX_IMAGE + 1],
    ] {
        {
            let _clipboard = Clipboard::open(window.0)?;
            ensure!(unsafe { EmptyClipboard() } != 0, "clear fallback fixture");
            put(format("PNG")?, &bad_png)?;
            put(DIBV5, b"bad DIBV5")?;
            put(DIB, &dib)?;
        }
        let result = read_snapshot(unsafe { GetClipboardSequenceNumber() }, false, true)?;
        let Some(Content::Image(bytes)) = result.content else {
            anyhow::bail!("DIB fallback missing");
        };
        assert_eq!(
            image::load_from_memory(&bytes)?.to_rgba8(),
            image().to_rgba8()
        );
    }
    let alternate = alternate_png(&image())?;
    let offer = Offer::prepare(
        &ClipboardInput::Bytes {
            bytes: alternate.clone(),
            mime: "image/png".into(),
            cache: PathBuf::new(),
        },
        43,
    )?;
    assert!(offer.hashes().contains(&Sha256::digest(&alternate).into()));
    assert!(
        offer
            .hashes()
            .contains(&Sha256::digest(dib_to_png(&dib)?).into())
    );
    // Missing, malformed and disallowing history values all fail closed.
    for flag in [
        vec![],
        vec![1],
        vec![1, 0, 0, 0, 0],
        0u32.to_le_bytes().to_vec(),
        2u32.to_le_bytes().to_vec(),
    ] {
        {
            let _clipboard = Clipboard::open(window.0)?;
            ensure!(unsafe { EmptyClipboard() } != 0, "clear history fixture");
            put(format(HISTORY)?, &flag)?;
            put(
                TEXT,
                &wide("private")
                    .iter()
                    .flat_map(|c| c.to_le_bytes())
                    .collect::<Vec<_>>(),
            )?;
        }
        let result = read_snapshot(unsafe { GetClipboardSequenceNumber() }, false, true)?;
        assert_eq!(result.status, SnapshotStatus::Sensitive);
        assert!(result.content.is_none());
    }
    eprintln!(
        "PASS all {steps} write fault points; PNG/DIBV5 -> DIB fallback; DIB hashes; fail-closed history"
    );
    Ok(())
}
#[test]
#[ignore = "requires disposable interactive Windows console clipboard and PNEU_CLIPBOARD_TEST_EXE"]
fn native_clipboard_rechecks_privacy_after_render() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    for mode in [
        "privacy-on-render-exclude",
        "privacy-on-render-ignore",
        "privacy-on-render-history",
    ] {
        let path = dir.path().join(mode);
        let source = fixture(mode, &path)?;
        let mut reader = Reader::spawn(
            command(unsafe { GetClipboardSequenceNumber() }, false, true)?,
            Duration::from_secs(5),
            OUTPUT_CAP,
            parse_output,
        )?;
        let result = wait_reader(&mut reader)?;
        assert_ne!(
            result.status,
            SnapshotStatus::Ready,
            "rendering privacy escaped"
        );
        assert!(result.content.is_none());
        assert_eq!(std::fs::read_to_string(path.join("privacy-added"))?, "true");
        assert!(
            !path.join(format!("rendered-{}", format("PNG")?)).exists(),
            "requested second format after privacy change"
        );
        drop(reader);
        drop(source);
    }
    eprintln!(
        "PASS render-time exclude/ignore/history additions abort before copying or requesting PNG"
    );
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires disposable interactive Windows console clipboard and PNEU_CLIPBOARD_TEST_EXE"]
async fn native_clipboard_cancels_contended_install_and_retries_reader() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    set_text("must survive canceled install")?;
    let sequence = unsafe { GetClipboardSequenceNumber() };
    {
        let _clipboard = Clipboard::open(null_mut())?;
        metadata()?;
    }
    let holder_path = dir.path().join("holder");
    let holder = fixture("hold-until-release", &holder_path)?;
    let actions = DesktopActions::new(dir.path().join("cache"));
    let mut prepared = actions
        .prepare_clipboard(ClipboardInput::Text("revoked".into()), 42)
        .await?;
    let mut install = Box::pin(actions.install_clipboard(&mut prepared));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut install)
            .await
            .is_err()
    );
    drop(install); // exactly how driver revocation/supersession cancels native writes
    prepared.stop().await;
    tokio::time::timeout(Duration::from_secs(1), actions.drain_clipboard()).await?;
    std::fs::write(holder_path.join("release"), b"release")?;
    drop(holder);
    assert_eq!(clipboard_text()?, "must survive canceled install");
    assert_eq!(unsafe { GetClipboardSequenceNumber() }, sequence);
    // Live authority also revokes a worker while its awaiting future remains
    // polled, independently of the future-drop cancellation mechanism.
    let holder_path = dir.path().join("authority-holder");
    let holder = fixture("hold-until-release", &holder_path)?;
    let allowed = Arc::new(AtomicBool::new(true));
    let authority = allowed.clone();
    let mut prepared = actions
        .prepare_clipboard(ClipboardInput::Text("authority revoked".into()), 43)
        .await?;
    prepared.set_install_authority(move || authority.load(Ordering::SeqCst));
    let mut install = Box::pin(actions.install_clipboard(&mut prepared));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut install)
            .await
            .is_err()
    );
    allowed.store(false, Ordering::SeqCst);
    std::fs::write(holder_path.join("release"), b"release")?;
    drop(holder);
    assert!(install.await.is_err());
    prepared.stop().await;
    actions.drain_clipboard().await;
    assert_eq!(clipboard_text()?, "must survive canceled install");
    assert_eq!(unsafe { GetClipboardSequenceNumber() }, sequence);
    // Contention begins after metadata and exceeds the old 250 ms helper limit.
    let holder_path = dir.path().join("reader-holder");
    let holder = fixture("hold-until-release", &holder_path)?;
    let mut reader = Reader::spawn(
        command(sequence, false, false)?,
        Duration::from_secs(5),
        OUTPUT_CAP,
        parse_output,
    )?;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        reader.poll().is_none(),
        "reader gave up during transient contention"
    );
    std::fs::write(holder_path.join("release"), b"release")?;
    drop(holder);
    let result = wait_reader(&mut reader)?;
    assert!(
        matches!(result.content, Some(Content::Text(t)) if t == "must survive canceled install")
    );
    actions.shutdown().await?;
    eprintln!("PASS canceled writer never writes after release; reader retries 600 ms contention");
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires disposable interactive Windows console clipboard"]
async fn native_clipboard_retiring_replacement_keeps_serving_predecessor() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let actions = DesktopActions::new(dir.path().join("cache"));
    let mut first = actions
        .prepare_clipboard(ClipboardInput::Text("first stays installed".into()), 101)
        .await?;
    actions.install_clipboard(&mut first).await?;
    let owner = WRITER.load(Ordering::SeqCst);
    let sequence = unsafe { GetClipboardSequenceNumber() };
    for cancel in [false, true] {
        let mut second = actions
            .prepare_clipboard(ClipboardInput::Text("must never install".into()), 102)
            .await?;
        if cancel {
            let holder_path = dir.path().join("holder");
            let holder = fixture("hold-until-release", &holder_path)?;
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    actions.install_clipboard(&mut second),
                )
                .await
                .is_err()
            );
            tokio::time::timeout(Duration::from_secs(1), second.stop()).await?;
            tokio::time::timeout(Duration::from_secs(1), actions.drain_clipboard()).await?;
            std::fs::write(holder_path.join("release"), b"release")?;
            drop(holder);
        } else {
            // Cancellation while prepared, before the driver authorizes write.
            second.stop().await;
            tokio::time::timeout(Duration::from_secs(1), actions.drain_clipboard()).await?;
        }
        assert_eq!(clipboard_text()?, "first stays installed");
        assert_eq!(unsafe { GetClipboardSequenceNumber() }, sequence);
        assert_eq!(WRITER.load(Ordering::SeqCst), owner);
        assert_eq!(unsafe { GetClipboardOwner() } as isize, owner);
    }
    let mut third = actions
        .prepare_clipboard(ClipboardInput::Text("third installed".into()), 103)
        .await?;
    tokio::time::timeout(
        Duration::from_secs(1),
        actions.install_clipboard(&mut third),
    )
    .await??;
    assert_eq!(clipboard_text()?, "third installed");
    first.stop().await;
    assert!(writer_owns_clipboard());
    third.stop().await;
    actions.shutdown().await?;
    eprintln!("PASS replacement retirement drains independently; third native install succeeds");
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires disposable interactive Windows console clipboard and PNEU_CLIPBOARD_TEST_EXE"]
async fn native_clipboard_marker_fallback_keeps_token_deadline_and_cancel() -> Result<()> {
    for cancel in [false, true] {
        let dir = crate::test_support::scratch()?;
        let core = test_core(dir.path()).await?;
        let source = fixture("marker-then-block", &dir.path().join("source"))?;
        let sequence = unsafe { GetClipboardSequenceNumber() };
        let mut read = new_pending(&core, sequence, true, Duration::from_millis(900));
        let (token, deadline, canceled) = (read.token, read.deadline, read.canceled.clone());
        read.start(true)?;
        let mut pending = Some(read);
        let started = Instant::now();
        while pending.as_ref().is_some_and(|p| p.marker) {
            service_pending(&core, unsafe { GetClipboardSequenceNumber() }, &mut pending)?;
            ensure!(
                started.elapsed() < Duration::from_secs(2),
                "marker phase stalled"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let read = pending
            .as_ref()
            .context("marker did not fall through to payload")?;
        assert_eq!(read.token, token);
        assert_eq!(read.deadline, deadline);
        assert!(Arc::ptr_eq(&read.canceled, &canceled));
        if cancel {
            canceled.store(true, Ordering::SeqCst);
        }
        let mut timed_out = false;
        while pending.is_some() {
            if let Err(error) =
                service_pending(&core, unsafe { GetClipboardSequenceNumber() }, &mut pending)
            {
                ensure!(
                    !cancel && error.to_string().contains("timed out"),
                    "unexpected marker fallback error: {error}"
                );
                timed_out = true;
            }
            ensure!(
                started.elapsed() < Duration::from_millis(1500),
                "fallback extended deadline"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(timed_out, !cancel);
        drop(source);
        core.shutdown_actions().await?;
    }
    eprintln!("PASS marker fallback retains observation, cancellation and absolute deadline");
    Ok(())
}

#[test]
#[ignore = "internal idle reader subprocess"]
fn clipboard_reader_idle_fixture() -> Result<()> {
    use std::io::Read;
    let mut go = [0];
    std::io::stdin().read_exact(&mut go)?;
    assert_eq!(&go, b"g");
    let directory =
        PathBuf::from(std::env::var_os("PNEU_M3_IDLE_DIR").context("idle fixture only")?);
    std::fs::write(directory.join("pid"), std::process::id().to_string())?;
    std::thread::sleep(Duration::from_secs(60));
    Ok(())
}
fn idle_reader(directory: &Path) -> Result<(Reader<Snapshot>, HANDLE)> {
    std::fs::create_dir_all(directory)?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args([
        "--exact",
        "ui::clipboard::tests::clipboard_reader_idle_fixture",
        "--ignored",
        "--nocapture",
    ])
    .env("PNEU_M3_IDLE_DIR", directory);
    let reader = Reader::spawn(cmd, Duration::from_secs(5), OUTPUT_CAP, parse_output)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !directory.join("pid").exists() {
        ensure!(Instant::now() < deadline, "idle helper startup stalled");
        std::thread::sleep(Duration::from_millis(5));
    }
    let pid: u32 = std::fs::read_to_string(directory.join("pid"))?.parse()?;
    eprintln!("idle helper PID={pid}");
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    ensure!(!handle.is_null(), "open idle helper witness");
    Ok((reader, handle))
}
fn cancellation_before_reap(canceled: Arc<AtomicBool>) {
    BEFORE_REAP.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !canceled.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                canceled.load(Ordering::SeqCst),
                "old reader reaped before replacement/invalidation reached actor"
            );
        }))
    });
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires disposable interactive Windows console clipboard and PNEU_CLIPBOARD_TEST_EXE"]
async fn native_clipboard_replacement_and_metadata_error_reap_pending() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let core = test_core(dir.path()).await?;
    for metadata_error in [false, true] {
        set_text("previous observation")?;
        let sequence = unsafe { GetClipboardSequenceNumber() };
        let mut read = new_pending(&core, sequence, false, Duration::from_secs(5));
        let (reader, process) = idle_reader(&dir.path().join(if metadata_error {
            "error"
        } else {
            "replacement"
        }))?;
        read.reader = Some(reader);
        let old_token = read.token;
        core.clipboard_test_idle().await;
        cancellation_before_reap(read.canceled.clone());
        let mut pending = Some(read);
        let mut baseline = Some(sequence);
        if metadata_error {
            let window = Window::new()?;
            let _clipboard = Clipboard::open(window.0)?;
            ensure!(
                unsafe { EmptyClipboard() } != 0,
                "clear enumeration fixture"
            );
            for i in 0..4097 {
                put(format(&format!("Pneu.M3.Enumeration.{i}"))?, b"x")?;
            }
        } else {
            set_text("replacement local content")?;
        }
        let result = poll(&core, &mut baseline, &mut pending);
        if metadata_error {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("format count limit")
            );
            assert!(pending.is_none());
        } else {
            result?;
            assert_ne!(
                pending.as_ref().context("replacement missing")?.token,
                old_token
            );
        }
        assert_eq!(
            unsafe { WaitForSingleObject(process, 0) },
            WAIT_OBJECT_0,
            "old helper survived"
        );
        unsafe {
            CloseHandle(process);
        }
        drop(pending);
        core.clipboard_test_idle().await;
    }
    set_text("metadata recovery baseline")?;
    core.shutdown_actions().await?;
    eprintln!(
        "PASS replacement/invalidation precede retirement; >4096 formats cancel and reap pending helper"
    );
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires disposable interactive Windows console clipboard and PNEU_CLIPBOARD_TEST_EXE"]
async fn native_clipboard_delayed_history_never_blocks_watcher() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    set_text("history test baseline")?;
    let core = test_core(dir.path()).await?;
    let watcher = Watch::start(core.clone())?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let path = dir.path().join("source");
    let source = fixture("delayed-history", &path)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.join("rendered").exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let history_requested = path.join(format!("rendered-{}", format(HISTORY)?)).exists();
    let started = Instant::now();
    watcher.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "history provider blocked watcher shutdown"
    );
    assert!(history_requested, "helper did not request delayed history");
    assert!(
        !path.join(format!("rendered-{TEXT}")).exists(),
        "payload requested before history allowed it"
    );
    drop(source);
    core.shutdown_actions().await?;
    eprintln!(
        "PASS delayed history is confined to killable helper; watcher shutdown {:?}",
        started.elapsed()
    );
    Ok(())
}
