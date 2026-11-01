#![cfg(unix)]
#[path = "../src/test_support.rs"]
mod test_support;
use anyhow::{Context, Result};
use async_trait::async_trait;
use pneu_proto::*;
use pneud::{
    Config, Core, Paths,
    actions::{PcActions, ReceivedFile},
    client::{PcRejection, Phone, PhoneConnection},
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Default)]
struct Actions {
    block_install: std::sync::atomic::AtomicBool,
    install_entered: tokio::sync::Notify,
    install_release: tokio::sync::Notify,
    text: Mutex<Vec<String>>,
    images: Mutex<Vec<PathBuf>>,
    image_bytes: Mutex<Vec<Vec<u8>>>,
    notices: Mutex<Vec<PathBuf>>,
    notice_mimes: Mutex<Vec<String>>,
    changed: tokio::sync::Notify,
    denied: std::sync::atomic::AtomicBool,
    opens: Mutex<Vec<PathBuf>>,
    block_image: std::sync::atomic::AtomicBool,
    deny_image: std::sync::atomic::AtomicBool,
    block_text: std::sync::atomic::AtomicBool,
    text_entered: tokio::sync::Notify,
    text_release: tokio::sync::Notify,
    clipboard_notices: Mutex<Vec<PathBuf>>,
    copy_failures: std::sync::atomic::AtomicUsize,
    image_entered: tokio::sync::Notify,
    image_release: tokio::sync::Notify,
    image_finished: tokio::sync::Notify,
    block_notice: std::sync::atomic::AtomicBool,
    notice_entered: tokio::sync::Notify,
    notice_release: tokio::sync::Notify,
    receipt_notices: Mutex<Vec<String>>,
    failure_saved: Mutex<Vec<bool>>,
}
// Signal completion even when connection replacement cancels a blocked action.
struct ImageAttempt<'a>(&'a tokio::sync::Notify);
impl Drop for ImageAttempt<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
impl Actions {
    async fn receipt_notice(&self, notice: String) -> Result<()> {
        if self.block_notice.load(std::sync::atomic::Ordering::SeqCst) {
            self.notice_entered.notify_one();
            self.notice_release.notified().await;
        }
        self.receipt_notices.lock().unwrap().push(notice);
        self.changed.notify_one();
        Ok(())
    }
}
#[async_trait]
impl PcActions for Actions {
    async fn clipboard_text(&self, text: &str) -> Result<()> {
        if self.block_text.load(std::sync::atomic::Ordering::SeqCst) {
            self.text_entered.notify_one();
            self.text_release.notified().await;
        }
        anyhow::ensure!(
            !self.denied.load(std::sync::atomic::Ordering::SeqCst),
            "clipboard unavailable"
        );
        self.text.lock().unwrap().push(text.into());
        self.changed.notify_one();
        Ok(())
    }
    async fn clipboard_image(&self, path: &Path, _: &str) -> Result<()> {
        if self.block_image.load(std::sync::atomic::Ordering::SeqCst) {
            self.image_entered.notify_one();
            self.image_release.notified().await;
        }
        anyhow::ensure!(
            !self.deny_image.load(std::sync::atomic::Ordering::SeqCst),
            "image copy denied"
        );
        self.images.lock().unwrap().push(path.into());
        self.changed.notify_one();
        Ok(())
    }
    async fn clipboard_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        self.clipboard_image(file.path(), mime).await?;
        self.image_bytes
            .lock()
            .unwrap()
            .push(file.read_bounded(16 * 1024 * 1024)?);
        Ok(())
    }
    async fn prepare_clipboard(
        &self,
        input: pneud::actions::ClipboardInput,
        _ticket: u128,
    ) -> Result<pneud::actions::PreparedClipboard> {
        match &input {
            pneud::actions::ClipboardInput::Text(_) => {
                if self.block_text.load(std::sync::atomic::Ordering::SeqCst) {
                    self.text_entered.notify_one();
                    self.text_release.notified().await;
                }
            }
            _ => {
                let _finished = ImageAttempt(&self.image_finished);
                if self.block_image.load(std::sync::atomic::Ordering::SeqCst) {
                    self.image_entered.notify_one();
                    self.image_release.notified().await;
                }
            }
        }
        pneud::actions::PreparedClipboard::immediate(input)
    }
    async fn install_clipboard(
        &self,
        prepared: &mut pneud::actions::PreparedClipboard,
    ) -> Result<()> {
        if self.block_install.load(std::sync::atomic::Ordering::SeqCst) {
            self.install_entered.notify_one();
            self.install_release.notified().await;
        }
        match prepared.input() {
            pneud::actions::ClipboardInput::Text(text) => {
                anyhow::ensure!(
                    !self.denied.load(std::sync::atomic::Ordering::SeqCst),
                    "clipboard unavailable"
                );
                self.text.lock().unwrap().push(text.clone());
            }
            pneud::actions::ClipboardInput::Image(file, _) => {
                anyhow::ensure!(
                    !self.deny_image.load(std::sync::atomic::Ordering::SeqCst),
                    "image copy denied"
                );
                self.images.lock().unwrap().push(file.path().into());
                self.image_bytes
                    .lock()
                    .unwrap()
                    .push(file.read_bounded(16 * 1024 * 1024)?);
            }
            _ => anyhow::bail!("unnormalized clipboard input"),
        }
        self.changed.notify_one();
        Ok(())
    }
    async fn notify(&self, path: &Path, mime: &str) -> Result<()> {
        self.notices.lock().unwrap().push(path.into());
        self.notice_mimes.lock().unwrap().push(mime.into());
        self.changed.notify_one();
        Ok(())
    }
    async fn notify_clipboard_image(&self, path: &Path, _: &str) -> Result<()> {
        self.clipboard_notices.lock().unwrap().push(path.to_owned());
        self.changed.notify_one();
        Ok(())
    }
    async fn notify_clipboard_failure(&self) -> Result<()> {
        self.copy_failures
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.changed.notify_one();
        Ok(())
    }
    async fn notify_text(&self, text: &str) -> Result<()> {
        self.receipt_notice(text.into()).await
    }
    async fn notify_text_failure(&self) -> Result<()> {
        self.receipt_notice("text failed".into()).await
    }
    async fn notify_save_failure(
        &self,
        _: &FileBegin,
        code: NackCode,
        destination: pneud::ui::ReceiveDestination,
        _: &Path,
    ) -> Result<()> {
        self.receipt_notice(format!("{destination:?}: {code:?}"))
            .await
    }
    async fn notify_image_failure(
        &self,
        _: &ReceivedFile,
        _: &FileBegin,
        saved: bool,
    ) -> Result<()> {
        self.failure_saved.lock().unwrap().push(saved);
        self.notify_clipboard_failure().await
    }
    async fn open(&self, path: &Path) -> Result<()> {
        self.opens.lock().unwrap().push(path.into());
        Ok(())
    }
}
#[test]
fn scratch_fixtures_follow_home_and_xdg_cache() -> Result<()> {
    if let Some(expected) = std::env::var_os("PNEU_TEST_EXPECTED_SCRATCH_CACHE") {
        let dir = test_support::scratch()?;
        assert!(dir.path().starts_with(std::path::PathBuf::from(expected)));
        return Ok(());
    }
    let home = tempfile::tempdir()?;
    for xdg in [false, true] {
        let expected = home
            .path()
            .join(if xdg { "custom-cache" } else { ".cache" });
        let mut child = std::process::Command::new(std::env::current_exe()?);
        child
            .args(["--exact", "scratch_fixtures_follow_home_and_xdg_cache"])
            .env("HOME", home.path())
            .env_remove("XDG_CACHE_HOME")
            .env("PNEU_TEST_EXPECTED_SCRATCH_CACHE", &expected);
        if xdg {
            child.env("XDG_CACHE_HOME", &expected);
        }
        let output = child.output()?;
        assert!(
            output.status.success(),
            "fixture cache ignored environment: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
struct Harness {
    dir: tempfile::TempDir,
    core: Arc<Core>,
    task: JoinHandle<Result<()>>,
    actions: Arc<Actions>,
    address: String,
    // Retain the bound socket through daemon restarts; no release/rebind race.
    listener: std::net::TcpListener,
}
impl Harness {
    async fn new() -> Result<Self> {
        Self::mesh().await
    }
    async fn mesh() -> Result<Self> {
        Self::with_dir(test_support::scratch()?).await
    }
    async fn with_dir(dir: tempfile::TempDir) -> Result<Self> {
        let actions = Arc::new(Actions::default());
        let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
        reservation.set_nonblocking(true)?;
        let listener = TcpListener::from_std(reservation.try_clone()?)?;
        let address = listener.local_addr()?.to_string();
        let paths = Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: dir.path().join("pneud.sock"),
        };
        let config = Config {
            port: listener.local_addr()?.port(),
            save_path: dir.path().join("saved"),
            ..Config::default()
        };
        let core = Core::new(paths, config, actions.clone()).await?;
        eprintln!(
            "mock Core PID={} port={} directory={} (no mDNS)",
            std::process::id(),
            core.bound_port(),
            dir.path().display()
        );
        let task = tokio::spawn(core.clone().serve(listener, false));
        Ok(Self {
            dir,
            core,
            task,
            actions,
            address,
            listener: reservation,
        })
    }
    fn phone(&self, name: &str) -> Result<Phone> {
        Phone::new(self.dir.path().join(name))
    }
    async fn pair(&self, phone: &Phone) -> Result<PhoneConnection> {
        let invitation = self
            .core
            .start_pairing_with_addresses(vec![self.address.clone()])
            .await?;
        let mut events = self.core.subscribe();
        let connection = phone.pair(&invitation).await?;
        event_matching(&mut events, |e| matches!(e, pneud::Event::ConnectionChanged { device, connected: true } if *device == phone.identity.device_id)).await?;
        Ok(connection)
    }
    async fn stop(self) -> Result<()> {
        self.core.request_shutdown();
        let result = tokio::time::timeout(Duration::from_secs(15), self.task).await??;
        eprintln!(
            "mock Core stopped PID={} port={}",
            std::process::id(),
            self.core.bound_port()
        );
        result
    }
    async fn wait_images(&self, n: usize) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.actions.images.lock().unwrap().len() < n {
                self.actions.changed.notified().await;
            }
        })
        .await?;
        Ok(())
    }
    fn saved(&self) -> Result<Vec<PathBuf>> {
        Ok(std::fs::read_dir(self.core.config().save_path)?
            .filter(|r| r.as_ref().is_ok_and(|e| e.path().is_file()))
            .map(|r| r.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?)
    }
}
type PhoneTls = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;
async fn raw_connect(h: &Harness, phone: &Phone) -> Result<PhoneTls> {
    let config = pneud::tls::client_config(&phone.identity, h.core.identity.fingerprint.clone())?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(&h.address).await?;
    Ok(tokio::time::timeout(
        Duration::from_secs(2),
        connector.connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp),
    )
    .await??)
}
fn admission_rejection() -> Message {
    Message::Error(ErrorMessage {
        code: ErrorCode::Unauthorized,
        message: Some("Client certificate is not admitted".into()),
        supported_version: None,
    })
}
async fn assert_admission_rejection(tls: &mut PhoneTls) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        assert_eq!(read_frame(tls).await?, Some(admission_rejection()));
        assert_eq!(read_frame(tls).await?, None, "fatal rejection closes TLS");
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
fn assert_simulator_rejection(error: &anyhow::Error) {
    let rejection = error
        .downcast_ref::<PcRejection>()
        .expect("authenticated PC ERROR");
    assert_eq!(Message::Error(rejection.0.clone()), admission_rejection());
    assert!(error.to_string().contains("unauthorized"));
}

fn metadata(id: Id, bytes: &[u8]) -> FileBegin {
    FileBegin {
        resume: None,
        folder: None,
        sensitive: false,
        hlc: None,
        source_device: None,
        transfer_id: id,
        file_name: "capture.png".into(),
        mime: "image/png".into(),
        size: bytes.len() as u64,
        sha256: Hash::new(hex::encode(Sha256::digest(bytes))).unwrap(),
        kind: FileKind::Screenshot,
        created_at: 0,
        source_media_id: None,
    }
}
async fn begin_chunks(
    connection: &mut PhoneConnection,
    meta: &FileBegin,
    bytes: &[u8],
) -> Result<()> {
    connection.write(&Message::FileBegin(meta.clone())).await?;
    if !bytes.is_empty() {
        connection
            .write(&Message::FileChunk(FileChunk {
                transfer_id: meta.transfer_id.clone(),
                offset: 0,
                data: bytes.to_vec(),
            }))
            .await?;
    }
    Ok(())
}
async fn end(connection: &mut PhoneConnection, id: Id) -> Result<Message> {
    connection
        .write(&Message::FileEnd(FileEnd { transfer_id: id }))
        .await?;
    connection.read(Duration::from_secs(3)).await
}

#[tokio::test]
async fn pair_file_durable_ack_duplicate_and_clipboard() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let bytes = b"\x89PNG\r\n\x1a\noriginal image bytes";
    let source = h.dir.path().join("original.png");
    std::fs::write(&source, bytes)?;
    let meta = metadata(Id::from_bytes(rand::random()), bytes);
    assert!(!c.send_file(meta.clone(), &source).await?.deduplicated);
    h.wait_images(1).await?;
    let paths = h.saved()?;
    assert_eq!(paths.len(), 1);
    assert_eq!(std::fs::read(&paths[0])?, bytes);
    assert!(c.send_file(meta.clone(), &source).await?.deduplicated);
    assert_eq!(h.saved()?.len(), 1);
    assert_eq!(h.actions.images.lock().unwrap().len(), 1);
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.actions.notices.lock().unwrap().is_empty() {
            h.actions.changed.notified().await;
        }
    })
    .await?;
    assert_eq!(h.actions.notices.lock().unwrap().len(), 1);
    let text = Clipboard {
        sensitive: false,
        hlc: None,
        source_device: None,
        transfer_id: Id::from_bytes(rand::random()),
        text: "hola ☃".into(),
        created_at: 0,
        origin: pneu_proto::ClipboardOrigin::Clipboard,
    };
    assert!(!c.send_text(text.clone()).await?.deduplicated);
    assert!(c.send_text(text.clone()).await?.deduplicated);
    assert_eq!(&*h.actions.text.lock().unwrap(), &["hola ☃"]);
    let mut conflict = meta;
    conflict.file_name = "other.png".into();
    begin_chunks(&mut c, &conflict, bytes).await?;
    assert!(matches!(
        end(&mut c, conflict.transfer_id).await?,
        Message::Nack(Nack {
            code: NackCode::IdConflict,
            retryable: false,
            ..
        })
    ));
    let conflict = metadata(text.transfer_id, bytes);
    begin_chunks(&mut c, &conflict, bytes).await?;
    assert!(matches!(
        end(&mut c, conflict.transfer_id).await?,
        Message::Nack(Nack {
            code: NackCode::IdConflict,
            ..
        })
    ));
    drop(c);
    h.stop().await
}
#[tokio::test]
async fn mid_transfer_disconnect_replays_from_zero() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let bytes = vec![77; CHUNK_SIZE + 17];
    let source = h.dir.path().join("recording.bin");
    std::fs::write(&source, &bytes)?;
    let mut meta = metadata(Id::from_bytes(rand::random()), &bytes);
    meta.file_name = "recording.bin".into();
    meta.mime = "application/octet-stream".into();
    meta.kind = FileKind::Recording;
    c.write(&Message::FileBegin(meta.clone())).await?;
    c.write(&Message::FileChunk(FileChunk {
        transfer_id: meta.transfer_id.clone(),
        offset: 0,
        data: bytes[..1024].to_vec(),
    }))
    .await?;
    drop(c);
    let mut c = phone.reconnect().await?;
    assert!(!c.send_file(meta.clone(), &source).await?.deduplicated);
    let paths = h.saved()?;
    assert_eq!(paths.len(), 1);
    assert_eq!(std::fs::read(&paths[0])?, bytes);
    drop(c);
    h.stop().await
}
#[tokio::test]
async fn unknown_cert_is_rejected_and_used_token_cannot_pair_again() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    // No HELLO is sent: rejection must follow the completed TLS handshake itself.
    let mut tls = raw_connect(&h, &phone).await?;
    assert_admission_rejection(&mut tls).await?;
    let error = phone
        .connect(
            std::slice::from_ref(&h.address),
            h.core.identity.fingerprint.clone(),
            None,
        )
        .await
        .err()
        .expect("unknown phone rejected");
    assert_simulator_rejection(&error);
    let invitation = h
        .core
        .start_pairing_with_addresses(vec![h.address.clone()])
        .await?;
    let c = phone.pair(&invitation).await?;
    let stranger = h.phone("stranger")?;
    assert!(stranger.pair(&invitation).await.is_err());
    drop(c);
    h.stop().await
}
#[tokio::test]
async fn forgotten_offline_phone_receives_unauthorized_before_hello() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut events = h.core.subscribe();
    let mut connection = h.pair(&phone).await?;
    connection.close().await?;
    drop(connection);
    event_matching(&mut events, |event| {
        matches!(
            event,
            pneud::Event::ConnectionChanged {
                connected: false,
                ..
            }
        )
    })
    .await?;
    // A paired reconnect still exchanges HELLO and carries application data.
    let mut connection = phone.reconnect().await?;
    connection
        .send_text(phone.text("still paired".into(), ClipboardOrigin::Clipboard)?)
        .await?;
    connection.close().await?;
    drop(connection);
    event_matching(&mut events, |event| {
        matches!(
            event,
            pneud::Event::ConnectionChanged {
                connected: false,
                ..
            }
        )
    })
    .await?;
    h.core
        .unpair_device(phone.identity.device_id.clone())
        .await?;
    assert!(
        phone.directory.join("binding.json").exists(),
        "offline phone retains its pin"
    );
    // Several retries must each receive the exact authenticated frame, including
    // when the phone does not send HELLO, and without opening a pairing window.
    for _ in 0..3 {
        let mut tls = raw_connect(&h, &phone).await?;
        assert_admission_rejection(&mut tls).await?;
        let error = phone
            .reconnect()
            .await
            .err()
            .expect("forgotten phone rejected");
        assert_simulator_rejection(&error);
    }
    h.stop().await
}

#[tokio::test]
async fn invalid_client_certificates_still_fail_tls() -> Result<()> {
    use rustls::pki_types::PrivatePkcs8KeyDer;
    let h = Harness::new().await?;
    // Open pairing to prove rejection is cryptographic, not fingerprint admission.
    h.core
        .start_pairing_with_addresses(vec![h.address.clone()])
        .await?;
    let mut phone = h.phone("invalid")?;
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let valid = rcgen::CertificateParams::new(vec!["pneu.local".into()])?.self_signed(&key)?;
    let mut expired = rcgen::CertificateParams::new(vec!["pneu.local".into()])?;
    expired.not_after = rcgen::date_time_ymd(2000, 1, 1);
    let expired = expired.self_signed(&key)?;
    let mut broken = valid.der().to_vec();
    *broken.last_mut().unwrap() ^= 1;
    let p384 = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)?;
    let wrong_curve =
        rcgen::CertificateParams::new(vec!["pneu.local".into()])?.self_signed(&p384)?;
    for (cert, private_key) in [
        (expired.der().clone(), key.serialize_der()),
        (broken.into(), key.serialize_der()),
        (wrong_curve.der().clone(), p384.serialize_der()),
    ] {
        phone.identity.cert = cert;
        phone.identity.key = PrivatePkcs8KeyDer::from(private_key).into();
        // TLS 1.3 clients can finish their half before consuming the server alert.
        // If so, the next read must fail TLS, never yield a protocol ERROR.
        if let Ok(mut tls) = raw_connect(&h, &phone).await {
            assert!(
                tokio::time::timeout(Duration::from_secs(2), read_frame(&mut tls))
                    .await?
                    .is_err()
            );
        }
    }
    // Failed certificate checks must not consume the provisional admission slot.
    let valid_phone = h.phone("valid")?;
    let invitation = h
        .core
        .start_pairing_with_addresses(vec![h.address.clone()])
        .await?;
    drop(valid_phone.pair(&invitation).await?);
    h.stop().await
}

#[tokio::test]
async fn unknown_clients_obey_provisional_and_total_connection_quotas() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("paired")?;
    let mut events = h.core.subscribe();
    let mut connection = h.pair(&phone).await?;
    connection.close().await?;
    drop(connection);
    event_matching(&mut events, |event| {
        matches!(
            event,
            pneud::Event::ConnectionChanged {
                connected: false,
                ..
            }
        )
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(1050)).await;
    h.core
        .start_pairing_with_addresses(vec![h.address.clone()])
        .await?;
    let candidate = h.phone("candidate")?;
    let mut provisional = raw_connect(&h, &candidate).await?;
    // Complete HELLO to ensure the provisional slot is held while waiting for token.
    write_frame(
        &mut provisional,
        &Message::Hello(Hello {
            setup: None,
            role: None,
            addresses: None,
            version: 1,
            device_id: candidate.identity.device_id.clone(),
            name: "candidate".into(),
            capabilities: vec![],
            paused: None,
        }),
    )
    .await?;
    assert!(matches!(
        read_frame(&mut provisional).await?,
        Some(Message::Hello(_))
    ));
    let stranger = h.phone("stranger")?;
    let mut tls = raw_connect(&h, &stranger).await?;
    assert_admission_rejection(&mut tls).await?; // admission throttle
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let mut tls = raw_connect(&h, &stranger).await?;
    assert_admission_rejection(&mut tls).await?; // still occupied after throttle expires
    // Paired clients bypass the provisional slot, but hold a total connection
    // permit even while waiting for HELLO, as do unfinished TLS handshakes.
    let mut held = Vec::new();
    for _ in 0..14 {
        held.push(raw_connect(&h, &phone).await?);
    }
    // Reserve the last permit with a handshake paused before client authentication.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let config = pneud::tls::client_config(&phone.identity, h.core.identity.fingerprint.clone())?;
    let mut client = rustls::ClientConnection::new(
        Arc::new(config),
        rustls::pki_types::ServerName::try_from("pneu.local")?,
    )?;
    let mut hello = Vec::new();
    client.write_tls(&mut hello)?;
    let mut handshake = tokio::net::TcpStream::connect(&h.address).await?;
    handshake.write_all(&hello).await?;
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), handshake.read(&mut byte)).await??,
        1
    );
    assert!(
        raw_connect(&h, &stranger).await.is_err(),
        "all sixteen connection permits held"
    );
    drop(held);
    drop(handshake);
    drop(provisional);
    h.stop().await
}

#[tokio::test]
async fn known_cert_cannot_claim_another_hello_identity() -> Result<()> {
    let h = Harness::new().await?;
    let mut phone = h.phone("phone")?;
    drop(h.pair(&phone).await?);
    phone.identity.device_id = Id::from_bytes(rand::random());
    let mut c = phone.reconnect().await?;
    assert!(matches!(
        c.read(Duration::from_secs(2)).await?,
        Message::Error(ErrorMessage {
            code: ErrorCode::Unauthorized,
            ..
        })
    ));
    drop(c);
    h.stop().await
}
#[tokio::test]
async fn digest_and_short_file_nacks_drain_without_a_visible_file() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let meta = metadata(Id::from_bytes(rand::random()), b"correct");
    begin_chunks(&mut c, &meta, b"badbyte").await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id.clone()).await?,
        Message::Nack(Nack {
            code: NackCode::HashMismatch,
            ..
        })
    ));
    begin_chunks(&mut c, &meta, b"short").await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id.clone()).await?,
        Message::Nack(Nack {
            code: NackCode::SizeMismatch,
            ..
        })
    ));
    assert_eq!(h.saved()?.len(), 0);
    begin_chunks(&mut c, &meta, b"correct").await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    drop(c);
    h.stop().await
}
#[tokio::test]
async fn outbound_clipboard_keeps_latest_unsent_and_acknowledges_inflight() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let a = h
        .core
        .send_clipboard(phone.identity.device_id.clone(), "first".into())
        .await?;
    let inflight = c.read(Duration::from_secs(3)).await?;
    assert!(matches!(&inflight, Message::Clipboard(t) if t.transfer_id==a));
    h.core
        .send_clipboard(phone.identity.device_id.clone(), "superseded".into())
        .await?;
    let latest = h
        .core
        .send_clipboard(phone.identity.device_id.clone(), "latest".into())
        .await?;
    c.handle(inflight).await?;
    let message = c.read(Duration::from_secs(3)).await?;
    assert!(matches!(&message, Message::Clipboard(t) if t.transfer_id==latest && t.text=="latest"));
    c.handle(message).await?;
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn unpair_closes_session_and_removes_trust() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let mut events = h.core.subscribe();
    h.core
        .unpair_device(phone.identity.device_id.clone())
        .await?;
    assert!(matches!(
        c.read(Duration::from_secs(3)).await?,
        Message::Unpair(_)
    ));
    assert!(matches!(
        c.read(Duration::from_secs(3)).await?,
        Message::Error(ErrorMessage {
            code: ErrorCode::Closing,
            ..
        })
    ));
    event_matching(&mut events, |e| matches!(e, pneud::Event::Unpaired { .. })).await?;
    assert!(phone.reconnect().await.is_err());
    assert!(c.handle(Message::Unpair(Unpair {})).await.is_err());
    assert!(!phone.directory.join("binding.json").exists());
    for table in [
        "peers",
        "receipts",
        "outgoing",
        "legacy_clipboard_clock",
        "clipboard_recent",
    ] {
        assert_eq!(device_rows(&h, &phone.identity.device_id, table)?, 0);
    }
    assert!(h.core.paired_devices().is_empty());
    drop(c);
    h.stop().await
}
#[tokio::test]
async fn empty_file_and_capacity_policy() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let empty = metadata(Id::from_bytes(rand::random()), b"");
    begin_chunks(&mut c, &empty, b"").await?;
    assert!(matches!(
        end(&mut c, empty.transfer_id).await?,
        Message::Ack(_)
    ));
    let mut config = h.core.config();
    config.max_file_bytes = 0;
    h.core.update_config(config).await?;
    let meta = metadata(Id::from_bytes(rand::random()), b"oversize");
    begin_chunks(&mut c, &meta, b"oversize").await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Nack(Nack {
            code: NackCode::StorageFailed,
            retryable: true,
            ..
        })
    ));
    drop(c);
    h.stop().await
}
#[tokio::test]
async fn missing_certificate_verify_private_key_is_rejected() -> Result<()> {
    use rustls::{
        SignatureScheme, client::ResolvesClientCert, pki_types::ServerName, sign::CertifiedKey,
    };
    #[derive(Debug)]
    struct WrongSigner(Arc<CertifiedKey>);
    impl ResolvesClientCert for WrongSigner {
        fn resolve(&self, _: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
        fn has_certs(&self) -> bool {
            true
        }
    }
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    drop(h.pair(&phone).await?);
    let wrong = h.phone("wrong-key")?;
    let key = rustls::crypto::ring::sign::any_supported_type(&wrong.identity.key)?;
    let mut config =
        pneud::tls::client_config(&phone.identity, h.core.identity.fingerprint.clone())?;
    config.client_auth_cert_resolver = Arc::new(WrongSigner(Arc::new(CertifiedKey::new(
        vec![phone.identity.cert.clone()],
        key,
    ))));
    let tcp = tokio::net::TcpStream::connect(&h.address).await?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let rejected = match connector
        .connect(ServerName::try_from("pneu.local")?, tcp)
        .await
    {
        Err(_) => true,
        Ok(mut stream) => {
            let hello = Message::Hello(Hello {
                setup: None,
                role: None,
                addresses: None,
                version: 1,
                device_id: phone.identity.device_id.clone(),
                name: "spoof".into(),
                capabilities: vec![],
                paused: None,
            });
            write_frame(&mut stream, &hello).await.is_err()
                || !matches!(
                    tokio::time::timeout(Duration::from_secs(2), read_frame(&mut stream)).await,
                    Ok(Ok(Some(Message::Hello(_))))
                )
        }
    };
    assert!(
        rejected,
        "CertificateVerify must prove possession of the certificate key"
    );
    h.stop().await
}

#[tokio::test]
async fn cache_mode_and_restart_required_port_change() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let original_port = h.core.bound_port();
    let mut changed = h.core.config();
    changed.port = if original_port == u16::MAX {
        original_port - 1
    } else {
        original_port + 1
    };
    assert_eq!(
        h.core.update_config(changed).await.unwrap_err().to_string(),
        "changing the port takes effect after restarting pneud"
    );
    assert_eq!(h.core.bound_port(), original_port);
    let mut config = h.core.config();
    config.save_to_folder = false;
    config.copy_image_to_clipboard = false;
    h.core.update_config(config).await?;
    let bytes = b"cache item";
    let meta = metadata(Id::from_bytes(rand::random()), bytes);
    begin_chunks(&mut c, &meta, bytes).await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id.clone()).await?,
        Message::Ack(_)
    ));
    let files = std::fs::read_dir(&h.core.paths.cache)?
        .filter(|e| e.as_ref().is_ok_and(|e| e.path().is_file()))
        .collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(files.len(), 1);
    assert!(
        files[0]
            .file_name()
            .to_string_lossy()
            .starts_with(".pneu-cache-")
    );
    assert_eq!(std::fs::read(files[0].path())?, bytes);
    // Other settings and ordinary connection replacement still work on the
    // original endpoint; changing it requires a restart.
    let mut replacement = phone.reconnect().await?;
    assert!(matches!(
        c.read(Duration::from_secs(3)).await?,
        Message::Error(ErrorMessage {
            code: ErrorCode::ConnectionReplaced,
            ..
        })
    ));
    begin_chunks(&mut replacement, &meta, bytes).await?;
    assert!(matches!(
        end(&mut replacement, meta.transfer_id).await?,
        Message::Ack(Ack {
            deduplicated: true,
            ..
        })
    ));
    drop(c);
    drop(replacement);
    h.stop().await
}

#[tokio::test]
async fn denied_clipboard_keeps_nothing_and_newer_text_wins_over_stale_replays() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    h.actions
        .denied
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut first = phone.text("first".into(), ClipboardOrigin::Clipboard)?;
    first.created_at = 1;
    let mut second = phone.text("second".into(), ClipboardOrigin::Clipboard)?;
    second.created_at = 2;
    assert!(c.send_text(first.clone()).await.is_err());
    assert_eq!(
        device_rows(&h, &phone.identity.device_id, "clipboard_recent")?,
        0
    );
    h.actions
        .denied
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(!c.send_text(second.clone()).await?.deduplicated);
    assert!(c.send_text(first).await?.deduplicated);
    assert!(c.send_text(second).await?.deduplicated);
    assert_eq!(&*h.actions.text.lock().unwrap(), &["second"]);
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn mime_hint_does_not_automatically_launch_executable_filename() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let mut config = h.core.config();
    config.open_in_viewer = true;
    h.core.update_config(config).await?;
    let bytes = b"\x89PNG\r\n\x1a\n[Desktop Entry]";
    let mut meta = metadata(Id::from_bytes(rand::random()), bytes);
    meta.file_name = "untrusted.desktop".into();
    begin_chunks(&mut c, &meta, bytes).await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.actions.notices.lock().unwrap().is_empty() {
            h.actions.changed.notified().await;
        }
    })
    .await?;
    assert_eq!(h.actions.images.lock().unwrap().len(), 1);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(h.actions.opens.lock().unwrap().is_empty());
    drop(c);
    h.stop().await
}
#[tokio::test]
async fn paired_reconnect_survives_lost_pair_result_and_daemon_restart() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let invitation = h
        .core
        .start_pairing_with_addresses(vec![h.address.clone()])
        .await?;
    let mut c = phone
        .connect(&invitation.addresses, invitation.fingerprint.clone(), None)
        .await?;
    c.write(&Message::PairRequest(PairRequest {
        token: invitation.token,
    }))
    .await?;
    drop(c);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !h.core.paired_devices().contains(&phone.identity.device_id) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let mut c = phone
        .connect(&invitation.addresses, invitation.fingerprint.clone(), None)
        .await?;
    let text = Clipboard {
        sensitive: false,
        hlc: None,
        source_device: None,
        transfer_id: Id::from_bytes(rand::random()),
        text: "recovered".into(),
        created_at: 0,
        origin: pneu_proto::ClipboardOrigin::Clipboard,
    };
    assert!(!c.send_text(text.clone()).await?.deduplicated);
    drop(c);
    let paths = h.core.paths.clone();
    let config = h.core.config();
    let actions = h.actions.clone();
    h.core.request_shutdown();
    h.task.await??;
    drop(h.core);
    let core = Core::new(paths, config, actions.clone()).await?;
    let listener = TcpListener::from_std(h.listener.try_clone()?)?;
    let task = tokio::spawn(core.clone().serve(listener, false));
    let mut c = phone
        .connect(&invitation.addresses, invitation.fingerprint, None)
        .await?;
    assert!(c.send_text(text).await?.deduplicated);
    assert_eq!(&*actions.text.lock().unwrap(), &["recovered"]);
    drop(c);
    core.request_shutdown();
    task.await??;
    Ok(())
}

#[tokio::test]
async fn replacement_closes_old_socket_even_during_slow_desktop_action() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut old = h.pair(&phone).await?;
    h.actions
        .block_image
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let bytes = b"\x89PNG\r\n\x1a\nimage bytes";
    let meta = metadata(Id::from_bytes(rand::random()), bytes);
    begin_chunks(&mut old, &meta, bytes).await?;
    assert!(matches!(
        end(&mut old, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    tokio::time::timeout(Duration::from_secs(3), h.actions.image_entered.notified()).await?;
    let mut replacement = phone.reconnect().await?;
    assert!(matches!(
        old.read(Duration::from_secs(3)).await?,
        Message::Error(ErrorMessage {
            code: ErrorCode::ConnectionReplaced,
            ..
        })
    ));
    let next = metadata(Id::from_bytes(rand::random()), b"next");
    begin_chunks(&mut replacement, &next, b"next").await?;
    assert!(matches!(
        end(&mut replacement, next.transfer_id).await?,
        Message::Ack(_)
    ));
    let nonce = Nonce::new("1234567890abcdef")?;
    replacement
        .write(&Message::Ping(Ping {
            nonce: nonce.clone(),
        }))
        .await?;
    assert!(
        matches!(replacement.read(Duration::from_secs(10)).await?,Message::Pong(p) if p.nonce==nonce)
    );
    h.actions
        .block_image
        .store(false, std::sync::atomic::Ordering::SeqCst);
    h.actions.image_release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), h.actions.image_finished.notified()).await?;
    assert!(
        h.actions.images.lock().unwrap().is_empty(),
        "replaced legacy link retained authority after clipboard preparation"
    );
    let final_connection = phone.reconnect().await?;
    drop(final_connection);
    drop(replacement);
    drop(old);
    h.stop().await
}

async fn event_matching(
    events: &mut tokio::sync::broadcast::Receiver<pneud::Event>,
    matches: impl Fn(&pneud::Event) -> bool,
) -> Result<pneud::Event> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await?;
            if matches(&event) {
                return Ok::<_, anyhow::Error>(event);
            }
        }
    })
    .await?
}
fn device_rows(h: &Harness, device: &Id, table: &str) -> Result<i64> {
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    let key = if table == "peers" { "id" } else { "device" };
    Ok(db.query_row(
        &format!("SELECT count(*) FROM {table} WHERE {key}=?"),
        [device.as_str()],
        |r| r.get(0),
    )?)
}

#[tokio::test]
async fn hello_and_status_report_pause_without_receipts_and_keep_manual_sends() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    phone.set_paused(true)?;
    let mut events = h.core.subscribe();
    let mut c = h.pair(&phone).await?;
    assert_eq!(c.pc.paused, None, "PC HELLO omits phone-only state");
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::PhonePaused { paused: true, .. })
    })
    .await?;
    assert_eq!(h.core.phone_paused(&phone.identity.device_id), Some(true));
    // New PC clipboard operations are dropped while the phone is paused.
    assert!(
        h.core
            .send_clipboard(phone.identity.device_id.clone(), "to phone".into())
            .await
            .is_err()
    );
    let nonce = Nonce::new("0123456789abcdef")?;
    c.write(&Message::Ping(Ping {
        nonce: nonce.clone(),
    }))
    .await?;
    assert!(matches!(c.read(Duration::from_secs(3)).await?, Message::Pong(p) if p.nonce == nonce));
    assert!(c.read(Duration::from_millis(100)).await.is_err());
    let manual = Clipboard {
        sensitive: false,
        hlc: None,
        source_device: None,
        transfer_id: Id::from_bytes(rand::random()),
        text: "manual share while paused".into(),
        created_at: 0,
        origin: ClipboardOrigin::Share,
    };
    assert!(!c.send_text(manual).await?.deduplicated);
    c.send_status(false).await?;
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::PhonePaused { paused: false, .. })
    })
    .await?;
    assert_eq!(h.core.phone_paused(&phone.identity.device_id), Some(false));
    let id = h
        .core
        .send_clipboard(phone.identity.device_id.clone(), "to phone".into())
        .await?;
    let clipboard = c.read(Duration::from_secs(3)).await?;
    assert!(matches!(&clipboard, Message::Clipboard(text) if text.transfer_id == id));
    c.handle(clipboard).await?;
    // STATUS itself has no receipt, and paused state is sent again in HELLO.
    c.send_status(true).await?;
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::PhonePaused { paused: true, .. })
    })
    .await?;
    drop(c);
    let mut c = phone.reconnect().await?;
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::PhonePaused { paused: true, .. })
    })
    .await?;
    assert!(c.read(Duration::from_millis(100)).await.is_err());
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn omitted_hello_paused_defaults_to_false() -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    drop(h.pair(&phone).await?);
    let config = pneud::tls::client_config(&phone.identity, h.core.identity.fingerprint.clone())?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(&h.address).await?;
    let mut tls = connector
        .connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)
        .await?;
    let mut events = h.core.subscribe();
    write_frame(
        &mut tls,
        &Message::Hello(Hello {
            setup: None,
            role: None,
            addresses: None,
            version: 1,
            device_id: phone.identity.device_id.clone(),
            name: "Phone".into(),
            capabilities: vec!["clipboard_text".into()],
            paused: None,
        }),
    )
    .await?;
    assert!(matches!(
        read_frame(&mut tls).await?,
        Some(Message::Hello(_))
    ));
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::PhonePaused { paused: false, .. })
    })
    .await?;
    assert_eq!(h.core.phone_paused(&phone.identity.device_id), Some(false));
    tls.shutdown().await?;
    drop(tls);
    h.stop().await
}

#[tokio::test]
async fn file_kind_and_clipboard_origin_survive_receipts_dedupe_and_events() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let mut events = h.core.subscribe();
    let source = h.dir.path().join("shared.txt");
    std::fs::write(&source, b"shared file")?;
    let meta = phone
        .file_metadata(&source, FileKind::File, "text/plain".into())
        .await?;
    assert!(!c.send_file(meta.clone(), &source).await?.deduplicated);
    event_matching(&mut events, |e| matches!(e, pneud::Event::TransferStarted { metadata, .. } if metadata.kind == FileKind::File)).await?;
    assert!(c.send_file(meta, &source).await?.deduplicated);
    for origin in [ClipboardOrigin::Clipboard, ClipboardOrigin::Share] {
        let mut text = phone.text("a link".into(), origin)?;
        text.created_at = if origin == ClipboardOrigin::Clipboard {
            1
        } else {
            2
        };
        assert_eq!(
            c.send_text(text.clone()).await?.deduplicated,
            origin == ClipboardOrigin::Share
        );
        event_matching(&mut events, |e| matches!(e, pneud::Event::ClipboardReceived { origin: actual, .. } if *actual == origin)).await?;
        assert!(c.send_text(text.clone()).await?.deduplicated);
        let mut conflicting = text;
        conflicting.origin = if origin == ClipboardOrigin::Share {
            ClipboardOrigin::Clipboard
        } else {
            ClipboardOrigin::Share
        };
        c.write(&Message::Clipboard(conflicting)).await?;
        assert!(matches!(
            c.read(Duration::from_secs(3)).await?,
            Message::Nack(Nack {
                code: NackCode::IdConflict,
                ..
            })
        ));
    }
    assert_eq!(h.actions.text.lock().unwrap().len(), 1);
    // Omission and explicit clipboard have the same semantic default for dedupe.
    let text = Clipboard {
        sensitive: false,
        hlc: None,
        source_device: None,
        transfer_id: Id::from_bytes(rand::random()),
        text: "default".into(),
        created_at: 3,
        origin: ClipboardOrigin::Clipboard,
    };
    // Sending an omitted origin is covered by raw vectors; the decoded value is
    // identical to explicit origin and serialized durable metadata uses that value.
    assert!(!c.send_text(text.clone()).await?.deduplicated);
    assert!(c.send_text(text).await?.deduplicated);
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn incoming_unpair_clears_pairing_receipts_and_active_staging() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let device = &phone.identity.device_id;
    let text = Clipboard {
        sensitive: false,
        hlc: None,
        source_device: None,
        transfer_id: Id::from_bytes(rand::random()),
        text: "received".into(),
        created_at: 0,
        origin: ClipboardOrigin::Share,
    };
    c.send_text(text).await?;
    let complete = metadata(Id::from_bytes(rand::random()), b"saved");
    begin_chunks(&mut c, &complete, b"saved").await?;
    assert!(matches!(
        end(&mut c, complete.transfer_id).await?,
        Message::Ack(_)
    ));
    assert_eq!(device_rows(&h, device, "receipts")?, 1);
    assert_eq!(device_rows(&h, device, "clipboard_recent")?, 1);
    h.core
        .send_clipboard(device.clone(), "pending".into())
        .await?;
    assert_eq!(device_rows(&h, device, "outgoing")?, 0);
    phone.set_paused(true)?;
    let mut events = h.core.subscribe();
    c.send_status(true).await?;
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::PhonePaused { paused: true, .. })
    })
    .await?;
    let partial = metadata(Id::from_bytes(rand::random()), b"unfinished");
    begin_chunks(&mut c, &partial, b"part").await?;
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::TransferProgress { .. })
    })
    .await?;
    c.unpair().await?;
    event_matching(&mut events, |e| matches!(e, pneud::Event::Unpaired { .. })).await?;
    event_matching(&mut events, |e| {
        matches!(
            e,
            pneud::Event::ConnectionChanged {
                connected: false,
                ..
            }
        )
    })
    .await?;
    assert!(!h.core.ui_snapshot().receiving);
    assert!(!h.core.ui_snapshot().paired);
    for table in [
        "peers",
        "receipts",
        "outgoing",
        "legacy_clipboard_clock",
        "clipboard_recent",
    ] {
        assert_eq!(device_rows(&h, device, table)?, 0);
    }
    assert_eq!(h.saved()?.len(), 1, "completed user files survive unpair");
    assert!(h.core.paired_devices().is_empty());
    assert_eq!(h.core.phone_paused(device), None);
    assert!(!phone.directory.join("binding.json").exists());
    assert!(phone.reconnect().await.is_err());
    drop(c);
    // Fresh approval may re-pair the same simulator identity; old dedupe is gone.
    // Respect the existing one-second provisional-admission rate limit.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut c = h.pair(&phone).await?;
    let text = Clipboard {
        sensitive: false,
        hlc: None,
        source_device: None,
        transfer_id: partial.transfer_id,
        text: "fresh".into(),
        created_at: 0,
        origin: ClipboardOrigin::Clipboard,
    };
    assert!(!c.send_text(text).await?.deduplicated);
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn missing_uncreatable_save_folder_retains_cache_and_retries_without_receipt() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let blocker = h.dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"keep me")?;
    let mut config = h.core.config();
    config.save_path = blocker.join("missing");
    h.core.update_config(config).await?;
    let meta = metadata(Id::from_bytes(rand::random()), b"retry bytes");
    begin_chunks(&mut c, &meta, b"retry bytes").await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id.clone()).await?,
        Message::Nack(Nack {
            code: NackCode::NotWritable,
            retryable: true,
            ..
        })
    ));
    assert_eq!(device_rows(&h, &phone.identity.device_id, "receipts")?, 0);
    let cached: Vec<_> = std::fs::read_dir(&h.core.paths.cache)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .collect();
    assert_eq!(cached.len(), 1);
    assert_eq!(std::fs::read(cached[0].path())?, b"retry bytes");
    for _ in 0..3 {
        begin_chunks(&mut c, &meta, b"retry bytes").await?;
        assert!(matches!(
            end(&mut c, meta.transfer_id.clone()).await?,
            Message::Nack(Nack {
                code: NackCode::NotWritable,
                ..
            })
        ));
        let copies = std::fs::read_dir(&h.core.paths.cache)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .count();
        assert_eq!(
            copies, 1,
            "retries must retain only one cache copy per operation"
        );
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        h.actions.notices.lock().unwrap().len(),
        1,
        "cached retries must not repeat desktop effects"
    );
    assert!(matches!(
        h.core.ui_snapshot().error,
        Some(pneud::ui::ReceiveError::Storage {
            destination: pneud::ui::ReceiveDestination::SaveFolder,
            ..
        })
    ));
    assert_eq!(std::fs::read(&blocker)?, b"keep me");
    let conflicting_text = Clipboard {
        sensitive: false,
        hlc: None,
        source_device: None,
        transfer_id: meta.transfer_id.clone(),
        text: "cannot replace failed file".into(),
        created_at: 1,
        origin: ClipboardOrigin::Clipboard,
    };
    c.write(&Message::Clipboard(conflicting_text)).await?;
    assert!(matches!(
        c.read(Duration::from_secs(2)).await?,
        Message::Nack(Nack {
            code: NackCode::IdConflict,
            ..
        })
    ));
    assert!(h.actions.text.lock().unwrap().is_empty());
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='stages'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    let mut config = h.core.config();
    config.save_path = h.dir.path().join("new-folder");
    h.core.update_config(config).await?;
    begin_chunks(&mut c, &meta, b"retry bytes").await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(Ack {
            deduplicated: false,
            ..
        })
    ));
    assert_eq!(h.saved()?.len(), 1);
    assert!(
        !cached[0].path().exists(),
        "successful receipt must retire the failed cache copy"
    );
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn actual_pair_api_recovers_lost_result_after_expiry_and_restart() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let invitation = h
        .core
        .start_pairing_with_addresses(vec![h.address.clone()])
        .await?;
    let mut events = h.core.subscribe();
    // Cancel the actual pairing API after persistence, before it can consume
    // PAIR_RESULT. The biased event branch makes this deterministic.
    tokio::select! {biased;
        result=event_matching(&mut events,|e|matches!(e,pneud::Event::Paired {..}))=>{result?;},
        result=phone.pair(&invitation)=>{result?;anyhow::bail!("pair result unexpectedly consumed");}
    }
    assert!(!phone.directory.join("binding.json").exists());
    let mut pending: PairingUri = serde_json::from_slice(&std::fs::read(
        phone.directory.join("pending-pairing.json"),
    )?)?;
    pending.expires_at = 0;
    pending.token = Hash::new("f".repeat(64))?;
    pneud::config::atomic_private(
        &phone.directory.join("pending-pairing.json"),
        &serde_json::to_vec(&pending)?,
    )?;
    let paths = h.core.paths.clone();
    let config = h.core.config();
    let actions = h.actions.clone();
    h.core.request_shutdown();
    h.task.await??;
    drop(h.core);
    let core = Core::new(paths, config, actions).await?;
    let listener = TcpListener::from_std(h.listener.try_clone()?)?;
    let task = tokio::spawn(core.clone().serve(listener, false));
    let mut c = phone.reconnect().await?;
    assert!(phone.directory.join("binding.json").exists());
    assert!(!phone.directory.join("pending-pairing.json").exists());
    let binding: pneud::client::Binding =
        serde_json::from_slice(&std::fs::read(phone.directory.join("binding.json"))?)?;
    assert_eq!(binding.device_id, core.identity.device_id);
    assert_eq!(binding.fingerprint, core.identity.fingerprint);
    c.send_text(phone.text("recovered API".into(), ClipboardOrigin::Share)?)
        .await?;
    c.write(&Message::PairRequest(PairRequest {
        token: pending.token,
    }))
    .await?;
    assert!(matches!(
        c.read(Duration::from_secs(2)).await?,
        Message::Error(ErrorMessage {
            code: ErrorCode::UnexpectedMessage,
            ..
        })
    ));
    drop(c);
    core.request_shutdown();
    task.await??;
    Ok(())
}

#[tokio::test]
async fn ping_is_prompt_during_an_unstalled_multichunk_transfer() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let bytes = vec![0x5a; 4 * 1024 * 1024];
    let meta = metadata(Id::from_bytes(rand::random()), &bytes);
    c.write(&Message::FileBegin(meta.clone())).await?;
    for (index, data) in bytes.chunks(CHUNK_SIZE).enumerate() {
        c.write(&Message::FileChunk(FileChunk {
            transfer_id: meta.transfer_id.clone(),
            offset: (index * CHUNK_SIZE) as u64,
            data: data.to_vec(),
        }))
        .await?;
        // Check several PINGs while the file is still in progress, rather than
        // after END when the file worker has already finished.
        if index % 4 == 0 {
            let nonce = Nonce::new(format!("{index:016x}"))?;
            c.write(&Message::Ping(Ping {
                nonce: nonce.clone(),
            }))
            .await?;
            assert!(
                matches!(c.read(Duration::from_secs(1)).await?, Message::Pong(p) if p.nonce == nonce)
            );
        }
    }
    assert!(matches!(end(&mut c, meta.transfer_id.clone()).await?,
        Message::Ack(Ack { transfer_id, deduplicated: false }) if transfer_id == meta.transfer_id));
    let saved = h.saved()?;
    assert_eq!(saved.len(), 1);
    assert_eq!(tokio::fs::read(&saved[0]).await?, bytes);
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn explicit_image_overflow_reports_failure_without_coalescing_or_blocking_delivery()
-> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let mut events = h.core.subscribe();
    h.actions
        .block_image
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let bytes = b"\x89PNG\r\n\x1a\nimage".to_vec();
    for index in 0..10 {
        let bytes = [bytes.as_slice(), format!("0-{index}").as_bytes()].concat();
        let mut meta = metadata(Id::from_bytes(rand::random()), &bytes);
        meta.kind = FileKind::ClipboardImage;
        meta.file_name = format!("explicit-{index}.png");
        begin_chunks(&mut c, &meta, &bytes).await?;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), end(&mut c, meta.transfer_id)).await??,
            Message::Ack(_)
        ));
        if index == 0 {
            tokio::time::timeout(Duration::from_secs(2), h.actions.image_entered.notified())
                .await?;
        }
    }
    event_matching(&mut events, |e| matches!(e, pneud::Event::Error {message, ..} if message.contains("explicit image queue is full"))).await?;
    h.actions
        .block_image
        .store(false, std::sync::atomic::Ordering::SeqCst);
    h.actions.image_release.notify_one();
    h.wait_images(9).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while h
            .actions
            .copy_failures
            .load(std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            h.actions.changed.notified().await;
        }
    })
    .await?;
    let images = h.actions.images.lock().unwrap().clone();
    assert_eq!(images.len(), 9);
    for (index, image) in images.iter().enumerate() {
        assert!(
            image
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(&format!("explicit-{index}.png"))
        );
    }
    assert_eq!(
        h.actions
            .copy_failures
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn automatic_actions_coalesce_without_backpressuring_files_and_keep_explicit_requests()
-> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let mut config = h.core.config();
    config.open_in_viewer = true;
    h.core.update_config(config).await?;
    h.actions
        .block_image
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // This test holds the first preparation while 70 durable transfers finish.
    // Exercise coalescing independently of the clipboard actor's deadline.
    // A blocking task prevents Tokio's paused clock from auto-advancing during
    // real socket/file I/O. Dropping clock_release also releases it on failure.
    let (clock_release, clock_held) = std::sync::mpsc::channel::<()>();
    let clock_task = tokio::task::spawn_blocking(move || {
        let _ = clock_held.recv();
    });
    tokio::time::pause();
    let bytes = b"\x89PNG\r\n\x1a\nimage".to_vec();
    for index in 0..67 {
        let bytes = [bytes.as_slice(), format!("0-{index}").as_bytes()].concat();
        let mut meta = metadata(Id::from_bytes(rand::random()), &bytes);
        meta.file_name = if index == 66 {
            "latest.png".into()
        } else {
            format!("automatic-{index}.png")
        };
        begin_chunks(&mut c, &meta, &bytes).await?;
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(750), end(&mut c, meta.transfer_id))
                .await??,
            Message::Ack(_)
        ));
        if index == 0 {
            tokio::time::timeout(Duration::from_secs(2), h.actions.image_entered.notified())
                .await?;
        }
    }
    for index in 0..3 {
        let bytes = [bytes.as_slice(), format!("1-{index}").as_bytes()].concat();
        let mut meta = metadata(Id::from_bytes(rand::random()), &bytes);
        meta.kind = FileKind::ClipboardImage;
        meta.file_name = format!("explicit-{index}.png");
        begin_chunks(&mut c, &meta, &bytes).await?;
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(750), end(&mut c, meta.transfer_id))
                .await??,
            Message::Ack(_)
        ));
    }
    assert_eq!(h.saved()?.len(), 67);
    h.actions
        .block_image
        .store(false, std::sync::atomic::Ordering::SeqCst);
    h.actions.image_release.notify_one();
    h.wait_images(5).await?;
    tokio::time::resume();
    drop(clock_release);
    clock_task.await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while h.actions.opens.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let images = h.actions.images.lock().unwrap().clone();
    assert_eq!(
        images.len(),
        5,
        "only running, explicit and latest automatic copies survive"
    );
    assert!(
        images[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("automatic-0.png"),
        "unexpected first copy: {images:?}"
    );
    for index in 0..3 {
        assert!(images.iter().any(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(&format!("explicit-{index}.png"))
        }));
    }
    assert!(
        images
            .last()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("latest.png")
    );
    assert_eq!(
        &*h.actions.opens.lock().unwrap(),
        &[images.last().unwrap().clone()]
    );
    assert!(h.actions.notices.lock().unwrap().len() <= 16);
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn slow_clipboard_does_not_block_file_ping_or_outgoing_receipt() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    // HLC allocation for a *new* local operation shares the OS effect gate.
    // Prepare this operation first, then test its receipt independently while
    // the opposite clipboard write is blocked.
    let id = h
        .core
        .send_clipboard(phone.identity.device_id.clone(), "reverse".into())
        .await?;
    let outgoing = c.read(Duration::from_secs(2)).await?;
    assert!(matches!(&outgoing, Message::Clipboard(t) if t.transfer_id==id));
    let mut events = h.core.subscribe();
    h.actions
        .block_text
        .store(true, std::sync::atomic::Ordering::SeqCst);
    c.write(&Message::Clipboard(
        phone.text("blocked".into(), ClipboardOrigin::Clipboard)?,
    ))
    .await?;
    tokio::time::timeout(Duration::from_secs(2), h.actions.text_entered.notified()).await?;
    let meta = metadata(Id::from_bytes(rand::random()), b"file alongside text");
    begin_chunks(&mut c, &meta, b"file alongside text").await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    c.handle(outgoing).await?;
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::ClipboardSent { .. })
    })
    .await?;
    let nonce = Nonce::new("cafebabecafebabe")?;
    c.write(&Message::Ping(Ping {
        nonce: nonce.clone(),
    }))
    .await?;
    assert!(matches!(c.read(Duration::from_secs(1)).await?,Message::Pong(p) if p.nonce==nonce));
    h.actions
        .block_text
        .store(false, std::sync::atomic::Ordering::SeqCst);
    h.actions.text_release.notify_one();
    assert!(matches!(
        c.read(Duration::from_secs(2)).await?,
        Message::Ack(_)
    ));
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn shutdown_cancels_a_tls_connection_waiting_for_hello() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    drop(h.pair(&phone).await?);
    let config = pneud::tls::client_config(&phone.identity, h.core.identity.fingerprint.clone())?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(&h.address).await?;
    let mut tls = connector
        .connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)
        .await?;
    h.core.request_shutdown();
    let hello = Message::Hello(Hello {
        setup: None,
        role: None,
        addresses: None,
        version: 1,
        device_id: phone.identity.device_id.clone(),
        name: "late".into(),
        capabilities: vec![],
        paused: None,
    });
    let _ = write_frame(&mut tls, &hello).await;
    tokio::time::timeout(Duration::from_secs(2), h.task).await???;
    assert!(h.core.connected_devices().is_empty());
    drop(tls);
    Ok(())
}

#[tokio::test]
async fn changing_save_path_applies_to_new_transfers_without_waiting_for_active_transfer()
-> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let old = h.core.config().save_path;
    let bytes = b"\x89PNG\r\n\x1a\nimage";
    let first = metadata(Id::from_bytes(rand::random()), bytes);
    let mut events = h.core.subscribe();
    begin_chunks(&mut c, &first, bytes).await?;
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::TransferProgress { .. })
    })
    .await?;
    let mut config = h.core.config();
    config.save_path = h.dir.path().join("new-save");
    tokio::time::timeout(Duration::from_millis(500), h.core.update_config(config)).await??;
    assert!(matches!(
        end(&mut c, first.transfer_id).await?,
        Message::Ack(_)
    ));
    assert_eq!(std::fs::read_dir(old.join(".pneu-staging"))?.count(), 0);
    assert_eq!(
        std::fs::read_dir(&old)?
            .filter(|e| e.as_ref().is_ok_and(|e| e.path().is_file()))
            .count(),
        1
    );
    let second = metadata(Id::from_bytes(rand::random()), bytes);
    begin_chunks(&mut c, &second, bytes).await?;
    assert!(matches!(
        end(&mut c, second.transfer_id).await?,
        Message::Ack(_)
    ));
    assert!(h.core.config().save_path.join(".pneu-staging").is_dir());
    assert_eq!(h.saved()?.len(), 1);
    assert_eq!(
        h.core.connected_devices(),
        vec![phone.identity.device_id.clone()]
    );
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn image_copy_uses_content_and_only_viewer_launches_require_extensions() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let mut config = h.core.config();
    config.open_in_viewer = true;
    h.core.update_config(config).await?;
    let bytes = b"\x89PNG\r\n\x1a\nimage".to_vec();
    for name in ["clipboard", "clipboard.bin", "clipboard.exe"] {
        let bytes = [bytes.as_slice(), format!("0-{name}").as_bytes()].concat();
        let mut meta = metadata(Id::from_bytes(rand::random()), &bytes);
        meta.kind = FileKind::ClipboardImage;
        meta.file_name = name.into();
        begin_chunks(&mut c, &meta, &bytes).await?;
        assert!(matches!(
            end(&mut c, meta.transfer_id).await?,
            Message::Ack(_)
        ));
    }
    // Ordinary image copying has the same content rule, with viewer safety
    // independently checking the extension.
    let mut meta = metadata(Id::from_bytes(rand::random()), &bytes);
    meta.file_name = "screenshot.bin".into();
    begin_chunks(&mut c, &meta, &bytes).await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    h.wait_images(4).await?;
    let invalid = b"executable bytes with an image filename and MIME";
    let meta = metadata(Id::from_bytes(rand::random()), invalid);
    begin_chunks(&mut c, &meta, invalid).await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.actions.images.lock().unwrap().len(), 4);
    assert_eq!(h.actions.clipboard_notices.lock().unwrap().len(), 3);
    assert!(h.actions.opens.lock().unwrap().is_empty());
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn unpair_then_new_pairing_cancels_old_running_and_queued_actions() -> Result<()> {
    let h = Harness::new().await?;
    let old = h.phone("old")?;
    let next = h.phone("next")?;
    let mut c = h.pair(&old).await?;
    let bytes = b"\x89PNG\r\n\x1a\nimage";
    h.actions
        .block_image
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for index in 0..2 {
        let mut meta = metadata(Id::from_bytes(rand::random()), bytes);
        meta.file_name = "old.png".into();
        begin_chunks(&mut c, &meta, bytes).await?;
        assert!(matches!(
            end(&mut c, meta.transfer_id).await?,
            Message::Ack(_)
        ));
        if index == 0 {
            tokio::time::timeout(Duration::from_secs(2), h.actions.image_entered.notified())
                .await?;
        }
    }
    let mut transfer_events = h.core.subscribe();
    let unfinished = metadata(Id::from_bytes(rand::random()), bytes);
    begin_chunks(&mut c, &unfinished, bytes).await?;
    event_matching(&mut transfer_events, |event|matches!(event,pneud::Event::TransferProgress {transfer_id,..} if *transfer_id==unfinished.transfer_id)).await?;
    assert!(h.core.ui_snapshot().receiving);
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.core.unpair_device(old.identity.device_id.clone()).await?;
    let mut replacement = h.pair(&next).await?;
    assert!(
        !h.core.ui_snapshot().receiving,
        "new pairing must discard the old unfinished receive"
    );
    h.actions
        .block_image
        .store(false, std::sync::atomic::Ordering::SeqCst);
    h.actions.image_release.notify_one();
    assert_eq!(
        h.core.paired_devices(),
        vec![next.identity.device_id.clone()]
    );
    assert!(old.reconnect().await.is_err());
    for table in [
        "peers",
        "receipts",
        "legacy_clipboard_clock",
        "clipboard_recent",
        "outgoing",
    ] {
        assert_eq!(device_rows(&h, &old.identity.device_id, table)?, 0);
    }
    let mut meta = metadata(Id::from_bytes(rand::random()), bytes);
    meta.file_name = "new.png".into();
    begin_chunks(&mut replacement, &meta, bytes).await?;
    assert!(matches!(
        end(&mut replacement, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    h.wait_images(1).await?;
    assert!(
        h.actions
            .images
            .lock()
            .unwrap()
            .iter()
            .all(|p| p.to_string_lossy().ends_with("new.png"))
    );
    drop(c);
    drop(replacement);
    h.stop().await
}

#[tokio::test]
async fn immediate_disconnect_after_receipt_preserves_text_link_and_failure_notices() -> Result<()>
{
    for mode in ["text", "link", "copy failure", "cache failure"] {
        let h = Harness::new().await?;
        let phone = h.phone("phone")?;
        let mut c = h.pair(&phone).await?;
        h.actions
            .block_notice
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut events = h.core.subscribe();
        let expected = if mode == "cache failure" {
            std::fs::rename(&h.core.paths.cache, h.dir.path().join("old-cache"))?;
            std::fs::write(&h.core.paths.cache, b"blocked cache")?;
            let bytes = b"image";
            let mut meta = metadata(Id::from_bytes(rand::random()), bytes);
            meta.kind = FileKind::ClipboardImage;
            begin_chunks(&mut c, &meta, bytes).await?;
            assert!(matches!(
                end(&mut c, meta.transfer_id).await?,
                Message::Nack(Nack {
                    code: NackCode::NotWritable,
                    ..
                })
            ));
            "Cache: NotWritable"
        } else {
            let text = if mode == "link" {
                "https://example.com/"
            } else {
                "text"
            };
            let failed = mode == "copy failure";
            h.actions
                .denied
                .store(failed, std::sync::atomic::Ordering::SeqCst);
            c.write(&Message::Clipboard(Clipboard {
                sensitive: false,
                hlc: None,
                source_device: None,
                transfer_id: Id::from_bytes(rand::random()),
                text: text.into(),
                created_at: 0,
                origin: ClipboardOrigin::Clipboard,
            }))
            .await?;
            let receipt = c.read(Duration::from_secs(2)).await?;
            if failed {
                assert!(matches!(
                    receipt,
                    Message::Nack(Nack {
                        code: NackCode::ClipboardUnavailable,
                        ..
                    })
                ));
            } else {
                assert!(matches!(receipt, Message::Ack(_)));
            }
            if failed { "text failed" } else { text }
        };
        // A delayed backend cannot delay the receipt, and is independent of READY.
        tokio::time::timeout(Duration::from_secs(2), h.actions.notice_entered.notified()).await?;
        c.close().await?;
        event_matching(&mut events, |event| {
            matches!(
                event,
                pneud::Event::ConnectionChanged {
                    connected: false,
                    ..
                }
            )
        })
        .await?;
        assert!(h.actions.receipt_notices.lock().unwrap().is_empty());
        h.actions.notice_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.actions.receipt_notices.lock().unwrap().is_empty() {
                h.actions.changed.notified().await;
            }
        })
        .await?;
        assert_eq!(&*h.actions.receipt_notices.lock().unwrap(), &[expected]);
        h.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn copy_failure_without_regular_notifications_keeps_actual_save_outcome() -> Result<()> {
    for (save, bytes) in [
        (true, &b"\x89PNG\r\n\x1a\nimage"[..]),
        (false, &b"\x89PNG\r\n\x1a\nimage"[..]),
        (true, &b"invalid image"[..]),
    ] {
        let h = Harness::new().await?;
        let phone = h.phone("phone")?;
        let mut c = h.pair(&phone).await?;
        let mut config = h.core.config();
        config.save_to_folder = save;
        config.notification_preview = false;
        h.core.update_config(config).await?;
        h.actions
            .deny_image
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut meta = metadata(Id::from_bytes(rand::random()), bytes);
        // Invalid explicit content uses the same saved=false failure API.
        if bytes == b"invalid image" {
            meta.kind = FileKind::ClipboardImage;
        }
        begin_chunks(&mut c, &meta, bytes).await?;
        assert!(matches!(
            end(&mut c, meta.transfer_id).await?,
            Message::Ack(_)
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.actions.failure_saved.lock().unwrap().is_empty() {
                h.actions.changed.notified().await;
            }
        })
        .await?;
        assert_eq!(
            &*h.actions.failure_saved.lock().unwrap(),
            &[save && bytes != b"invalid image"]
        );
        c.close().await?;
        h.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn immediate_disconnect_after_file_ack_preserves_queued_desktop_effects() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let mut config = h.core.config();
    config.open_in_viewer = true;
    h.core.update_config(config).await?;
    h.actions
        .block_image
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let bytes = b"\x89PNG\r\n\x1a\nimage";
    let first = metadata(Id::from_bytes(rand::random()), bytes);
    begin_chunks(&mut c, &first, bytes).await?;
    assert!(matches!(
        end(&mut c, first.transfer_id).await?,
        Message::Ack(_)
    ));
    tokio::time::timeout(Duration::from_secs(2), h.actions.image_entered.notified()).await?;
    let source = h.dir.path().join("latest.png");
    std::fs::write(&source, [bytes.as_slice(), b"latest"].concat())?;
    let meta = phone
        .file_metadata(&source, FileKind::Screenshot, "image/png".into())
        .await?;
    let mut events = h.core.subscribe();
    assert!(!c.send_file(meta, &source).await?.deduplicated);
    // Exactly the simulator's send-file lifecycle: close immediately on ACK.
    c.close().await?;
    event_matching(&mut events, |e| {
        matches!(
            e,
            pneud::Event::ConnectionChanged {
                connected: false,
                ..
            }
        )
    })
    .await?;
    assert!(h.core.connected_devices().is_empty());
    assert!(h.actions.images.lock().unwrap().is_empty());
    h.actions
        .block_image
        .store(false, std::sync::atomic::Ordering::SeqCst);
    h.actions.image_release.notify_one();
    h.wait_images(2).await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while h.actions.notices.lock().unwrap().len() < 2
            || h.actions.opens.lock().unwrap().is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let latest = h
        .saved()?
        .into_iter()
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("latest.png")
        })
        .unwrap();
    assert_eq!(h.actions.images.lock().unwrap().last(), Some(&latest));
    assert!(h.actions.notices.lock().unwrap().contains(&latest));
    assert_eq!(&*h.actions.opens.lock().unwrap(), &[latest]);
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn unpair_from_either_side_cancels_the_pending_viewer_debounce() -> Result<()> {
    for incoming in [false, true] {
        let h = Harness::new().await?;
        let phone = h.phone("phone")?;
        let mut c = h.pair(&phone).await?;
        let mut config = h.core.config();
        config.open_in_viewer = true;
        h.core.update_config(config).await?;
        let bytes = b"\x89PNG\r\n\x1a\nimage";
        let meta = metadata(Id::from_bytes(rand::random()), bytes);
        begin_chunks(&mut c, &meta, bytes).await?;
        assert!(matches!(
            end(&mut c, meta.transfer_id).await?,
            Message::Ack(_)
        ));
        h.wait_images(1).await?;
        if incoming {
            let mut events = h.core.subscribe();
            c.unpair().await?;
            event_matching(&mut events, |e| matches!(e, pneud::Event::Unpaired { .. })).await?;
        } else {
            h.core
                .unpair_device(phone.identity.device_id.clone())
                .await?;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(h.actions.opens.lock().unwrap().is_empty());
        assert_eq!(h.saved()?.len(), 1, "unpair leaves completed user files");
        drop(c);
        h.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn clipboard_images_use_cache_copy_regardless_of_toggles_and_never_open() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let mut config = h.core.config();
    config.copy_image_to_clipboard = false;
    config.open_in_viewer = true;
    h.core.update_config(config).await?;
    let bytes = b"\x89PNG\r\n\x1a\nimage";
    let mut meta = metadata(Id::from_bytes(rand::random()), bytes);
    meta.kind = FileKind::ClipboardImage;
    begin_chunks(&mut c, &meta, bytes).await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id.clone()).await?,
        Message::Ack(_)
    ));
    h.wait_images(1).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.actions.clipboard_notices.lock().unwrap().is_empty() {
            h.actions.changed.notified().await;
        }
    })
    .await?;
    let image = h.actions.images.lock().unwrap()[0].clone();
    assert!(image.starts_with(&h.core.paths.cache));
    assert!(!h.core.config().save_path.exists());
    assert!(h.actions.notices.lock().unwrap().is_empty());
    begin_chunks(&mut c, &meta, bytes).await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(Ack {
            deduplicated: true,
            ..
        })
    ));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(h.actions.opens.lock().unwrap().is_empty());
    assert_eq!(h.actions.images.lock().unwrap().len(), 1);
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn long_image_names_preserve_extension_and_bursts_open_only_latest() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new().await?;
    std::fs::create_dir(&h.core.config().save_path)?;
    std::fs::set_permissions(
        h.core.config().save_path,
        std::fs::Permissions::from_mode(0o775),
    )?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let mut config = h.core.config();
    config.open_in_viewer = true;
    h.core.update_config(config).await?;
    let bytes = b"\x89PNG\r\n\x1a\nimage".to_vec();
    for index in 0..3 {
        let bytes = [bytes.as_slice(), format!("0-{index}").as_bytes()].concat();
        let mut meta = metadata(Id::from_bytes(rand::random()), &bytes);
        meta.file_name = format!("{index}-{}.png", "é".repeat(122));
        begin_chunks(&mut c, &meta, &bytes).await?;
        assert!(matches!(
            end(&mut c, meta.transfer_id).await?,
            Message::Ack(_)
        ));
    }
    h.wait_images(1).await?;
    assert!(h.actions.opens.lock().unwrap().is_empty());
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.actions.opens.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let images = h.actions.images.lock().unwrap().clone();
    let opens = h.actions.opens.lock().unwrap().clone();
    let newest = h
        .core
        .config()
        .save_path
        .join(format!("2-{}.png", "é".repeat(122)));
    assert!(h.saved()?.contains(&newest));
    assert_eq!(opens, vec![newest.clone()]);
    assert_eq!(images.last(), Some(&newest));
    assert!(
        images
            .iter()
            .all(|p| p.extension().is_some_and(|e| e == "png"))
    );
    assert_eq!(
        std::fs::metadata(h.core.config().save_path)?
            .permissions()
            .mode()
            & 0o777,
        0o775
    );
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn simulator_checks_image_capability_and_never_creates_an_outbox() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    c.pc.capabilities.retain(|cap| cap != "clipboard_image");
    let source = h.dir.path().join("image.png");
    std::fs::write(&source, b"\x89PNG\r\n\x1a\nimage")?;
    let meta = phone
        .file_metadata(&source, FileKind::ClipboardImage, "image/png".into())
        .await?;
    assert!(c.send_file(meta, &source).await.is_err());
    c.send_text(phone.text("independent of files".into(), ClipboardOrigin::Share)?)
        .await?;
    assert!(!phone.directory.join("outbox").exists());
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn reenabled_clipboard_sync_wakes_an_idle_reconnected_session() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut events = h.core.subscribe();
    let mut c = h.pair(&phone).await?;
    let id = h
        .core
        .send_clipboard(
            phone.identity.device_id.clone(),
            "queued without receipt".into(),
        )
        .await?;
    let initial = c.read(Duration::from_secs(2)).await?;
    assert!(matches!(initial,Message::Clipboard(t) if t.transfer_id == id));
    let mut config = h.core.config();
    config.clipboard_sync_to_phone = false;
    h.core.update_config(config.clone()).await?;
    c.close().await?;
    drop(c);
    event_matching(&mut events, |e| {
        matches!(
            e,
            pneud::Event::ConnectionChanged {
                connected: false,
                ..
            }
        )
    })
    .await?;
    let mut c = phone.reconnect().await?;
    event_matching(&mut events, |e| {
        matches!(
            e,
            pneud::Event::ConnectionChanged {
                connected: true,
                ..
            }
        )
    })
    .await?;
    config.clipboard_sync_to_phone = true;
    h.core.update_config(config).await?;
    assert!(
        c.read(Duration::from_millis(150)).await.is_err(),
        "disconnected clipboard must not replay"
    );
    let fresh = h
        .core
        .send_clipboard(
            phone.identity.device_id.clone(),
            "new connected share".into(),
        )
        .await?;
    let message = c.read(Duration::from_secs(2)).await?;
    assert!(
        matches!(&message,Message::Clipboard(t) if t.transfer_id == fresh && t.transfer_id != id)
    );
    c.handle(message).await?;
    event_matching(
        &mut events,
        |e| matches!(e,pneud::Event::ClipboardSent {transfer_id,..} if *transfer_id == fresh),
    )
    .await?;
    drop(c);
    h.stop().await
}
#[tokio::test]
async fn heif_and_avif_are_ordinary_files_and_explicit_heic_is_unsupported() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let mut config = h.core.config();
    config.open_in_viewer = true;
    h.core.update_config(config).await?;
    let bytes = b"\0\0\0\x18ftypheic\0\0\0\0heicmif1";
    for (mime, extension) in [
        ("image/heic", "heic"),
        ("image/heif", "heif"),
        ("image/avif", "avif"),
    ] {
        let mut meta = metadata(Id::from_bytes(rand::random()), bytes);
        meta.mime = mime.into();
        meta.file_name = format!("ordinary.{extension}");
        meta.kind = FileKind::Photo;
        begin_chunks(&mut c, &meta, bytes).await?;
        assert!(matches!(
            end(&mut c, meta.transfer_id).await?,
            Message::Ack(_)
        ));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.actions.notices.lock().unwrap().len() < 3 {
            h.actions.changed.notified().await;
        }
    })
    .await?;
    assert_eq!(
        &*h.actions.notice_mimes.lock().unwrap(),
        &["application/octet-stream"; 3]
    );
    let mut events = h.core.subscribe();
    let mut meta = metadata(Id::from_bytes(rand::random()), bytes);
    meta.mime = "image/heic".into();
    meta.kind = FileKind::ClipboardImage;
    meta.file_name = "explicit.heic".into();
    begin_chunks(&mut c, &meta, bytes).await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    event_matching(
        &mut events,
        |e| matches!(e, pneud::Event::Error { message, .. } if message.contains("unsupported")),
    )
    .await?;
    assert!(h.actions.images.lock().unwrap().is_empty());
    assert!(h.actions.clipboard_notices.lock().unwrap().is_empty());
    assert!(h.actions.opens.lock().unwrap().is_empty());
    assert_eq!(h.saved()?.len(), 3);
    assert_eq!(
        std::fs::read_dir(&h.core.paths.cache)?
            .filter(|e| e.as_ref().is_ok_and(|e| e.path().is_file()))
            .count(),
        1
    );
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn denied_clipboard_image_never_reports_copied_success() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    h.actions
        .deny_image
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut events = h.core.subscribe();
    let bytes = b"\x89PNG\r\n\x1a\nimage";
    let mut meta = metadata(Id::from_bytes(rand::random()), bytes);
    meta.kind = FileKind::ClipboardImage;
    begin_chunks(&mut c, &meta, bytes).await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id.clone()).await?,
        Message::Ack(_)
    ));
    event_matching(&mut events,|e|matches!(e,pneud::Event::Error {message,..} if message.contains("image clipboard unavailable"))).await?;
    assert!(h.actions.images.lock().unwrap().is_empty());
    assert!(h.actions.clipboard_notices.lock().unwrap().is_empty());
    assert!(h.actions.notices.lock().unwrap().is_empty());
    begin_chunks(&mut c, &meta, bytes).await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(Ack {
            deduplicated: true,
            ..
        })
    ));
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn failed_clipboard_has_no_barrier_or_receipt_to_conflict_with_interleaved_file() -> Result<()>
{
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    h.actions
        .denied
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let bytes = b"file bytes";
    let meta = metadata(Id::from_bytes(rand::random()), bytes);
    let mut events = h.core.subscribe();
    c.write(&Message::FileBegin(meta.clone())).await?;
    // File and text have separate workers. Establish this test's intended
    // interleaving rather than assuming wire order establishes processing order.
    event_matching(&mut events, |event| matches!(event, pneud::Event::TransferStarted { metadata, .. } if metadata.transfer_id == meta.transfer_id)).await?;
    let mut text = phone.text("failed".into(), ClipboardOrigin::Clipboard)?;
    text.transfer_id = meta.transfer_id.clone();
    assert!(c.send_text(text).await.is_err());
    c.write(&Message::FileChunk(FileChunk {
        transfer_id: meta.transfer_id.clone(),
        offset: 0,
        data: bytes.to_vec(),
    }))
    .await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    h.actions
        .denied
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(
        !c.send_text(phone.text("new".into(), ClipboardOrigin::Clipboard)?)
            .await?
            .deduplicated
    );
    assert_eq!(&*h.actions.text.lock().unwrap(), &["new"]);
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn received_image_descriptor_survives_swap_while_copy_is_blocked() -> Result<()> {
    use std::sync::atomic::Ordering;
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    h.actions.block_image.store(true, Ordering::SeqCst);
    let bytes = b"\x89PNG\r\n\x1a\noriginal image";
    let source = h.dir.path().join("source.png");
    std::fs::write(&source, bytes)?;
    c.send_file(metadata(Id::from_bytes(rand::random()), bytes), &source)
        .await?;
    tokio::time::timeout(Duration::from_secs(2), h.actions.image_entered.notified()).await?;
    let saved = h.saved()?.pop().unwrap();
    std::fs::remove_file(&saved)?;
    let secret = h.dir.path().join("secret");
    std::fs::write(&secret, b"secret text")?;
    std::os::unix::fs::symlink(&secret, &saved)?;
    h.actions.image_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.actions.image_bytes.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(&*h.actions.image_bytes.lock().unwrap(), &[bytes.to_vec()]);
    drop(c);
    h.stop().await
}

#[tokio::test]
async fn text_claim_rejects_interleaved_file_before_placement() -> Result<()> {
    use std::sync::atomic::Ordering;
    for fallback in [false, true] {
        let h = Harness::new().await?;
        if fallback {
            let blocker = h.dir.path().join("blocked-folder");
            std::fs::write(&blocker, b"blocker")?;
            let mut config = h.core.config();
            config.save_path = blocker.join("child");
            h.core.update_config(config).await?;
        }
        let phone = h.phone("phone")?;
        let mut c = h.pair(&phone).await?;
        h.actions.block_text.store(true, Ordering::SeqCst);
        let text = phone.text("claimed text".into(), ClipboardOrigin::Clipboard)?;
        let bytes = b"conflicting file";
        let meta = metadata(text.transfer_id.clone(), bytes);
        let mut events = h.core.subscribe();
        begin_chunks(&mut c, &meta, bytes).await?;
        event_matching(&mut events, |e| matches!(e, pneud::Event::TransferProgress {transfer_id,received,..} if *transfer_id == meta.transfer_id && *received == bytes.len() as u64)).await?;
        // The file has a live stage but no receipt yet. Text claims its ID while
        // blocked inside clipboard integration; FILE_END must conflict at commit.
        c.write(&Message::Clipboard(text.clone())).await?;
        tokio::time::timeout(Duration::from_secs(2), h.actions.text_entered.notified()).await?;
        assert!(matches!(
            end(&mut c, meta.transfer_id).await?,
            Message::Nack(Nack {
                code: NackCode::IdConflict,
                ..
            })
        ));
        assert!(h.actions.text.lock().unwrap().is_empty());
        h.actions.text_release.notify_one();
        assert!(matches!(
            c.read(Duration::from_secs(3)).await?,
            Message::Ack(Ack {
                deduplicated: false,
                ..
            })
        ));
        assert_eq!(&*h.actions.text.lock().unwrap(), &["claimed text"]);
        assert!(h.saved().unwrap_or_default().is_empty());
        drop(c);
        h.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn outgoing_clipboard_images_require_capability_and_keep_latest_while_receipting()
-> Result<()> {
    use tokio::io::AsyncWriteExt;
    async fn hold_file_lane(h: &Harness, phone: &Phone, tls: &mut PhoneTls) -> Result<Id> {
        let id = h
            .core
            .send_clipboard_image(
                phone.identity.device_id.clone(),
                b"\x89PNG\r\n\x1a\nreceipt barrier".to_vec(),
            )
            .await?;
        assert!(
            matches!(read_frame(tls).await?,Some(Message::FileBegin(begin)) if begin.transfer_id==id)
        );
        assert!(
            matches!(read_frame(tls).await?,Some(Message::FileChunk(chunk)) if chunk.transfer_id==id)
        );
        assert!(
            matches!(read_frame(tls).await?,Some(Message::FileEnd(end)) if end.transfer_id==id)
        );
        Ok(id)
    }
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    drop(h.pair(&phone).await?);
    let mut config = h.core.config();
    config.clipboard_include_images = true;
    h.core.update_config(config).await?;
    assert!(
        h.core
            .send_clipboard_image(
                phone.identity.device_id.clone(),
                b"\x89PNG\r\n\x1a\nunsupported".to_vec()
            )
            .await
            .is_err()
    );
    let config = pneud::tls::client_config(&phone.identity, h.core.identity.fingerprint.clone())?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(&h.address).await?;
    let mut tls = connector
        .connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)
        .await?;
    let mut events = h.core.subscribe();
    write_frame(
        &mut tls,
        &Message::Hello(Hello {
            setup: None,
            role: None,
            addresses: None,
            version: 1,
            device_id: phone.identity.device_id.clone(),
            name: "Image phone".into(),
            capabilities: vec![
                "files".into(),
                "clipboard_text".into(),
                "clipboard_image".into(),
            ],
            paused: Some(false),
        }),
    )
    .await?;
    assert!(matches!(
        read_frame(&mut tls).await?,
        Some(Message::Hello(_))
    ));
    event_matching(&mut events, |event| {
        matches!(
            event,
            pneud::Event::ConnectionChanged {
                connected: true,
                ..
            }
        )
    })
    .await?;
    let initial = hold_file_lane(&h, &phone, &mut tls).await?;
    // Clipboard text has its own receipt lane while this image awaits ACK.
    let text = h
        .core
        .send_clipboard(
            phone.identity.device_id.clone(),
            "text during image receipt".into(),
        )
        .await?;
    assert!(
        matches!(read_frame(&mut tls).await?,Some(Message::Clipboard(frame)) if frame.transfer_id==text)
    );
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: text,
            deduplicated: false,
        }),
    )
    .await?;
    let prepared = h.core.observe_clipboard_offer().unwrap();
    h.core
        .send_observed_image(prepared.clone(), b"\x89PNG\r\n\x1a\nprepared".to_vec())
        .await?;
    h.core.observe_clipboard_offer(); // a secret/unsupported/cleared offer invalidates images too
    assert!(
        h.core
            .send_observed_image(prepared, b"\x89PNG\r\n\x1a\nstale".to_vec())
            .await
            .is_err()
    );
    let observed = h.core.observe_clipboard_offer().unwrap();
    h.core
        .toggle_setting(pneud::ui::Toggle::IncludeImages)
        .await?;
    h.core
        .toggle_setting(pneud::ui::Toggle::IncludeImages)
        .await?;
    assert!(
        h.core
            .send_observed_image(observed, b"\x89PNG\r\n\x1a\npolicy".to_vec())
            .await
            .is_err()
    );
    assert_eq!(
        device_rows(&h, &phone.identity.device_id, "outgoing")?,
        0,
        "image preparation keeps clipboard delivery in memory"
    );
    let observed = h.core.observe_clipboard_offer().unwrap();
    write_frame(&mut tls, &Message::Status(Status { paused: true })).await?;
    event_matching(&mut events, |event| {
        matches!(event, pneud::Event::PhonePaused { paused: true, .. })
    })
    .await?;
    write_frame(&mut tls, &Message::Status(Status { paused: false })).await?;
    event_matching(&mut events, |event| {
        matches!(event, pneud::Event::PhonePaused { paused: false, .. })
    })
    .await?;
    assert!(
        h.core
            .send_observed_image(observed, b"\x89PNG\r\n\x1a\npause".to_vec())
            .await
            .is_err()
    );
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: initial,
            deduplicated: false,
        }),
    )
    .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(150), read_frame(&mut tls))
            .await
            .is_err(),
        "an invalidated prepared image must not be sent"
    );
    let initial = hold_file_lane(&h, &phone, &mut tls).await?;
    let first = b"\x89PNG\r\n\x1a\nold".to_vec();
    let mut newest = b"\x89PNG\r\n\x1a\n".to_vec();
    newest.extend(vec![7; CHUNK_SIZE * 3]);
    h.core
        .send_clipboard_image(phone.identity.device_id.clone(), first)
        .await?;
    let image_id = h
        .core
        .send_clipboard_image(phone.identity.device_id.clone(), newest.clone())
        .await?;
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: initial,
            deduplicated: false,
        }),
    )
    .await?;
    let Some(Message::FileBegin(begin)) =
        tokio::time::timeout(Duration::from_secs(2), read_frame(&mut tls)).await??
    else {
        panic!("expected image FILE_BEGIN")
    };
    assert_eq!(begin.transfer_id, image_id);
    assert_eq!(begin.kind, FileKind::ClipboardImage);
    assert_eq!(begin.size, newest.len() as u64);
    let challenge = Ping {
        nonce: Nonce::new("1".repeat(16))?,
    };
    write_frame(&mut tls, &Message::Ping(challenge.clone())).await?;
    let mut received = Vec::new();
    let mut pong = false;
    let mut ended = false;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !pong || !ended {
            match read_frame(&mut tls).await?.expect("image frame") {
                Message::FileChunk(chunk) => {
                    assert_eq!(chunk.transfer_id, image_id);
                    assert_eq!(chunk.offset, received.len() as u64);
                    received.extend(chunk.data);
                }
                Message::FileEnd(end) => {
                    assert_eq!(end.transfer_id, image_id);
                    ended = true;
                }
                Message::Pong(p) => {
                    assert_eq!(p, challenge);
                    pong = true;
                }
                other => panic!("unexpected image frame {other:?}"),
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert_eq!(received, newest);
    assert_eq!(
        begin.sha256.as_str(),
        hex::encode(Sha256::digest(&received))
    );
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: image_id.clone(),
            deduplicated: false,
        }),
    )
    .await?;
    event_matching(&mut events,|event|matches!(event,pneud::Event::ClipboardSent {transfer_id,..} if *transfer_id==image_id)).await?;
    write_frame(&mut tls, &Message::Status(Status { paused: true })).await?;
    event_matching(&mut events, |event| {
        matches!(event, pneud::Event::PhonePaused { paused: true, .. })
    })
    .await?;
    assert!(
        h.core
            .send_clipboard_image(phone.identity.device_id.clone(), received)
            .await
            .is_err()
    );
    tls.shutdown().await?;
    drop(tls);
    h.stop().await
}

#[tokio::test]
async fn automatic_clipboard_text_is_latest_only_and_never_queued_across_connections() -> Result<()>
{
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut events = h.core.subscribe();
    let mut connection = h.pair(&phone).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.core.clipboard_target().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let (device, generation) = h.core.clipboard_target().unwrap();
    let first = h
        .core
        .send_clipboard(device.clone(), "in flight".into())
        .await?;
    assert!(
        matches!(connection.read(Duration::from_secs(2)).await?,Message::Clipboard(text) if text.transfer_id==first)
    );
    h.core
        .send_automatic_clipboard(device.clone(), generation, "old selection".into())
        .await?;
    let newest = h
        .core
        .send_automatic_clipboard(device.clone(), generation, "newest selection".into())
        .await?;
    assert_eq!(
        device_rows(&h, &device, "outgoing")?,
        0,
        "clipboard delivery stays in memory while an explicit send awaits ACK"
    );
    connection
        .write(&Message::Ack(Ack {
            transfer_id: first,
            deduplicated: false,
        }))
        .await?;
    assert!(
        matches!(connection.read(Duration::from_secs(2)).await?,Message::Clipboard(text) if text.transfer_id==newest && text.text=="newest selection")
    );
    h.core
        .send_automatic_clipboard(device.clone(), generation, "pending at disconnect".into())
        .await?;
    connection.close().await?;
    drop(connection);
    event_matching(&mut events, |event| {
        matches!(
            event,
            pneud::Event::ConnectionChanged {
                connected: false,
                ..
            }
        )
    })
    .await?;
    let mut connection = phone.reconnect().await?;
    event_matching(&mut events, |event| {
        matches!(
            event,
            pneud::Event::ConnectionChanged {
                connected: true,
                ..
            }
        )
    })
    .await?;
    assert!(
        h.core
            .send_automatic_clipboard(device.clone(), generation, "stale offer".into())
            .await
            .is_err()
    );
    assert!(
        connection.read(Duration::from_millis(200)).await.is_err(),
        "disconnected selection must not arrive on reconnect"
    );
    assert_eq!(device_rows(&h, &device, "outgoing")?, 0);
    connection.close().await?;
    drop(connection);
    h.stop().await
}

#[tokio::test]
async fn offers_policy_changes_and_stale_discard_cannot_send_prepared_clipboard() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    let device = phone.identity.device_id.clone();
    let explicit = h
        .core
        .send_clipboard(device.clone(), "explicit awaiting receipt".into())
        .await?;
    assert!(
        matches!(c.read(Duration::from_secs(2)).await?,Message::Clipboard(text) if text.transfer_id==explicit)
    );
    let prepared = h.core.observe_clipboard_offer().unwrap();
    h.core
        .send_observed_clipboard(prepared, "prepared A".into())
        .await?;
    let stale = h.core.observe_clipboard_offer().unwrap(); // clear, unsupported and secret offers all follow this path
    assert!(
        h.core
            .send_observed_clipboard(stale.clone(), "slow offer".into())
            .await
            .is_ok()
    );
    h.core.observe_clipboard_offer();
    assert!(
        h.core
            .send_observed_clipboard(stale, "stale async offer".into())
            .await
            .is_err()
    );
    assert_eq!(device_rows(&h, &device, "outgoing")?, 0);
    c.write(&Message::Ack(Ack {
        transfer_id: explicit,
        deduplicated: false,
    }))
    .await?;
    assert!(c.read(Duration::from_millis(150)).await.is_err());
    let observed = h.core.observe_clipboard_offer().unwrap();
    c.write(&Message::Status(Status { paused: true })).await?;
    let mut events = h.core.subscribe();
    // Querying generation while STATUS is handled avoids relying on sleeps.
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.core.phone_paused(&device) != Some(true) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    c.write(&Message::Status(Status { paused: false })).await?;
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::PhonePaused { paused: false, .. })
    })
    .await?;
    assert!(
        h.core
            .send_observed_clipboard(observed, "debounced before pause".into())
            .await
            .is_err()
    );
    let observed = h.core.observe_clipboard_offer().unwrap();
    h.core.toggle_setting(pneud::ui::Toggle::Clipboard).await?;
    h.core.toggle_setting(pneud::ui::Toggle::Clipboard).await?;
    assert!(
        h.core
            .send_observed_clipboard(observed, "debounced before policy change".into())
            .await
            .is_err()
    );
    let replacement = h.phone("replacement")?;
    h.core.unpair_device(device.clone()).await?;
    drop(c);
    // A different unknown certificate must respect the one-second admission throttle.
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let mut newer = h.pair(&replacement).await?;
    let generation = h.core.observe_clipboard_offer().unwrap();
    let latest = h
        .core
        .send_observed_clipboard(generation, "new phone selection".into())
        .await?;
    assert!(h.core.discard_clipboard(device).await.is_err());
    assert!(
        matches!(newer.read(Duration::from_secs(2)).await?,Message::Clipboard(text) if text.transfer_id==latest)
    );
    drop(newer);
    h.stop().await
}
#[tokio::test]
async fn save_folder_ipc_and_startup_reject_cache_aliases_and_children() -> Result<()> {
    let h = Harness::new().await?;
    let alias = h.dir.path().join("cache-alias");
    std::os::unix::fs::symlink(&h.core.paths.cache, &alias)?;
    let original = h.core.config();
    original.save(&h.core.paths.config)?;
    let persisted = std::fs::read(&h.core.paths.config)?;
    let (listener, _owner) = pneud::ipc::bind(&h.core.paths.socket)?;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let ipc = tokio::spawn(pneud::ipc::serve(h.core.clone(), listener, rx));
    let outcome: Result<()> = async {
        for folder in [
            h.core.paths.cache.clone(),
            h.core.paths.cache.join("new/nested"),
            alias.clone(),
            alias.join("new/nested"),
        ] {
            let mut config = original.clone();
            config.save_path = folder.clone();
            let error = h.core.update_config(config.clone()).await.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("save folder must not be the daemon cache directory")
            );
            let error = Core::new(h.core.paths.clone(), config, h.actions.clone())
                .await
                .err()
                .expect("startup rejects cache save folder");
            assert!(
                error
                    .to_string()
                    .contains("save folder must not be the daemon cache directory")
            );
            // The GUI folder chooser issues this same IPC command.
            let error = pneud::ipc::request(
                &h.core.paths.socket,
                pneud::ipc::Command::SaveFolder { folder },
            )
            .await
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("save folder must not be the daemon cache directory")
            );
            assert_eq!(h.core.config().save_path, original.save_path);
            assert_eq!(std::fs::read(&h.core.paths.config)?, persisted);
        }
        let allowed = h.dir.path().join("cache-sibling/new");
        pneud::ipc::request(
            &h.core.paths.socket,
            pneud::ipc::Command::SaveFolder {
                folder: allowed.clone(),
            },
        )
        .await?;
        assert_eq!(h.core.config().save_path, allowed);
        Ok(())
    }
    .await;
    stop.send_replace(true);
    ipc.await??;
    h.stop().await?;
    outcome
}

#[tokio::test]
async fn field_specific_settings_preserve_concurrent_folder_and_toggle_changes() -> Result<()> {
    let h = Harness::new().await?;
    for index in 0..12 {
        let folder = h.dir.path().join(format!("folder-{index}"));
        let (a, b) = tokio::join!(
            h.core.change_save_folder(folder.clone()),
            h.core.toggle_setting(pneud::ui::Toggle::Notifications)
        );
        a?;
        b?;
        assert_eq!(h.core.config().save_path, folder);
        assert_eq!(h.core.config().notification_preview, index % 2 != 0);
        assert_eq!(Config::load(&h.core.paths.config)?.save_path, folder);
    }
    h.stop().await
}

#[tokio::test]
async fn unfinished_receive_resets_on_reconnect_and_unpair() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut old = h.pair(&phone).await?;
    let mut events = h.core.subscribe();
    let bytes = b"partial transfer";
    let meta = metadata(Id::from_bytes(rand::random()), bytes);
    begin_chunks(&mut old, &meta, bytes).await?;
    event_matching(&mut events,|event|matches!(event,pneud::Event::TransferProgress {transfer_id,..} if *transfer_id==meta.transfer_id)).await?;
    assert!(h.core.ui_snapshot().receiving);
    let mut replacement = phone.reconnect().await?;
    event_matching(&mut events, |event| {
        matches!(
            event,
            pneud::Event::ConnectionChanged {
                connected: true,
                ..
            }
        )
    })
    .await?;
    assert!(
        !h.core.ui_snapshot().receiving,
        "reconnect must clear unfinished transfer and receiving grace"
    );
    assert_eq!(h.core.ui_snapshot().icon(), "pneu-tray-connected");
    let meta = metadata(Id::from_bytes(rand::random()), bytes);
    begin_chunks(&mut replacement, &meta, bytes).await?;
    event_matching(&mut events,|event|matches!(event,pneud::Event::TransferProgress {transfer_id,..} if *transfer_id==meta.transfer_id)).await?;
    assert!(h.core.ui_snapshot().receiving);
    h.core
        .unpair_device(phone.identity.device_id.clone())
        .await?;
    assert!(!h.core.ui_snapshot().receiving);
    assert!(!h.core.ui_snapshot().paired);
    assert_eq!(h.core.ui_snapshot().icon(), "pneu-tray-disconnected");
    drop(old);
    drop(replacement);
    h.stop().await
}
#[tokio::test]
async fn disconnect_and_orderly_shutdown_keep_last_seen_after_restart() -> Result<()> {
    for shutdown in [false, true] {
        let h = Harness::new().await?;
        let phone = h.phone("phone")?;
        let mut c = h.pair(&phone).await?;
        let ping = Ping {
            nonce: Nonce::new("c".repeat(16))?,
        };
        c.write(&Message::Ping(ping.clone())).await?;
        assert_eq!(c.read(Duration::from_secs(2)).await?, Message::Pong(ping)); // admission persistence finished
        let before = h.core.ui_snapshot().phone.last_seen;
        tokio::time::sleep(Duration::from_millis(20)).await;
        if !shutdown {
            let mut events = h.core.subscribe();
            c.close().await?;
            event_matching(&mut events, |event| {
                matches!(
                    event,
                    pneud::Event::ConnectionChanged {
                        connected: false,
                        ..
                    }
                )
            })
            .await?;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let disk: pneud::ui::Phone = serde_json::from_slice(&std::fs::read(
                        h.core.paths.data.join("phone.json"),
                    )?)?;
                    if disk.last_seen > before {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
        }
        let Harness {
            dir,
            core,
            task,
            actions,
            ..
        } = h;
        let paths = core.paths.clone();
        let config = core.config();
        core.request_shutdown();
        tokio::time::timeout(Duration::from_secs(3), task).await???;
        let final_seen = core.ui_snapshot().phone.last_seen;
        assert!(final_seen > before);
        assert!(!core.ui_snapshot().receiving);
        drop(c);
        drop(core);
        let restarted = Core::new(paths, config, actions).await?;
        assert_eq!(restarted.ui_snapshot().phone.last_seen, final_seen);
        assert!(!restarted.ui_snapshot().connected);
        restarted.shutdown_actions().await?;
        drop(restarted);
        drop(dir);
    }
    Ok(())
}

#[tokio::test]
async fn two_phones_receive_automatic_clipboard_and_unpair_independently() -> Result<()> {
    let h = Harness::new().await?;
    let first = h.phone("first")?;
    let second = h.phone("second")?;
    let mut a = h.pair(&first).await?;
    // Unknown certificates are intentionally admitted at most once per second.
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let mut b = h.pair(&second).await?;
    assert_eq!(h.core.paired_devices().len(), 2);
    assert_eq!(h.core.connected_devices().len(), 2);
    let offer = h.core.observe_clipboard_offer().unwrap();
    let id = h
        .core
        .send_observed_clipboard(offer, "both phones".into())
        .await?;
    for connection in [&mut a, &mut b] {
        assert!(
            matches!(connection.read(Duration::from_secs(2)).await?, Message::Clipboard(c) if c.transfer_id == id && c.text == "both phones")
        );
    }
    // One pending receipt cannot hold up the other recipient's next delivery.
    b.write(&Message::Ack(Ack {
        transfer_id: id.clone(),
        deduplicated: false,
    }))
    .await?;
    let offer = h.core.observe_clipboard_offer().unwrap();
    let next = h
        .core
        .send_observed_clipboard(offer, "independent delivery".into())
        .await?;
    assert!(
        matches!(b.read(Duration::from_secs(2)).await?,Message::Clipboard(c) if c.transfer_id == next)
    );
    b.write(&Message::Ack(Ack {
        transfer_id: next,
        deduplicated: false,
    }))
    .await?;
    h.core
        .unpair_device(first.identity.device_id.clone())
        .await?;
    assert!(matches!(
        a.read(Duration::from_secs(2)).await?,
        Message::Unpair(_)
    ));
    assert_eq!(
        h.core.connected_devices(),
        vec![second.identity.device_id.clone()]
    );
    assert!(
        !b.send_text(Clipboard {
            sensitive: false,
            hlc: None,
            source_device: None,
            transfer_id: Id::from_bytes(rand::random()),
            text: "still authorized".into(),
            created_at: 1,
            origin: ClipboardOrigin::Clipboard
        })
        .await?
        .deduplicated
    );
    assert_eq!(&*h.actions.text.lock().unwrap(), &["still authorized"]);
    drop(a);
    drop(b);
    h.stop().await
}

#[tokio::test]
async fn frozen_recipients_and_same_device_replacement_preserve_other_session() -> Result<()> {
    let h = Harness::new().await?;
    let first = h.phone("first")?;
    let second = h.phone("second")?;
    let mut a = h.pair(&first).await?;
    let frozen = h.core.observe_clipboard_offer().unwrap();
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let mut b = h.pair(&second).await?;
    let id = h
        .core
        .send_observed_clipboard(frozen, "before second connected".into())
        .await?;
    assert!(
        matches!(a.read(Duration::from_secs(2)).await?,Message::Clipboard(c) if c.transfer_id == id)
    );
    a.write(&Message::Ack(Ack {
        transfer_id: id,
        deduplicated: false,
    }))
    .await?;
    assert!(b.read(Duration::from_millis(100)).await.is_err());
    let mut events = h.core.subscribe();
    let mut replacement = first.reconnect().await?;
    event_matching(&mut events, |e|matches!(e,pneud::Event::ConnectionChanged {device,connected:true} if *device == first.identity.device_id)).await?;
    assert!(matches!(
        a.read(Duration::from_secs(2)).await?,
        Message::Error(ErrorMessage {
            code: ErrorCode::ConnectionReplaced,
            ..
        })
    ));
    assert_eq!(h.core.connected_devices().len(), 2);
    // A new certificate with this same ID exercises approved re-pairing.
    let mut repaired = h.phone("repaired")?;
    repaired.identity.device_id = first.identity.device_id.clone();
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let mut current = h.pair(&repaired).await?;
    assert!(matches!(
        replacement.read(Duration::from_secs(2)).await?,
        Message::Error(ErrorMessage {
            code: ErrorCode::Unauthorized,
            ..
        })
    ));
    assert_eq!(h.core.paired_devices().len(), 2);
    assert_eq!(h.core.connected_devices().len(), 2);
    let offer = h.core.observe_clipboard_offer().unwrap();
    h.core
        .unpair_device(first.identity.device_id.clone())
        .await?;
    // Removing even the first frozen recipient cannot cancel the other's offer.
    let id = h
        .core
        .send_observed_clipboard(offer, "remaining recipient".into())
        .await?;
    assert!(
        matches!(b.read(Duration::from_secs(2)).await?,Message::Clipboard(c) if c.transfer_id == id)
    );
    b.write(&Message::Ack(Ack {
        transfer_id: id,
        deduplicated: false,
    }))
    .await?;
    assert!(matches!(
        current.read(Duration::from_secs(2)).await?,
        Message::Unpair(_)
    ));
    assert!(
        !b.send_text(second.text(
            "authorized after re-pair".into(),
            ClipboardOrigin::Clipboard
        )?)
        .await?
        .deduplicated
    );
    drop((a, b, replacement, current));
    h.stop().await
}

#[tokio::test]
async fn sweep_ready_budget_refuses_extra_peer_and_allows_same_peer_replacement() -> Result<()> {
    let h = Harness::new().await?;
    let mut phones = Vec::new();
    for index in 0..16 {
        if index != 0 {
            tokio::time::sleep(Duration::from_millis(1010)).await;
        }
        let phone = h.phone(&format!("peer-{index}"))?;
        let mut connection = h.pair(&phone).await?;
        let mut events = h.core.subscribe();
        connection.close().await?;
        event_matching(&mut events, |e|matches!(e,pneud::Event::ConnectionChanged {device,connected:false} if *device==phone.identity.device_id)).await?;
        phones.push(phone);
    }
    let extra_phone = h.phone("legacy-peer-16")?;
    let denied = h
        .pair(&extra_phone)
        .await
        .err()
        .expect("a new seventeenth binding must be refused");
    assert!(denied.to_string().contains("StorageFailed"));
    assert_eq!(h.core.paired_devices().len(), 16);
    // Released databases may already exceed the new admission cap. Seed that
    // legacy condition while this disposable daemon is fully stopped, then
    // test the independent READY-connection budget and same-peer replacement.
    let paths = h.core.paths.clone();
    let config = h.core.config();
    let actions = h.actions.clone();
    h.core.request_shutdown();
    h.task.await??;
    drop(h.core);
    {
        let db = rusqlite::Connection::open(paths.data.join("state.sqlite3"))?;
        db.execute("INSERT INTO peers(id,fp,generation,role,name,addresses,established) VALUES(?,?,1,'phone','Legacy peer','[]',0)",rusqlite::params![extra_phone.identity.device_id.as_str(),extra_phone.identity.fingerprint.as_str()])?;
        db.execute(
            "INSERT INTO peer_generations VALUES(?,1)",
            [extra_phone.identity.device_id.as_str()],
        )?;
    }
    // All clients in this fixture bind to the same computer identity/routes.
    std::fs::copy(
        phones[0].directory.join("binding.json"),
        extra_phone.directory.join("binding.json"),
    )?;
    phones.push(extra_phone);
    let core = Core::new(paths, config, actions.clone()).await?;
    let listener = TcpListener::from_std(h.listener.try_clone()?)?;
    let task = tokio::spawn(core.clone().serve(listener, false));
    let h = Harness {
        dir: h.dir,
        core,
        task,
        actions,
        address: h.address,
        listener: h.listener,
    };
    let mut connected = Vec::new();
    for phone in phones.iter().take(16) {
        let mut events = h.core.subscribe();
        connected.push(phone.reconnect().await?);
        event_matching(&mut events, |e|matches!(e,pneud::Event::ConnectionChanged {device,connected:true} if *device==phone.identity.device_id)).await?;
    }
    assert_eq!(h.core.connected_devices().len(), 16);
    let mut extra = phones[16].reconnect().await?;
    let refused = extra.read(Duration::from_secs(2)).await;
    let count = h.core.connected_devices().len();
    let mut events = h.core.subscribe();
    let mut replacement = phones[0].reconnect().await?;
    event_matching(&mut events, |e|matches!(e,pneud::Event::ConnectionChanged {device,connected:true} if *device==phones[0].identity.device_id)).await?;
    let replaced = connected[0].read(Duration::from_secs(2)).await?;
    let ping = Ping {
        nonce: Nonce::new("d".repeat(16))?,
    };
    replacement.write(&Message::Ping(ping.clone())).await?;
    assert_eq!(
        replacement.read(Duration::from_secs(2)).await?,
        Message::Pong(ping)
    );
    let mut pending = Vec::new();
    for _ in 0..16 {
        pending.push(raw_connect(&h, &phones[0]).await?);
    }
    let overflow = raw_connect(&h, &phones[0]).await;
    drop((connected, replacement, extra, pending));
    h.stop().await?;
    assert!(matches!(
        refused,
        Ok(Message::Error(ErrorMessage {
            code: ErrorCode::Busy,
            ..
        }))
    ));
    assert_eq!(count, 16);
    assert!(matches!(
        replaced,
        Message::Error(ErrorMessage {
            code: ErrorCode::ConnectionReplaced,
            ..
        })
    ));
    assert!(overflow.is_err(), "pending transports must remain bounded");
    Ok(())
}

#[test]
fn sweep_superseded_connections_close_while_metadata_worker_is_blocked() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let h = Harness::new().await?;
        let phone = h.phone("paired")?;
        let mut initial = h.pair(&phone).await?;
        let ping = Ping {
            nonce: Nonce::new("d".repeat(16))?,
        };
        initial.write(&Message::Ping(ping.clone())).await?;
        assert_eq!(
            initial.read(Duration::from_secs(2)).await?,
            Message::Pong(ping)
        );
        // PONG bypasses READY initialization. Reach the control loop before
        // occupying the only blocking worker and exercising replacements.
        let mut events = h.core.subscribe();
        for paused in [true, false] {
            initial.write(&Message::Status(Status { paused })).await?;
            event_matching(&mut events, |event| {
                matches!(event, pneud::Event::PhonePaused { paused: value, .. } if *value == paused)
            }).await?;
        }
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        });
        entered_rx.await?;
        let mut held = Vec::new();
        for _ in 0..200 {
            let mut events = h.core.subscribe();
            held.push(phone.reconnect().await?);
            event_matching(&mut events, |e| {
                matches!(
                    e,
                    pneud::Event::ConnectionChanged {
                        connected: true,
                        ..
                    }
                )
            })
            .await?;
        }
        let mut closed = 0;
        for connection in held.iter_mut().take(199) {
            if matches!(
                connection.read(Duration::from_millis(100)).await,
                Ok(Message::Error(ErrorMessage {
                    code: ErrorCode::ConnectionReplaced,
                    ..
                }))
            ) && connection.read(Duration::from_millis(100)).await.is_err()
            {
                closed += 1;
            }
        }
        release_tx.send(())?;
        blocker.await?;
        drop((initial, held));
        h.stop().await?;
        assert_eq!(
            closed, 199,
            "superseded sockets must close before metadata persistence resumes"
        );
        Ok(())
    })
}

#[tokio::test]
async fn sweep_repair_cancels_own_actions_and_other_pair_preserves_them() -> Result<()> {
    for same_device in [true, false] {
        let h = Harness::new().await?;
        let old = h.phone("old")?;
        let mut next = h.phone("next")?;
        if same_device {
            next.identity.device_id = old.identity.device_id.clone();
        }
        assert_ne!(old.identity.fingerprint, next.identity.fingerprint);
        let mut c = h.pair(&old).await?;
        let bytes = b"\x89PNG\r\n\x1a\nimage";
        h.actions
            .block_image
            .store(true, std::sync::atomic::Ordering::SeqCst);
        for (index, kind) in [FileKind::Screenshot, FileKind::ClipboardImage]
            .into_iter()
            .enumerate()
        {
            let bytes = [bytes.as_slice(), index.to_string().as_bytes()].concat();
            let mut meta = metadata(Id::from_bytes(rand::random()), &bytes);
            meta.file_name = format!("old-{index}.png");
            meta.kind = kind;
            begin_chunks(&mut c, &meta, &bytes).await?;
            assert!(matches!(
                end(&mut c, meta.transfer_id).await?,
                Message::Ack(_)
            ));
            if index == 0 {
                tokio::time::timeout(Duration::from_secs(2), h.actions.image_entered.notified())
                    .await?;
            }
        }
        tokio::time::sleep(Duration::from_millis(1010)).await;
        let mut replacement = h.pair(&next).await?;
        h.actions
            .block_image
            .store(false, std::sync::atomic::Ordering::SeqCst);
        h.actions.image_release.notify_one();
        if same_device {
            assert!(matches!(
                c.read(Duration::from_secs(2)).await?,
                Message::Error(ErrorMessage {
                    code: ErrorCode::Unauthorized,
                    ..
                })
            ));
            assert!(old.reconnect().await.is_err());
            let new_bytes = b"\x89PNG\r\n\x1a\nnew image";
            let mut meta = metadata(Id::from_bytes(rand::random()), new_bytes);
            meta.file_name = "new.png".into();
            meta.kind = FileKind::ClipboardImage;
            begin_chunks(&mut replacement, &meta, new_bytes).await?;
            assert!(matches!(
                end(&mut replacement, meta.transfer_id).await?,
                Message::Ack(_)
            ));
            h.wait_images(1).await?;
            assert_eq!(h.actions.images.lock().unwrap().len(), 1);
            assert_eq!(
                *h.actions.image_bytes.lock().unwrap(),
                vec![new_bytes.to_vec()]
            );
        } else {
            h.wait_images(2).await?;
            assert_eq!(h.core.connected_devices().len(), 2);
            assert_eq!(h.actions.images.lock().unwrap().len(), 2);
        }
        drop((c, replacement));
        h.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn computer_invitation_join_preserves_phone_and_reports_role() -> Result<()> {
    let a = Harness::mesh().await?;
    let b = Harness::mesh().await?;
    let phone = a.phone("mesh-phone")?;
    let mut phone_link = a.pair(&phone).await?;
    let uri = a
        .core
        .start_pairing_with_addresses(vec![a.address.clone()])
        .await?
        .build()?;
    let preview = b.core.join(&uri).await?;
    assert_eq!(preview["state"], "confirmation");
    assert_eq!(preview["fingerprint"], a.core.identity.fingerprint.as_str());
    assert!(b.core.paired_devices().is_empty());
    b.core
        .confirm_join(preview["attempt"].as_str().unwrap())
        .await?;
    tokio::time::timeout(Duration::from_secs(8), async {
        while a.core.connected_devices().len() != 2 || b.core.connected_devices().len() != 1 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("invitation did not connect")?;
    wait_computer_setup(&a, &b).await?;
    phone_link.send_status(true).await?;
    let mut sent = b.core.subscribe();
    let clip = b
        .core
        .send_clipboard(a.core.identity.device_id.clone(), "computer text".into())
        .await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !a
            .actions
            .text
            .lock()
            .unwrap()
            .iter()
            .any(|t| t == "computer text")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("computer clipboard did not arrive")?;
    event_matching(&mut sent, |event| matches!(event, pneud::Event::ClipboardSent {transfer_id, ..} if transfer_id == &clip)).await?;
    let devices = serde_json::to_value(b.core.ui_snapshot())?;
    assert_eq!(devices["devices"][0]["role"], "computer");
    b.core
        .remove_setup_device(a.core.identity.device_id.clone())
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while a.core.paired_devices().contains(&b.core.identity.device_id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("setup removal did not reach peer")?;
    phone_link.close().await?;
    a.stop().await?;
    b.stop().await?;
    Ok(())
}

#[tokio::test]
async fn computer_invitation_errors_and_cli_stdin_are_private() -> Result<()> {
    use pneud::ipc::{self, Command};
    let a = Harness::mesh().await?;
    let b = Harness::mesh().await?;
    let c = Harness::mesh().await?;
    let invitation = a
        .core
        .start_pairing_with_addresses(vec![a.address.clone()])
        .await?;
    let uri = invitation.build()?;
    assert_eq!(a.core.join(&uri).await?["state"], "own_code");
    assert_eq!(b.core.join("pneu://pair?broken").await?["state"], "garbled");
    let mut expired = invitation.clone();
    expired.expires_at = 1;
    assert_eq!(b.core.join(&expired.build()?).await?["state"], "expired");
    let (listener, _owner) = ipc::bind(&b.core.paths.socket)?;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let ipc_task = tokio::spawn(ipc::serve(b.core.clone(), listener, rx));
    // Run the real CLI with a stdin pipe. Its only argv is `join`; all config
    // and runtime paths point at the mock Core, never the user's daemon.
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pneud"))
        .arg("join")
        .env("HOME", b.dir.path())
        .env("XDG_DATA_HOME", b.dir.path().join("data"))
        .env("XDG_CONFIG_HOME", b.dir.path().join("config"))
        .env("XDG_CACHE_HOME", b.dir.path().join("cache"))
        .env("XDG_RUNTIME_DIR", b.dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    eprintln!(
        "stdin CLI PID={} runtime={} (no daemon process)",
        child.id().unwrap(),
        b.dir.path().display()
    );
    #[cfg(target_os = "linux")]
    {
        let argv = std::fs::read(format!("/proc/{}/cmdline", child.id().unwrap()))?;
        assert!(!argv.windows(uri.len()).any(|w| w == uri.as_bytes()));
    }
    use tokio::io::AsyncWriteExt;
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(format!("{uri}\n").as_bytes()).await?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let preview: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(preview["state"], "confirmation");
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&uri));
    let attempt = preview["attempt"].as_str().unwrap().to_owned();
    ipc::request(
        &b.core.paths.socket,
        Command::JoinConfirm {
            attempt: attempt.clone(),
        },
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while b.core.join_status(&attempt).unwrap()["state"] == "pairing" {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(b.core.join_status(&attempt)?["state"], "paired");
    assert_eq!(b.core.join(&uri).await?["state"], "already_paired");
    let reused = c.core.join(&uri).await?;
    let attempt = reused["attempt"].as_str().unwrap();
    c.core.confirm_join(attempt).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while c.core.join_status(attempt).unwrap()["state"] == "pairing" {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    // The consumed invitation closes provisional admission. A generic pinned
    // authorization denial is not evidence that this particular token was used.
    assert_eq!(c.core.join_status(attempt)?["state"], "unreachable");
    stop.send_replace(true);
    ipc_task.await??;
    a.stop().await?;
    b.stop().await?;
    c.stop().await?;
    Ok(())
}

// Mesh admission is exercised with a simulator identity, never the real clipboard.
async fn mesh_phone(h: &Harness, phone: &Phone) -> Result<PhoneTls> {
    negotiated_phone(h, phone, true, true).await
}
async fn hello_phone(h: &Harness, phone: &Phone, mesh: bool, text: bool) -> Result<PhoneTls> {
    let mut events = h.core.subscribe();
    let mut tls = raw_connect(h, phone).await?;
    write_frame(
        &mut tls,
        &Message::Hello(Hello {
            setup: None,
            version: 1,
            device_id: phone.identity.device_id.clone(),
            name: "Mesh phone".into(),
            capabilities: [
                mesh.then_some("mesh"),
                text.then_some("clipboard_text"),
                Some("files"),
                Some("clipboard_image"),
            ]
            .into_iter()
            .flatten()
            .map(str::to_owned)
            .collect(),
            role: Some(DeviceRole::Phone),
            addresses: None,
            paused: None,
        }),
    )
    .await?;
    assert!(matches!(
        read_frame(&mut tls).await?,
        Some(Message::Hello(_))
    ));
    event_matching(&mut events, |e| matches!(e, pneud::Event::ConnectionChanged {device, connected:true} if device == &phone.identity.device_id)).await?;
    Ok(tls)
}
async fn negotiated_phone(h: &Harness, phone: &Phone, mesh: bool, text: bool) -> Result<PhoneTls> {
    let mut tls = hello_phone(h, phone, mesh, text).await?;
    if !mesh {
        return Ok(tls);
    }
    // A mesh endpoint sends full state immediately after HELLO. Mirror Android:
    // persist/ACK the snapshot and return our complete state before content.
    let Message::PeerSettings(snapshot) = wire_reply(&mut tls).await? else {
        panic!("missing settings snapshot")
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
    assert!(
        matches!(wire_reply(&mut tls).await?,Message::Ack(ack) if ack.transfer_id==response.transfer_id)
    );
    Ok(tls)
}
async fn wire_reply(tls: &mut PhoneTls) -> Result<Message> {
    Ok(
        tokio::time::timeout(Duration::from_secs(3), read_frame(tls))
            .await??
            .expect("reply"),
    )
}
#[tokio::test]
async fn mesh_rejects_unstamped_skewed_and_forged_clipboard_without_effects() -> Result<()> {
    for case in ["unstamped", "skew", "source", "image"] {
        let h = Harness::mesh().await?;
        let phone = h.phone("phone")?;
        h.pair(&phone).await?.close().await?;
        let mut tls = mesh_phone(&h, &phone).await?;
        let mut text = phone.text("invalid".into(), ClipboardOrigin::Clipboard)?;
        text.hlc = Some(pneud::now_ms());
        text.source_device = Some(phone.identity.device_id.clone());
        match case {
            "unstamped" => {
                text.hlc = None;
                text.source_device = None;
            }
            "skew" => text.hlc = Some(pneud::now_ms() + 120_000),
            "source" => text.source_device = Some(h.core.identity.device_id.clone()),
            _ => {}
        }
        let message = if case == "image" {
            let mut begin = metadata(text.transfer_id.clone(), b"\x89PNG\r\n\x1a\nimage");
            begin.kind = FileKind::ClipboardImage;
            Message::FileBegin(begin)
        } else {
            Message::Clipboard(text)
        };
        write_frame(&mut tls, &message).await?;
        assert!(
            matches!(wire_reply(&mut tls).await?, Message::Error(e) if e.code == ErrorCode::MalformedMessage),
            "{case}"
        );
        assert!(h.actions.text.lock().unwrap().is_empty());
        assert!(h.actions.images.lock().unwrap().is_empty());
        assert_eq!(
            h.core.paired_devices(),
            vec![phone.identity.device_id.clone()]
        );
        assert_eq!(
            device_rows(&h, &phone.identity.device_id, "clipboard_recent")?,
            0
        );
        let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
        assert_eq!(
            db.query_row("SELECT last_seen FROM mesh_clock", [], |r| r
                .get::<_, String>(0))?,
            "0"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM clipboard_clock", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        drop(db);
        drop(tls);
        h.stop().await?;
    }
    Ok(())
}
#[tokio::test]
async fn mesh_global_order_and_slow_image_recheck_preserve_newer_text() -> Result<()> {
    let h = Harness::mesh().await?;
    let p = h.phone("p")?;
    let q = h.phone("q")?;
    h.pair(&p).await?.close().await?;
    h.pair(&q).await?.close().await?;
    let mut a = mesh_phone(&h, &p).await?;
    let mut b = mesh_phone(&h, &q).await?;
    let stamp = pneud::now_ms();
    let mut image = metadata(Id::from_bytes(rand::random()), b"\x89PNG\r\n\x1a\nimage");
    image.kind = FileKind::ClipboardImage;
    image.hlc = Some(stamp);
    image.source_device = Some(p.identity.device_id.clone());
    write_frame(&mut a, &Message::FileBegin(image.clone())).await?;
    let mut newer = q.text("newest".into(), ClipboardOrigin::Clipboard)?;
    newer.hlc = Some(stamp + 2);
    newer.source_device = Some(q.identity.device_id.clone());
    write_frame(&mut b, &Message::Clipboard(newer)).await?;
    assert!(matches!(
        wire_reply(&mut b).await?,
        Message::Ack(Ack {
            deduplicated: false,
            ..
        })
    ));
    let mut older = p.text("older from another peer".into(), ClipboardOrigin::Clipboard)?;
    older.hlc = Some(stamp + 1);
    older.source_device = Some(p.identity.device_id.clone());
    write_frame(&mut a, &Message::Clipboard(older)).await?;
    assert!(matches!(
        wire_reply(&mut a).await?,
        Message::Ack(Ack {
            deduplicated: true,
            ..
        })
    ));
    write_frame(
        &mut a,
        &Message::FileChunk(FileChunk {
            transfer_id: image.transfer_id.clone(),
            offset: 0,
            data: b"\x89PNG\r\n\x1a\nimage".to_vec(),
        }),
    )
    .await?;
    write_frame(
        &mut a,
        &Message::FileEnd(FileEnd {
            transfer_id: image.transfer_id,
        }),
    )
    .await?;
    assert!(matches!(wire_reply(&mut a).await?, Message::Ack(_)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*h.actions.text.lock().unwrap(), vec!["newest"]);
    assert!(h.actions.images.lock().unwrap().is_empty());
    drop((a, b));
    h.stop().await
}

// Token pairing can expose a transient READY connection before setup ID
// assignment reconnects. Connected-only clipboard assertions start only after
// matching persisted IDs have exchanged MEMBERS and pair settings are settled.
async fn wait_computer_setup(a: &Harness, b: &Harness) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let sa = a.core.setup_status().await?;
            let sb = b.core.setup_status().await?;
            let mut ready = !sa["setup_id"].is_null() && sa["setup_id"] == sb["setup_id"];
            for (local, remote) in [(a, b), (b, a)] {
                ready &= local.core.connected_devices().contains(&remote.core.identity.device_id);
                let db = rusqlite::Connection::open(local.core.paths.data.join("state.sqlite3"))?;
                ready &= db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM members_receipts WHERE device=? AND generation=(SELECT generation FROM setup_state WHERE singleton=1))",
                    [remote.core.identity.device_id.as_str()], |r| r.get::<_, bool>(0),
                )?;
                let settings = local.core.peer_settings(remote.core.identity.device_id.clone()).await?;
                ready &= settings["can_sync"] == true
                    && settings["settings"].as_array().is_some_and(|v| !v.is_empty() && v.iter().all(|f| f["pending"] == false));
            }
            if ready { return Ok::<_, anyhow::Error>(()); }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.context("computer setup and settings did not synchronize")??;
    Ok(())
}

async fn link_computers(a: &Harness, b: &Harness) -> Result<()> {
    let uri = a
        .core
        .start_pairing_with_addresses(vec![a.address.clone()])
        .await?
        .build()?;
    let join = b.core.join(&uri).await?;
    b.core
        .confirm_join(join["attempt"].as_str().unwrap())
        .await?;
    tokio::time::timeout(Duration::from_secs(8), async {
        while !a
            .core
            .connected_devices()
            .contains(&b.core.identity.device_id)
            || !b
                .core
                .connected_devices()
                .contains(&a.core.identity.device_id)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    wait_computer_setup(a, b).await
}
#[tokio::test]
async fn mesh_file_images_from_computer_and_phone_use_the_same_clipboard_register() -> Result<()> {
    let a = Harness::mesh().await?;
    let b = Harness::mesh().await?;
    b.core
        .set_setting("copy_image_to_clipboard".into(), true.into())
        .await?;
    link_computers(&a, &b).await?;
    let png = b"\x89PNG\r\n\x1a\nfile image".to_vec();
    let path = a.dir.path().join("selected.png");
    std::fs::write(&path, &png)?;
    let operation = a
        .core
        .send_files(
            vec![path],
            pneud::outbox::Targets::Devices(vec![b.core.identity.device_id.clone()]),
        )
        .await?;
    let operation: Id = serde_json::from_value(operation["operation"].clone())?;
    b.wait_images(1).await?;
    assert_eq!(b.actions.image_bytes.lock().unwrap()[0], png);
    let db = rusqlite::Connection::open(a.core.paths.data.join("state.sqlite3"))?;
    let meta: String = db.query_row(
        "SELECT meta FROM outbox_artifacts WHERE operation=?",
        [operation.as_str()],
        |r| r.get(0),
    )?;
    let begin: FileBegin = serde_json::from_str(&meta)?;
    assert_eq!(begin.source_device, Some(a.core.identity.device_id.clone()));
    assert!(begin.hlc.is_some());
    let applied = || -> Result<(String, String)> {
        let db = rusqlite::Connection::open(b.core.paths.data.join("state.sqlite3"))?;
        Ok(
            db.query_row("SELECT hlc,source_device FROM clipboard_clock", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?,
        )
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !applied().is_ok_and(|clock| clock.0 == begin.hlc.unwrap().to_string()) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(
        applied()?,
        (
            begin.hlc.unwrap().to_string(),
            a.core.identity.device_id.as_str().into()
        )
    );
    let local_effects: i64 =
        db.query_row("SELECT count(*) FROM clipboard_clock", [], |r| r.get(0))?;
    assert_eq!(
        local_effects, 0,
        "sending an image file must not apply a clipboard value locally"
    );
    drop(db);
    // Android sends ordinary image FILEs with the same authenticated origin
    // stamp. Exercise the phone receive path against the same global register.
    let phone = b.phone("image-phone")?;
    b.pair(&phone).await?.close().await?;
    let mut link = mesh_phone(&b, &phone).await?;
    let phone_png = b"\x89PNG\r\n\x1a\nphone image".to_vec();
    let mut incoming = metadata(Id::from_bytes(rand::random()), &phone_png);
    incoming.kind = FileKind::File;
    incoming.hlc = Some(pneud::now_ms().max(begin.hlc.unwrap() + 1));
    incoming.source_device = Some(phone.identity.device_id.clone());
    write_frame(&mut link, &Message::FileBegin(incoming.clone())).await?;
    write_frame(
        &mut link,
        &Message::FileChunk(FileChunk {
            transfer_id: incoming.transfer_id.clone(),
            offset: 0,
            data: phone_png,
        }),
    )
    .await?;
    write_frame(
        &mut link,
        &Message::FileEnd(FileEnd {
            transfer_id: incoming.transfer_id.clone(),
        }),
    )
    .await?;
    assert!(matches!(wire_reply(&mut link).await?, Message::Ack(_)));
    b.wait_images(2).await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !applied().is_ok_and(|clock| clock.0 == incoming.hlc.unwrap().to_string()) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(
        applied()?,
        (
            incoming.hlc.unwrap().to_string(),
            phone.identity.device_id.as_str().into()
        )
    );
    drop(link);
    a.stop().await?;
    b.stop().await
}

#[tokio::test]
async fn mesh_three_way_near_simultaneous_local_copies_converge_without_forwarding() -> Result<()> {
    let a = Harness::mesh().await?;
    let b = Harness::mesh().await?;
    let phone = a.phone("shared-phone")?;
    a.pair(&phone).await?.close().await?;
    b.pair(&phone).await?.close().await?;
    let mut pa = mesh_phone(&a, &phone).await?;
    let mut pb = mesh_phone(&b, &phone).await?;
    link_computers(&a, &b).await?;
    let oa = a.core.observe_clipboard_offer().unwrap();
    let ob = b.core.observe_clipboard_offer().unwrap();
    a.actions.text.lock().unwrap().push("copy A".into());
    b.actions.text.lock().unwrap().push("copy B".into());
    let (sa, sb) = tokio::join!(
        a.core.send_observed_clipboard(oa, "copy A".into()),
        b.core.send_observed_clipboard(ob, "copy B".into()),
    );
    let (id_a, id_b) = (sa?, sb?);
    let ca = match wire_reply(&mut pa).await? {
        Message::Clipboard(c) => c,
        m => panic!("{m:?}"),
    };
    let cb = match wire_reply(&mut pb).await? {
        Message::Clipboard(c) => c,
        m => panic!("{m:?}"),
    };
    assert_eq!(ca.transfer_id, id_a);
    assert_eq!(cb.transfer_id, id_b);
    assert_eq!(ca.source_device.as_ref(), Some(&a.core.identity.device_id));
    assert_eq!(cb.source_device.as_ref(), Some(&b.core.identity.device_id));
    let key = |c: &Clipboard| {
        (
            c.hlc.unwrap(),
            c.source_device.as_ref().unwrap().as_str().to_owned(),
        )
    };
    // A single simulated phone clipboard consumes the two links in the reverse
    // order as well: the greater tuple wins on every endpoint.
    let winner = if key(&ca) > key(&cb) { &ca } else { &cb };
    let mut phone_register = None;
    let mut phone_clipboard = String::new();
    for c in [&cb, &ca] {
        let stamp = key(c);
        if phone_register.as_ref().is_none_or(|r| &stamp > r) {
            phone_register = Some(stamp);
            phone_clipboard = c.text.clone();
        }
    }
    for (link, c) in [(&mut pa, &ca), (&mut pb, &cb)] {
        write_frame(
            link,
            &Message::Ack(Ack {
                transfer_id: c.transfer_id.clone(),
                deduplicated: false,
            }),
        )
        .await?;
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if [a.actions.clone(), b.actions.clone()]
                .iter()
                .all(|act| act.text.lock().unwrap().last() == Some(&winner.text))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("computer clipboards did not converge")?;
    assert_eq!(phone_clipboard, winner.text);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let mut complete = true;
            for h in [&a, &b] {
                let db =
                    rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3")).unwrap();
                let register: (String, String) = db
                    .query_row("SELECT hlc,source_device FROM clipboard_clock", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .unwrap();
                complete &= (register.0.parse::<u64>().unwrap(), register.1) == key(winner);
            }
            if complete {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("durable clipboard registers did not converge")?;
    for h in [&a, &b] {
        let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
        let register: (String, String) =
            db.query_row("SELECT hlc,source_device FROM clipboard_clock", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        assert_eq!((register.0.parse::<u64>()?, register.1), key(winner));
        assert!(
            h.actions.receipt_notices.lock().unwrap().is_empty(),
            "computer sync stays silent"
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut pa))
            .await
            .is_err(),
        "no automatic forwarding to phone"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut pb))
            .await
            .is_err()
    );
    drop((pa, pb));
    a.stop().await?;
    b.stop().await
}
#[tokio::test]
async fn mesh_legacy_effect_fences_older_image_and_local_restart_preserves_clock() -> Result<()> {
    let h = Harness::mesh().await?;
    let mesh = h.phone("mesh")?;
    let legacy = h.phone("legacy")?;
    h.pair(&mesh).await?.close().await?;
    let mut legacy_link = h.pair(&legacy).await?;
    let mut m = mesh_phone(&h, &mesh).await?;
    let mut image = metadata(Id::from_bytes(rand::random()), b"\x89PNG\r\n\x1a\nold");
    image.kind = FileKind::ClipboardImage;
    image.hlc = Some(pneud::now_ms());
    image.source_device = Some(mesh.identity.device_id.clone());
    write_frame(&mut m, &Message::FileBegin(image.clone())).await?;
    legacy_link
        .send_text(legacy.text("legacy wins now".into(), ClipboardOrigin::Clipboard)?)
        .await?;
    write_frame(
        &mut m,
        &Message::FileChunk(FileChunk {
            transfer_id: image.transfer_id.clone(),
            offset: 0,
            data: b"\x89PNG\r\n\x1a\nold".to_vec(),
        }),
    )
    .await?;
    write_frame(
        &mut m,
        &Message::FileEnd(FileEnd {
            transfer_id: image.transfer_id,
        }),
    )
    .await?;
    assert!(matches!(wire_reply(&mut m).await?, Message::Ack(_)));
    let offer = h.core.observe_clipboard_offer().unwrap();
    h.core
        .send_observed_clipboard(offer, "new local".into())
        .await?;
    let mesh_out = match wire_reply(&mut m).await? {
        Message::Clipboard(c) => c,
        m => panic!("{m:?}"),
    };
    let legacy_out = legacy_link.read(Duration::from_secs(3)).await?;
    assert!(
        matches!(&legacy_out, Message::Clipboard(c) if c.hlc.is_none() && c.source_device.is_none())
    );
    write_frame(
        &mut m,
        &Message::Ack(Ack {
            transfer_id: mesh_out.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    legacy_link.handle(legacy_out).await?;
    let previous = mesh_out.hlc.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.actions.images.lock().unwrap().is_empty());
    assert_eq!(*h.actions.text.lock().unwrap(), vec!["legacy wins now"]);
    drop((m, legacy_link));
    let paths = h.core.paths.clone();
    let config = h.core.config();
    let actions = h.actions.clone();
    let address = h.address.clone();
    h.core.request_shutdown();
    h.task.await??;
    drop(h.core);
    let core = Core::new(paths, config, actions).await?;
    let listener = TcpListener::from_std(h.listener.try_clone()?)?;
    let task = tokio::spawn(core.clone().serve(listener, false));
    // Same simulator identity, negotiated mesh again after restart.
    let restarted = Harness {
        dir: h.dir,
        core,
        task,
        actions: h.actions,
        address,
        listener: h.listener,
    };
    let mut m = mesh_phone(&restarted, &mesh).await?;
    let offer = restarted.core.observe_clipboard_offer().unwrap();
    restarted
        .core
        .send_observed_clipboard(offer, "after restart".into())
        .await?;
    let after = match wire_reply(&mut m).await? {
        Message::Clipboard(c) => c,
        _ => panic!("clipboard"),
    };
    assert!(after.hlc.unwrap() > previous);
    drop(m);
    restarted.stop().await
}

#[tokio::test]
async fn computer_restart_redials_persisted_pair_and_does_not_replay_old_clipboard() -> Result<()> {
    let a = Harness::mesh().await?;
    let b = Harness::mesh().await?;
    let mut inviter_events = a.core.subscribe();
    link_computers(&a, &b).await?;
    event_matching(&mut inviter_events,|e| matches!(e,pneud::Event::ComputerPaired {device,name} if device==&b.core.identity.device_id && name==&b.core.config().name)).await?;
    let offer = a.core.observe_clipboard_offer().unwrap();
    a.core
        .send_observed_clipboard(offer, "before disconnect".into())
        .await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while b.actions.text.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("initial clipboard did not arrive")?;
    let old_id = b.core.identity.device_id.clone();
    let paths = b.core.paths.clone();
    let config = b.core.config();
    let actions = b.actions.clone();
    let address = b.address.clone();
    b.core.request_shutdown();
    b.task.await??;
    drop(b.core);
    tokio::time::timeout(Duration::from_secs(3), async {
        while a.core.connected_devices().contains(&old_id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("stopped peer stayed connected")?;
    // No computer recipient exists while B is down; observation must not queue
    // a value for that peer. Restart uses the real persisted identity/routes.
    assert!(a.core.observe_clipboard_offer().is_none());
    actions.text.lock().unwrap().clear();
    let core = Core::new(paths, config, actions.clone()).await?;
    assert_eq!(core.identity.device_id, old_id);
    let listener = TcpListener::from_std(b.listener.try_clone()?)?;
    let task = tokio::spawn(core.clone().serve(listener, false));
    let b = Harness {
        dir: b.dir,
        core,
        task,
        actions,
        address,
        listener: b.listener,
    };
    tokio::time::timeout(Duration::from_secs(8), async {
        while a.core.connected_devices().is_empty() || b.core.connected_devices().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("persisted pair did not reconnect")?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        b.actions.text.lock().unwrap().is_empty(),
        "mesh reconnect never replays a local clip"
    );
    let offer = a.core.observe_clipboard_offer().unwrap();
    a.core
        .send_observed_clipboard(offer, "after restart".into())
        .await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while b.actions.text.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("post-restart clipboard did not arrive")?;
    assert_eq!(*b.actions.text.lock().unwrap(), vec!["after restart"]);
    a.stop().await?;
    b.stop().await
}

#[tokio::test]
async fn local_clipboard_allocation_waits_for_an_active_os_effect() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("phone")?;
    let mut c = h.pair(&phone).await?;
    h.actions
        .block_text
        .store(true, std::sync::atomic::Ordering::SeqCst);
    c.write(&Message::Clipboard(
        phone.text("incoming".into(), ClipboardOrigin::Clipboard)?,
    ))
    .await?;
    tokio::time::timeout(Duration::from_secs(2), h.actions.text_entered.notified()).await?;
    let core = h.core.clone();
    let device = phone.identity.device_id.clone();
    let mut local = Box::pin(core.send_clipboard(device, "local after incoming".into()));
    assert!(
        futures_util::poll!(local.as_mut()).is_pending(),
        "local register must serialize with the active OS effect"
    );
    h.actions
        .block_text
        .store(false, std::sync::atomic::Ordering::SeqCst);
    h.actions.text_release.notify_one();
    let id = tokio::time::timeout(Duration::from_secs(10), local).await??;
    let mut got_receipt = false;
    let mut got_outgoing = false;
    for _ in 0..2 {
        match c.read(Duration::from_secs(2)).await? {
            Message::Ack(_) => got_receipt = true,
            m @ Message::Clipboard(_) => {
                assert!(matches!(&m,Message::Clipboard(t) if t.transfer_id==id));
                c.handle(m).await?;
                got_outgoing = true;
            }
            m => panic!("{m:?}"),
        }
    }
    assert!(got_receipt && got_outgoing);
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    assert_eq!(
        db.query_row("SELECT source_device FROM clipboard_clock", [], |r| r
            .get::<_, String>(0))?,
        h.core.identity.device_id.as_str()
    );
    drop(db);
    drop(c);
    h.stop().await
}
#[tokio::test]
async fn mesh_outgoing_image_has_local_stamp_and_unstamped_ordinary_image_is_saved_only()
-> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("mesh")?;
    h.pair(&phone).await?.close().await?;
    let mut m = mesh_phone(&h, &phone).await?;
    let mut config = h.core.config();
    config.clipboard_include_images = true;
    h.core.update_config(config).await?;
    let offer = h.core.observe_clipboard_offer().unwrap();
    let png = b"\x89PNG\r\n\x1a\noutgoing".to_vec();
    let id = h.core.send_observed_image(offer, png.clone()).await?;
    let begin = match wire_reply(&mut m).await? {
        Message::FileBegin(f) => f,
        m => panic!("{m:?}"),
    };
    assert_eq!(begin.transfer_id, id);
    assert_eq!(begin.kind, FileKind::ClipboardImage);
    assert_eq!(
        begin.source_device.as_ref(),
        Some(&h.core.identity.device_id)
    );
    assert!(begin.hlc.is_some());
    assert!(matches!(wire_reply(&mut m).await?,Message::FileChunk(c) if c.data==png));
    assert!(matches!(wire_reply(&mut m).await?, Message::FileEnd(_)));
    write_frame(
        &mut m,
        &Message::Ack(Ack {
            transfer_id: id,
            deduplicated: false,
        }),
    )
    .await?;
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    let clock: (String, i64) =
        db.query_row("SELECT hlc,created_at FROM clipboard_clock", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    assert_eq!(
        clock,
        (begin.hlc.unwrap().to_string(), begin.created_at as i64)
    );
    drop(db);
    let mut incoming = metadata(Id::from_bytes(rand::random()), &png);
    incoming.kind = FileKind::File;
    write_frame(&mut m, &Message::FileBegin(incoming.clone())).await?;
    write_frame(
        &mut m,
        &Message::FileChunk(FileChunk {
            transfer_id: incoming.transfer_id.clone(),
            offset: 0,
            data: png,
        }),
    )
    .await?;
    write_frame(
        &mut m,
        &Message::FileEnd(FileEnd {
            transfer_id: incoming.transfer_id,
        }),
    )
    .await?;
    assert!(matches!(wire_reply(&mut m).await?, Message::Ack(_)));
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.actions.notices.lock().unwrap().is_empty() {
            h.actions.changed.notified().await;
        }
    })
    .await?;
    assert!(h.actions.images.lock().unwrap().is_empty());
    assert_eq!(h.saved()?.len(), 1);
    drop(m);
    h.stop().await
}

#[tokio::test]
async fn clock_exhaustion_prevents_legacy_text_and_image_os_writes() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("legacy")?;
    let mut c = h.pair(&phone).await?;
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    db.execute("UPDATE mesh_clock SET last_seen=?", [u64::MAX.to_string()])?;
    let text = phone.text("must not apply".into(), ClipboardOrigin::Clipboard)?;
    c.write(&Message::Clipboard(text)).await?;
    assert!(matches!(
        c.read(Duration::from_secs(2)).await?,
        Message::Nack(_)
    ));
    assert!(h.actions.text.lock().unwrap().is_empty());
    let png = b"\x89PNG\r\n\x1a\nimage";
    let path = h.dir.path().join("exhausted.png");
    std::fs::write(&path, png)?;
    let mut begin = metadata(Id::from_bytes(rand::random()), png);
    begin.kind = FileKind::ClipboardImage;
    let mut events = h.core.subscribe();
    c.send_file(begin, &path).await?;
    event_matching(
        &mut events,
        |e| matches!(e,pneud::Event::Error {message,..} if message.contains("clock exhausted")),
    )
    .await?;
    assert!(h.actions.images.lock().unwrap().is_empty());
    assert_eq!(
        device_rows(&h, &phone.identity.device_id, "clipboard_recent")?,
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM clipboard_clock", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        db.query_row("SELECT last_seen FROM mesh_clock", [], |r| r
            .get::<_, String>(0))?,
        u64::MAX.to_string()
    );
    drop(db);
    drop(c);
    h.stop().await
}

async fn wait_pair_settings(h: &Harness, peer: &Id, key: &str, enabled: bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let state = h.core.peer_settings(peer.clone()).await?;
            if state["can_sync"] == true
                && state["settings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["key"] == key && f["enabled"] == enabled && f["pending"] == false)
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}
#[tokio::test]
async fn peer_settings_pc_roundtrip_enforces_both_directions_and_offline_reconnect() -> Result<()> {
    let a = Harness::mesh().await?;
    let b = Harness::mesh().await?;
    link_computers(&a, &b).await?;
    let aid = a.core.identity.device_id.clone();
    let bid = b.core.identity.device_id.clone();
    let ab = format!("{aid}/{bid}/clipboard");
    let ba = format!("{bid}/{aid}/clipboard");
    wait_pair_settings(&a, &bid, &ab, true).await?;
    wait_pair_settings(&b, &aid, &ba, true).await?;
    a.core
        .set_peer_setting(bid.clone(), ab.clone(), false)
        .await?;
    wait_pair_settings(&a, &bid, &ab, false).await?;
    wait_pair_settings(&b, &aid, &ab, false).await?;
    assert!(
        a.core
            .send_clipboard(bid.clone(), "blocked".into())
            .await
            .is_err()
    );
    b.core
        .send_clipboard(aid.clone(), "reverse allowed".into())
        .await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while a.actions.text.lock().unwrap().is_empty() {
            a.actions.changed.notified().await;
        }
    })
    .await?;
    let paths = b.core.paths.clone();
    let config = b.core.config();
    let actions = b.actions.clone();
    let address = b.address.clone();
    b.core.request_shutdown();
    b.task.await??;
    drop(b.core);
    tokio::time::timeout(Duration::from_secs(3), async {
        while a.core.connected_devices().contains(&bid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let status = a
        .core
        .set_peer_setting(bid.clone(), ba.clone(), false)
        .await?;
    let changed = status["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == ba)
        .unwrap();
    assert_eq!(changed["pending"], true);
    assert!(changed["applies_when"].is_string());
    let core = Core::new(paths, config, actions.clone()).await?;
    let listener = TcpListener::from_std(b.listener.try_clone()?)?;
    let task = tokio::spawn(core.clone().serve(listener, false));
    let b = Harness {
        dir: b.dir,
        core,
        task,
        actions,
        address,
        listener: b.listener,
    };
    wait_pair_settings(&a, &bid, &ba, false).await?;
    wait_pair_settings(&b, &aid, &ba, false).await?;
    assert!(
        b.core
            .send_clipboard(aid.clone(), "also blocked".into())
            .await
            .is_err()
    );
    // Re-enable one direction; the other direction remains disabled.
    b.core
        .set_peer_setting(aid.clone(), ab.clone(), true)
        .await?;
    wait_pair_settings(&a, &bid, &ab, true).await?;
    a.core.send_clipboard(bid, "enabled again".into()).await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while b.actions.text.lock().unwrap().is_empty() {
            b.actions.changed.notified().await;
        }
    })
    .await?;
    assert_eq!(*b.actions.text.lock().unwrap(), vec!["enabled again"]);
    a.stop().await?;
    b.stop().await
}

#[tokio::test]
async fn peer_settings_receiver_rejects_disabled_text_and_drains_automatic_media() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("settings-phone")?;
    h.pair(&phone).await?.close().await?;
    let mut tls = mesh_phone(&h, &phone).await?;
    let peer = phone.identity.device_id.clone();
    let local = h.core.identity.device_id.clone();
    let key = format!("{peer}/{local}/clipboard");
    h.core.set_peer_setting(peer.clone(), key, false).await?;
    let Message::PeerSettings(snapshot) = wire_reply(&mut tls).await? else {
        panic!("settings update")
    };
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: snapshot.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    let id = Id::from_bytes(rand::random());
    write_frame(
        &mut tls,
        &Message::Clipboard(Clipboard {
            sensitive: false,
            transfer_id: id.clone(),
            text: "malicious disabled send".into(),
            created_at: 1,
            hlc: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_millis() as u64,
            ),
            source_device: Some(peer.clone()),
            origin: ClipboardOrigin::Clipboard,
        }),
    )
    .await?;
    assert!(matches!(
        wire_reply(&mut tls).await?,
        Message::Nack(Nack {
            code: NackCode::UnsupportedKind,
            ..
        })
    ));
    assert!(h.actions.text.lock().unwrap().is_empty());
    h.core
        .set_peer_setting(
            peer.clone(),
            format!("{peer}/{local}/automatic_media"),
            false,
        )
        .await?;
    let Message::PeerSettings(snapshot) = wire_reply(&mut tls).await? else {
        panic!("settings update")
    };
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: snapshot.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    let bytes = b"not saved";
    let begin = FileBegin {
        resume: None,
        folder: None,
        sensitive: false,
        transfer_id: Id::from_bytes(rand::random()),
        file_name: "photo.png".into(),
        mime: "image/png".into(),
        size: bytes.len() as u64,
        sha256: Hash::new(hex::encode(Sha256::digest(bytes)))?,
        kind: FileKind::Photo,
        created_at: 1,
        hlc: None,
        source_device: None,
        source_media_id: None,
    };
    for frame in [
        Message::FileBegin(begin.clone()),
        Message::FileChunk(FileChunk {
            transfer_id: begin.transfer_id.clone(),
            offset: 0,
            data: bytes.to_vec(),
        }),
        Message::FileEnd(FileEnd {
            transfer_id: begin.transfer_id,
        }),
    ] {
        write_frame(&mut tls, &frame).await?;
    }
    assert!(matches!(
        wire_reply(&mut tls).await?,
        Message::Nack(Nack {
            code: NackCode::UnsupportedKind,
            ..
        })
    ));
    assert!(!h.core.config().save_path.exists() || h.saved()?.is_empty());
    let ping = Ping {
        nonce: Nonce::new("0000000000000001")?,
    };
    write_frame(&mut tls, &Message::Ping(ping.clone())).await?;
    assert_eq!(wire_reply(&mut tls).await?, Message::Pong(ping));
    drop(tls);
    h.stop().await
}

#[tokio::test]
async fn peer_settings_foreign_pair_is_fatal_and_legacy_peer_is_unaffected() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("settings-phone")?;
    let mut legacy = h.pair(&phone).await?;
    let state = h
        .core
        .peer_settings(phone.identity.device_id.clone())
        .await?;
    assert_eq!(state["can_sync"], false);
    assert!(
        h.core
            .set_peer_setting(phone.identity.device_id.clone(), "invalid".into(), false)
            .await
            .is_err()
    );
    legacy.close().await?;
    let mut tls = mesh_phone(&h, &phone).await?;
    let local = h.core.identity.device_id.clone();
    h.core
        .set_peer_setting(
            phone.identity.device_id.clone(),
            format!("{}/{local}/clipboard", phone.identity.device_id),
            false,
        )
        .await?;
    let Message::PeerSettings(update) = wire_reply(&mut tls).await? else {
        panic!("settings update")
    };
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: update.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    let foreign = Id::from_bytes([255; 16]);
    let writer = local.clone();
    let snapshot = PeerSettings {
        transfer_id: Id::from_bytes(rand::random()),
        settings: vec![
            PeerSetting {
                key: format!("{local}/{foreign}/clipboard"),
                enabled: false,
                hlc: 1,
                writer_device: writer.clone(),
            },
            PeerSetting {
                key: format!("{foreign}/{local}/clipboard"),
                enabled: false,
                hlc: 1,
                writer_device: writer,
            },
        ],
    };
    write_frame(&mut tls, &Message::PeerSettings(snapshot)).await?;
    assert!(matches!(
        wire_reply(&mut tls).await?,
        Message::Error(ErrorMessage {
            code: ErrorCode::UnexpectedMessage,
            ..
        })
    ));
    drop(tls);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut legacy = phone
        .connect(
            std::slice::from_ref(&h.address),
            h.core.identity.fingerprint.clone(),
            Some(&h.core.identity.device_id),
        )
        .await?;
    legacy
        .send_text(phone.text("legacy still works".into(), ClipboardOrigin::Clipboard)?)
        .await?;
    // The legacy client gets no PEER_SETTINGS, regardless of retained mesh fields.
    assert_eq!(*h.actions.text.lock().unwrap(), vec!["legacy still works"]);
    legacy.close().await?;
    h.stop().await
}

#[tokio::test]
async fn duplicate_mesh_replacement_retries_unacknowledged_clipboard_with_exact_id_stamp_payload()
-> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("duplicate-settings")?;
    h.pair(&phone).await?.close().await?;
    let mut first = mesh_phone(&h, &phone).await?;
    h.core
        .send_clipboard(phone.identity.device_id.clone(), "retain me".into())
        .await?;
    let original = wire_reply(&mut first).await?;
    assert!(matches!(&original, Message::Clipboard(_)));
    // Do not ACK: the duplicate wins registration while the operation is pending.
    let mut replacement = mesh_phone(&h, &phone).await?;
    let retry = wire_reply(&mut replacement).await?;
    assert_eq!(original, retry);
    let Message::Clipboard(text) = retry else {
        unreachable!()
    };
    let mut events = h.core.subscribe();
    let acknowledged = text.transfer_id.clone();
    write_frame(
        &mut replacement,
        &Message::Ack(Ack {
            transfer_id: text.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    event_matching(&mut events, |event| {
        matches!(event, pneud::Event::ClipboardSent { transfer_id, .. } if *transfer_id == acknowledged)
    })
    .await?;
    let mut final_link = mesh_phone(&h, &phone).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(150), read_frame(&mut final_link))
            .await
            .is_err(),
        "acknowledged work was replayed"
    );
    drop(first);
    drop(replacement);
    drop(final_link);
    h.stop().await
}

#[tokio::test]
async fn duplicate_mesh_replacement_preserves_image_frames_and_completed_deferred_effect()
-> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("duplicate-image")?;
    h.pair(&phone).await?.close().await?;
    let mut config = h.core.config();
    config.clipboard_include_images = true;
    h.core.update_config(config).await?;
    let mut first = mesh_phone(&h, &phone).await?;
    let png = b"\x89PNG\r\n\x1a\nclipboard fixture".to_vec();
    h.core
        .send_clipboard_image(phone.identity.device_id.clone(), png)
        .await?;
    let mut original = Vec::new();
    loop {
        let frame = wire_reply(&mut first).await?;
        let end = matches!(frame, Message::FileEnd(_));
        original.push(frame);
        if end {
            break;
        }
    }
    let mut replacement = mesh_phone(&h, &phone).await?;
    let mut retry = Vec::new();
    loop {
        let frame = wire_reply(&mut replacement).await?;
        let end = matches!(frame, Message::FileEnd(_));
        retry.push(frame);
        if end {
            break;
        }
    }
    assert_eq!(
        original, retry,
        "replacement changed transfer id, stamp, bytes or framing"
    );
    let Message::FileEnd(end) = retry.last().unwrap() else {
        unreachable!()
    };
    write_frame(
        &mut replacement,
        &Message::Ack(Ack {
            transfer_id: end.transfer_id.clone(),
            deduplicated: false,
        }),
    )
    .await?;
    // A completed incoming operation can have its OS effect delayed beyond its
    // ACK. Duplicate replacement must retain that effect's captured authority.
    h.actions
        .block_image
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let bytes = b"\x89PNG\r\n\x1a\nincoming";
    let image = FileBegin {
        resume: None,
        folder: None,
        sensitive: false,
        transfer_id: Id::from_bytes(rand::random()),
        file_name: "clipboard.png".into(),
        mime: "image/png".into(),
        size: bytes.len() as u64,
        sha256: Hash::new(hex::encode(Sha256::digest(bytes)))?,
        kind: FileKind::ClipboardImage,
        created_at: 1,
        hlc: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as u64
                + 100,
        ),
        source_device: Some(phone.identity.device_id.clone()),
        source_media_id: None,
    };
    for frame in [
        Message::FileBegin(image.clone()),
        Message::FileChunk(FileChunk {
            transfer_id: image.transfer_id.clone(),
            offset: 0,
            data: bytes.to_vec(),
        }),
        Message::FileEnd(FileEnd {
            transfer_id: image.transfer_id.clone(),
        }),
    ] {
        write_frame(&mut replacement, &frame).await?;
    }
    tokio::time::timeout(Duration::from_secs(3), h.actions.image_entered.notified()).await?;
    assert!(matches!(
        wire_reply(&mut replacement).await?,
        Message::Ack(_)
    ));
    let final_link = mesh_phone(&h, &phone).await?;
    h.actions.image_release.notify_one();
    h.wait_images(1).await?;
    assert_eq!(*h.actions.image_bytes.lock().unwrap(), vec![bytes.to_vec()]);
    drop(first);
    drop(replacement);
    drop(final_link);
    h.stop().await
}

#[tokio::test]
async fn peer_settings_storage_failure_is_atomic_and_keeps_control_alive_for_exact_retry()
-> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("settings-failure")?;
    h.pair(&phone).await?.close().await?;
    let mut tls = mesh_phone(&h, &phone).await?;
    let peer = phone.identity.device_id.clone();
    let state = h.core.peer_settings(peer.clone()).await?;
    let mut fields: Vec<PeerSetting> = state["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| serde_json::from_value(f.clone()))
        .collect::<serde_json::Result<_>>()?;
    for f in &mut fields {
        f.enabled = false;
        f.hlc = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        f.writer_device = peer.clone();
    }
    let snapshot = PeerSettings {
        transfer_id: Id::from_bytes(rand::random()),
        settings: fields,
    };
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    db.execute_batch("CREATE TRIGGER fail_settings BEFORE INSERT ON settings_receipts BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;")?;
    write_frame(&mut tls, &Message::PeerSettings(snapshot.clone())).await?;
    assert!(matches!(
        wire_reply(&mut tls).await?,
        Message::Nack(Nack {
            code: NackCode::StorageFailed,
            ..
        })
    ));
    let after = h.core.peer_settings(peer.clone()).await?;
    assert!(
        after["settings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["enabled"] == true)
    );
    let ping = Ping {
        nonce: Nonce::new("1234567890abcdef")?,
    };
    write_frame(&mut tls, &Message::Ping(ping.clone())).await?;
    assert_eq!(wire_reply(&mut tls).await?, Message::Pong(ping));
    db.execute_batch("DROP TRIGGER fail_settings;")?;
    write_frame(&mut tls, &Message::PeerSettings(snapshot.clone())).await?;
    assert!(matches!(
        wire_reply(&mut tls).await?,
        Message::Ack(Ack {
            deduplicated: false,
            ..
        })
    ));
    // PC sends the resulting full state so each field's pending flag can clear.
    if let Message::PeerSettings(update) = wire_reply(&mut tls).await? {
        write_frame(
            &mut tls,
            &Message::Ack(Ack {
                transfer_id: update.transfer_id,
                deduplicated: false,
            }),
        )
        .await?;
    } else {
        panic!("missing merged snapshot");
    }
    write_frame(&mut tls, &Message::PeerSettings(snapshot)).await?;
    assert!(matches!(
        wire_reply(&mut tls).await?,
        Message::Ack(Ack {
            deduplicated: true,
            ..
        })
    ));
    assert!(
        h.core.peer_settings(peer).await?["settings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["enabled"] == false)
    );
    drop(tls);
    h.stop().await
}

#[tokio::test]
async fn peer_settings_disable_revalidates_already_prepared_clipboard_image_at_effect_boundary()
-> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("deferred-policy")?;
    h.pair(&phone).await?.close().await?;
    let mut tls = mesh_phone(&h, &phone).await?;
    h.actions
        .block_image
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let peer = phone.identity.device_id.clone();
    let local = h.core.identity.device_id.clone();
    let bytes = b"\x89PNG\r\n\x1a\nclipboard image";
    let image = FileBegin {
        resume: None,
        folder: None,
        sensitive: false,
        transfer_id: Id::from_bytes(rand::random()),
        file_name: "clipboard.png".into(),
        mime: "image/png".into(),
        size: bytes.len() as u64,
        sha256: Hash::new(hex::encode(Sha256::digest(bytes)))?,
        kind: FileKind::ClipboardImage,
        created_at: 1,
        hlc: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as u64,
        ),
        source_device: Some(peer.clone()),
        source_media_id: None,
    };
    for frame in [
        Message::FileBegin(image.clone()),
        Message::FileChunk(FileChunk {
            transfer_id: image.transfer_id.clone(),
            offset: 0,
            data: bytes.to_vec(),
        }),
        Message::FileEnd(FileEnd {
            transfer_id: image.transfer_id.clone(),
        }),
    ] {
        write_frame(&mut tls, &frame).await?;
    }
    tokio::time::timeout(Duration::from_secs(3), h.actions.image_entered.notified()).await?;
    assert!(matches!(wire_reply(&mut tls).await?, Message::Ack(_)));
    h.core
        .set_peer_setting(
            peer,
            format!("{}/{local}/clipboard", phone.identity.device_id),
            false,
        )
        .await?;
    let Message::PeerSettings(update) = wire_reply(&mut tls).await? else {
        panic!("settings update")
    };
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: update.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    h.actions.image_release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), h.actions.image_finished.notified()).await?;
    assert!(
        h.actions.images.lock().unwrap().is_empty(),
        "a disabled direction retained a pre-await authorization"
    );
    drop(tls);
    h.stop().await
}

#[tokio::test]
async fn explicit_clipboard_supersedes_slow_watcher_while_waiting_for_receipt() -> Result<()> {
    for mesh in [false, true] {
        for image in [false, true] {
            let h = Harness::mesh().await?;
            let phone = h.phone("manual-supersession")?;
            h.pair(&phone).await?.close().await?;
            h.core
                .set_setting("clipboard_include_images".into(), true.into())
                .await?;
            let mut link = negotiated_phone(&h, &phone, mesh, true).await?;
            let peer = phone.identity.device_id.clone();
            let first = h
                .core
                .send_clipboard(peer.clone(), "awaiting receipt".into())
                .await?;
            assert!(
                matches!(wire_reply(&mut link).await?,Message::Clipboard(c) if c.transfer_id==first)
            );
            let reading = h.core.observe_clipboard_offer().unwrap();
            let latest = if image {
                h.core
                    .send_clipboard_image(peer.clone(), b"\x89PNG\r\n\x1a\nexplicit".to_vec())
                    .await?
            } else {
                h.core
                    .send_clipboard(peer.clone(), "latest explicit".into())
                    .await?
            };
            assert!(
                h.core
                    .send_observed_clipboard(reading.clone(), "stale watcher".into())
                    .await
                    .is_err()
            );
            assert!(
                h.core
                    .send_observed_image(reading, b"\x89PNG\r\n\x1a\nstale".to_vec())
                    .await
                    .is_err()
            );
            write_frame(
                &mut link,
                &Message::Ack(Ack {
                    transfer_id: first,
                    deduplicated: false,
                }),
            )
            .await?;
            if image {
                assert!(
                    matches!(wire_reply(&mut link).await?,Message::FileBegin(b) if b.transfer_id==latest && b.hlc.is_some()==mesh)
                );
                assert!(
                    matches!(wire_reply(&mut link).await?,Message::FileChunk(c) if c.transfer_id==latest)
                );
                assert!(
                    matches!(wire_reply(&mut link).await?,Message::FileEnd(e) if e.transfer_id==latest)
                );
            } else {
                assert!(
                    matches!(wire_reply(&mut link).await?,Message::Clipboard(c) if c.transfer_id==latest && c.text=="latest explicit" && c.hlc.is_some()==mesh)
                );
            }
            write_frame(
                &mut link,
                &Message::Ack(Ack {
                    transfer_id: latest,
                    deduplicated: false,
                }),
            )
            .await?;
            let ping = Ping {
                nonce: Nonce::new("01".repeat(8))?,
            };
            write_frame(&mut link, &Message::Ping(ping.clone())).await?;
            assert_eq!(wire_reply(&mut link).await?, Message::Pong(ping));
            drop(link);
            h.stop().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn mesh_upgrade_discards_legacy_outbox_before_settings_and_downgrade_starts_fresh()
-> Result<()> {
    for text_capability in [false, true] {
        let h = Harness::mesh().await?;
        let phone = h.phone("transition-phone")?;
        let mut legacy = h.pair(&phone).await?;
        let device = phone.identity.device_id.clone();
        h.core
            .send_clipboard(device.clone(), "durable legacy".into())
            .await?;
        assert!(matches!(
            legacy.read(Duration::from_secs(3)).await?,
            Message::Clipboard(_)
        ));
        // Reproduce an unstamped released row, including a future retry.
        let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
        db.execute("UPDATE outgoing SET payload=json_remove(payload, '$.hlc', '$.source_device'),retry_at=? WHERE device=?",
            rusqlite::params![(pneud::now_ms()+60_000) as i64,device.as_str()])?;
        legacy.close().await?;
        let mesh = hello_phone(&h, &phone, true, text_capability).await?;
        // Do not exchange settings: retirement must not depend on content-ready.
        tokio::time::timeout(Duration::from_secs(3), async {
            while device_rows(&h, &device, "outgoing").unwrap() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        drop(mesh);
        let mut downgrade = negotiated_phone(&h, &phone, false, true).await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), read_frame(&mut downgrade))
                .await
                .is_err()
        );
        let fresh = h
            .core
            .send_clipboard(device.clone(), "fresh legacy".into())
            .await?;
        assert!(
            matches!(wire_reply(&mut downgrade).await?,Message::Clipboard(c) if c.transfer_id==fresh && c.hlc.is_none() && c.text=="fresh legacy")
        );
        drop(downgrade);
        let mut retry = negotiated_phone(&h, &phone, false, true).await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(150), read_frame(&mut retry))
                .await
                .is_err(),
            "disconnected legacy clipboard must not replay"
        );
        drop(retry);
        h.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn mesh_to_legacy_downgrade_drops_in_memory_clipboard_selections() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("downgrade-phone")?;
    h.pair(&phone).await?.close().await?;
    let mut mesh = mesh_phone(&h, &phone).await?;
    let peer = phone.identity.device_id.clone();
    h.core
        .send_clipboard(peer.clone(), "in flight mesh".into())
        .await?;
    assert!(matches!(
        wire_reply(&mut mesh).await?,
        Message::Clipboard(_)
    ));
    h.core
        .send_clipboard(peer.clone(), "pending mesh".into())
        .await?;
    assert_eq!(device_rows(&h, &peer, "outgoing")?, 0);
    // Replace the mesh connection directly while it waits for the earlier ACK.
    let mut legacy = negotiated_phone(&h, &phone, false, true).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut legacy))
            .await
            .is_err()
    );
    let id = h
        .core
        .send_clipboard(peer.clone(), "new legacy".into())
        .await?;
    assert!(
        matches!(wire_reply(&mut legacy).await?,Message::Clipboard(c) if c.transfer_id==id && c.hlc.is_none())
    );
    assert_eq!(device_rows(&h, &peer, "outgoing")?, 0);
    drop(mesh);
    drop(legacy);
    h.stop().await
}

#[tokio::test]
async fn retained_settings_snapshot_resumes_after_legacy_capability_loss() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("settings-transition")?;
    h.pair(&phone).await?.close().await?;
    let mut mesh = mesh_phone(&h, &phone).await?;
    let peer = phone.identity.device_id.clone();
    h.core
        .set_peer_setting(
            peer.clone(),
            format!("{}/{peer}/clipboard", h.core.identity.device_id),
            false,
        )
        .await?;
    let Message::PeerSettings(pending) = wire_reply(&mut mesh).await? else {
        panic!("settings edit");
    };
    drop(mesh);
    let mut legacy = negotiated_phone(&h, &phone, false, true).await?;
    let ping = Message::Ping(Ping {
        nonce: Nonce::new("01".repeat(8))?,
    });
    write_frame(&mut legacy, &ping).await?;
    assert!(matches!(wire_reply(&mut legacy).await?, Message::Pong(_)));
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    let payload: String = db.query_row(
        "SELECT payload FROM settings_outgoing WHERE device=?",
        [peer.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(serde_json::from_str::<PeerSettings>(&payload)?, pending);
    drop(legacy);
    let mut mesh = hello_phone(&h, &phone, true, true).await?;
    assert_eq!(
        wire_reply(&mut mesh).await?,
        Message::PeerSettings(pending.clone())
    );
    write_frame(
        &mut mesh,
        &Message::Ack(Ack {
            transfer_id: pending.transfer_id,
            deduplicated: true,
        }),
    )
    .await?;
    write_frame(&mut mesh, &ping).await?;
    assert!(matches!(wire_reply(&mut mesh).await?, Message::Pong(_)));
    // PONG uses the priority control lane and is not a persistence barrier for
    // the preceding application ACK. Wait for that ACK's durable effect.
    tokio::time::timeout(Duration::from_secs(5), async {
        while device_rows(&h, &peer, "settings_outgoing")? != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    drop(mesh);
    h.stop().await
}
async fn raw_file_reply(link: &mut PhoneTls, meta: &FileBegin, bytes: &[u8]) -> Result<Message> {
    write_frame(link, &Message::FileBegin(meta.clone())).await?;
    write_frame(
        link,
        &Message::FileChunk(FileChunk {
            transfer_id: meta.transfer_id.clone(),
            offset: 0,
            data: bytes.to_vec(),
        }),
    )
    .await?;
    write_frame(
        link,
        &Message::FileEnd(FileEnd {
            transfer_id: meta.transfer_id.clone(),
        }),
    )
    .await?;
    wire_reply(link).await
}
#[tokio::test]
async fn receipts_and_failed_file_cache_survive_mesh_capability_transitions() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("file-transition")?;
    let mut legacy = h.pair(&phone).await?;
    let peer = phone.identity.device_id.clone();
    let saved = metadata(Id::from_bytes(rand::random()), b"saved");
    begin_chunks(&mut legacy, &saved, b"saved").await?;
    assert!(matches!(
        end(&mut legacy, saved.transfer_id.clone()).await?,
        Message::Ack(_)
    ));
    let blocker = h.dir.path().join("blocked");
    std::fs::write(&blocker, b"keep")?;
    let mut config = h.core.config();
    config.save_path = blocker.join("child");
    h.core.update_config(config).await?;
    let failed = metadata(Id::from_bytes(rand::random()), b"retry");
    begin_chunks(&mut legacy, &failed, b"retry").await?;
    assert!(
        matches!(end(&mut legacy,failed.transfer_id.clone()).await?,Message::Nack(n) if n.code==NackCode::NotWritable)
    );
    legacy.close().await?;
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    let retained: String = db.query_row(
        "SELECT meta FROM receipts WHERE device=? AND id=?",
        rusqlite::params![peer.as_str(), saved.transfer_id.as_str()],
        |r| r.get(0),
    )?;
    for (mesh, text) in [(true, false), (false, false), (true, true)] {
        let mut link = negotiated_phone(&h, &phone, mesh, text).await?;
        assert!(
            matches!(raw_file_reply(&mut link,&saved,b"saved").await?,Message::Ack(a) if a.deduplicated)
        );
        assert!(
            matches!(raw_file_reply(&mut link,&failed,b"retry").await?,Message::Nack(n) if n.code==NackCode::NotWritable)
        );
        assert_eq!(device_rows(&h, &peer, "failed_cache")?, 1);
        let current: String = db.query_row(
            "SELECT meta FROM receipts WHERE device=? AND id=?",
            rusqlite::params![peer.as_str(), saved.transfer_id.as_str()],
            |r| r.get(0),
        )?;
        assert_eq!(current, retained);
        drop(link);
    }
    let mut config = h.core.config();
    config.save_path = h.dir.path().join("recovered");
    h.core.update_config(config).await?;
    let mut link = mesh_phone(&h, &phone).await?;
    assert!(
        matches!(raw_file_reply(&mut link,&failed,b"retry").await?,Message::Ack(a) if !a.deduplicated)
    );
    assert_eq!(device_rows(&h, &peer, "failed_cache")?, 0);
    assert_eq!(device_rows(&h, &peer, "receipts")?, 2);
    assert_eq!(std::fs::read(h.saved()?[0].clone())?, b"retry");
    drop(link);
    h.stop().await
}
#[tokio::test]
async fn mesh_upgrade_discards_legacy_text_that_would_exceed_mesh_frame_size() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("size-transition")?;
    let peer = phone.identity.device_id.clone();
    let mut legacy = h.pair(&phone).await?;
    let mut text = Clipboard {
        sensitive: false,
        transfer_id: Id::from_bytes([0; 16]),
        text: "\u{1}".repeat(10_800),
        created_at: pneud::now_ms(),
        origin: ClipboardOrigin::Clipboard,
        hlc: None,
        source_device: None,
    };
    let remaining = MAX_CONTROL_PAYLOAD - serde_json::to_vec(&text)?.len();
    text.text.push_str(&"x".repeat(remaining));
    h.core.send_clipboard(peer.clone(), text.text).await?;
    assert!(matches!(
        legacy.read(Duration::from_secs(3)).await?,
        Message::Clipboard(_)
    ));
    legacy.close().await?;
    let mut link = mesh_phone(&h, &phone).await?;
    write_frame(
        &mut link,
        &Message::Ping(Ping {
            nonce: Nonce::new("01".repeat(8))?,
        }),
    )
    .await?;
    assert!(matches!(wire_reply(&mut link).await?, Message::Pong(_)));
    tokio::time::timeout(Duration::from_secs(3), async {
        while device_rows(&h, &peer, "outgoing").unwrap() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(h.core.connected_devices(), vec![peer]);
    drop(link);
    h.stop().await
}

#[tokio::test]
async fn persisted_disabled_startup_policy_excludes_offers_before_settings_init() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("startup-policy")?;
    h.pair(&phone).await?.close().await?;
    let mut tls = mesh_phone(&h, &phone).await?;
    let peer = phone.identity.device_id.clone();
    let key = format!("{}/{peer}/clipboard", h.core.identity.device_id);
    h.core
        .set_peer_setting(peer.clone(), key.clone(), false)
        .await?;
    let Message::PeerSettings(off) = wire_reply(&mut tls).await? else {
        panic!("off")
    };
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: off.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    drop(tls);
    let paths = h.core.paths.clone();
    let config = h.core.config();
    let actions = h.actions.clone();
    let address = h.address.clone();
    h.core.request_shutdown();
    h.task.await??;
    drop(h.core);
    let core = Core::new(paths, config, actions).await?;
    let listener = TcpListener::from_std(h.listener.try_clone()?)?;
    let task = tokio::spawn(core.clone().serve(listener, false));
    let h = Harness {
        dir: h.dir,
        core,
        task,
        actions: h.actions,
        address,
        listener: h.listener,
    };
    // Hold the new registration before settings cache initialization.
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    let mut tls = hello_phone(&h, &phone, true, true).await?;
    let offer = h.core.observe_clipboard_offer();
    assert!(
        offer.is_none(),
        "persisted disabled direction admitted before settings init"
    );
    db.execute_batch("COMMIT")?;
    let Message::PeerSettings(off) = wire_reply(&mut tls).await? else {
        panic!("startup snapshot")
    };
    assert!(!off.settings.iter().find(|f| f.key == key).unwrap().enabled);
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: off.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    let response = PeerSettings {
        transfer_id: Id::from_bytes(rand::random()),
        settings: off.settings,
    };
    write_frame(&mut tls, &Message::PeerSettings(response)).await?;
    assert!(matches!(wire_reply(&mut tls).await?, Message::Ack(_)));
    h.core.set_peer_setting(peer.clone(), key, true).await?;
    let Message::PeerSettings(on) = wire_reply(&mut tls).await? else {
        panic!("on")
    };
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: on.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    assert!(
        h.core.observe_clipboard_offer().is_some(),
        "enabled policy admits new offers"
    );
    drop(tls);
    h.stop().await
}
#[tokio::test]
async fn automatic_media_disable_suppresses_deferred_media_clipboard() -> Result<()> {
    for kind in [FileKind::Screenshot, FileKind::Photo, FileKind::Recording] {
        let h = Harness::mesh().await?;
        let phone = h.phone("deferred-automatic")?;
        h.pair(&phone).await?.close().await?;
        let mut tls = mesh_phone(&h, &phone).await?;
        h.actions
            .block_image
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let peer = phone.identity.device_id.clone();
        let bytes = b"\x89PNG\r\n\x1a\nphoto";
        let mut image = metadata(Id::from_bytes(rand::random()), bytes);
        image.kind = kind;
        image.hlc = Some(pneud::now_ms());
        image.source_device = Some(peer.clone());
        for frame in [
            Message::FileBegin(image.clone()),
            Message::FileChunk(FileChunk {
                transfer_id: image.transfer_id.clone(),
                offset: 0,
                data: bytes.to_vec(),
            }),
            Message::FileEnd(FileEnd {
                transfer_id: image.transfer_id.clone(),
            }),
        ] {
            write_frame(&mut tls, &frame).await?;
        }
        tokio::time::timeout(Duration::from_secs(3), h.actions.image_entered.notified()).await?;
        assert!(matches!(wire_reply(&mut tls).await?, Message::Ack(_)));
        h.core
            .set_peer_setting(
                peer.clone(),
                format!("{peer}/{}/automatic_media", h.core.identity.device_id),
                false,
            )
            .await?;
        let Message::PeerSettings(off) = wire_reply(&mut tls).await? else {
            panic!("off")
        };
        write_frame(
            &mut tls,
            &Message::Ack(Ack {
                transfer_id: off.transfer_id,
                deduplicated: false,
            }),
        )
        .await?;
        h.actions.image_release.notify_one();
        // A later explicit image drains the same serialized worker, proving the
        // earlier screenshot boundary finished without copying it.
        let mut next = image.clone();
        next.transfer_id = Id::from_bytes(rand::random());
        next.kind = FileKind::ClipboardImage;
        next.hlc = Some(image.hlc.unwrap() + 1);
        for frame in [
            Message::FileBegin(next.clone()),
            Message::FileChunk(FileChunk {
                transfer_id: next.transfer_id.clone(),
                offset: 0,
                data: bytes.to_vec(),
            }),
            Message::FileEnd(FileEnd {
                transfer_id: next.transfer_id.clone(),
            }),
        ] {
            write_frame(&mut tls, &frame).await?;
        }
        h.actions.image_release.notify_one();
        assert!(matches!(wire_reply(&mut tls).await?, Message::Ack(_)));
        h.wait_images(1).await?;
        assert_eq!(h.actions.images.lock().unwrap().len(), 1);
        let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
        let destination: String = db.query_row(
            "SELECT destination FROM receipts WHERE device=? AND id=?",
            rusqlite::params![peer.as_str(), image.transfer_id.as_str()],
            |r| r.get(0),
        )?;
        assert!(
            std::path::Path::new(&destination).is_file(),
            "saved screenshot survives disabled clipboard effect"
        );
        drop(tls);
        h.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn rejected_mesh_ipc_preserves_clock_and_valid_deferred_text() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("rejected-share")?;
    h.pair(&phone).await?.close().await?;
    let mut tls = mesh_phone(&h, &phone).await?;
    let peer = phone.identity.device_id.clone();
    h.actions
        .block_text
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let valid = Clipboard {
        sensitive: false,
        transfer_id: Id::from_bytes(rand::random()),
        text: "valid incoming clipboard".into(),
        created_at: 1,
        origin: ClipboardOrigin::Clipboard,
        hlc: Some(pneud::now_ms()),
        source_device: Some(peer.clone()),
    };
    write_frame(&mut tls, &Message::Clipboard(valid.clone())).await?;
    tokio::time::timeout(Duration::from_secs(3), h.actions.text_entered.notified()).await?;
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    let before: i64 = db.query_row("SELECT count(*) FROM clipboard_clock", [], |r| r.get(0))?;
    assert_eq!(before, 0);
    let clock_before: (i64, String) = db.query_row(
        "SELECT last_outgoing_at,last_seen FROM local_clock,mesh_clock",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert!(
        h.core
            .send_clipboard(peer.clone(), "\u{1}".repeat(11000))
            .await
            .is_err()
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM clipboard_clock", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    let clock_after: (i64, String) = db.query_row(
        "SELECT last_outgoing_at,last_seen FROM local_clock,mesh_clock",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(clock_after, clock_before);
    h.actions.text_release.notify_one();
    assert!(
        matches!(wire_reply(&mut tls).await?,Message::Ack(a) if a.transfer_id==valid.transfer_id && !a.deduplicated)
    );
    assert_eq!(*h.actions.text.lock().unwrap(), vec![valid.text]);

    drop(tls);
    h.stop().await
}

#[tokio::test]
async fn retained_mesh_text_receipt_precedes_disabled_direction_policy() -> Result<()> {
    let h = Harness::mesh().await?;
    let phone = h.phone("receipt-policy")?;
    h.pair(&phone).await?.close().await?;
    let mut tls = mesh_phone(&h, &phone).await?;
    let peer = phone.identity.device_id.clone();
    let text = Clipboard {
        sensitive: false,
        transfer_id: Id::from_bytes(rand::random()),
        text: "accepted before disabling".into(),
        created_at: 1,
        origin: ClipboardOrigin::Clipboard,
        hlc: Some(pneud::now_ms()),
        source_device: Some(peer.clone()),
    };
    write_frame(&mut tls, &Message::Clipboard(text.clone())).await?;
    assert!(
        matches!(wire_reply(&mut tls).await?, Message::Ack(a) if a.transfer_id == text.transfer_id && !a.deduplicated)
    );
    h.core
        .set_peer_setting(
            peer.clone(),
            format!("{peer}/{}/clipboard", h.core.identity.device_id),
            false,
        )
        .await?;
    let Message::PeerSettings(off) = wire_reply(&mut tls).await? else {
        panic!("settings update")
    };
    write_frame(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: off.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    write_frame(&mut tls, &Message::Clipboard(text.clone())).await?;
    assert!(
        matches!(wire_reply(&mut tls).await?, Message::Ack(a) if a.transfer_id == text.transfer_id && a.deduplicated)
    );
    let mut conflict = text.clone();
    conflict.text = "different retained metadata".into();
    write_frame(&mut tls, &Message::Clipboard(conflict)).await?;
    assert!(
        matches!(wire_reply(&mut tls).await?, Message::Nack(n) if n.code == NackCode::IdConflict)
    );
    let mut fresh = text.clone();
    fresh.transfer_id = Id::from_bytes(rand::random());
    write_frame(&mut tls, &Message::Clipboard(fresh)).await?;
    assert!(
        matches!(wire_reply(&mut tls).await?, Message::Nack(n) if n.code == NackCode::UnsupportedKind)
    );
    assert_eq!(*h.actions.text.lock().unwrap(), vec![text.text]);
    drop(tls);
    h.stop().await
}

#[tokio::test]
async fn probe_authorized_legacy_install_after_reconnect() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("old-authorized")?;
    let mut old = h.pair(&phone).await?;
    h.actions
        .block_install
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let text = phone.text("stale legacy effect".into(), ClipboardOrigin::Clipboard)?;
    old.write(&Message::Clipboard(text)).await?;
    tokio::time::timeout(Duration::from_secs(3), h.actions.install_entered.notified()).await?;
    let mut events = h.core.subscribe();
    let replacement = phone.reconnect().await?;
    event_matching(&mut events,|e|matches!(e,pneud::Event::ConnectionChanged{device,connected:true} if device==&phone.identity.device_id)).await?;
    h.actions.install_release.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(1), h.actions.changed.notified()).await;
    let wrote = h.actions.text.lock().unwrap().clone();
    eprintln!("after connection replacement: {wrote:?}");
    drop(old);
    drop(replacement);
    h.stop().await?;
    assert!(
        wrote.is_empty(),
        "revoked connection's pending install reached OS"
    );
    Ok(())
}
#[tokio::test]
async fn probe_authorized_mesh_install_after_disable() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("disabled-authorized")?;
    h.pair(&phone).await?.close().await?;
    let mut tls = mesh_phone(&h, &phone).await?;
    h.actions
        .block_install
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let text = Clipboard {
        sensitive: false,
        transfer_id: Id::from_bytes([86; 16]),
        text: "disabled mesh effect".into(),
        created_at: 1,
        origin: ClipboardOrigin::Clipboard,
        hlc: Some(pneud::now_ms()),
        source_device: Some(phone.identity.device_id.clone()),
    };
    write_frame(&mut tls, &Message::Clipboard(text)).await?;
    tokio::time::timeout(Duration::from_secs(3), h.actions.install_entered.notified()).await?;
    h.core
        .set_peer_setting(
            phone.identity.device_id.clone(),
            format!(
                "{}/{}/clipboard",
                phone.identity.device_id, h.core.identity.device_id
            ),
            false,
        )
        .await?;
    h.actions.install_release.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(1), h.actions.changed.notified()).await;
    let wrote = h.actions.text.lock().unwrap().clone();
    eprintln!("after incoming direction disabled: {wrote:?}");
    drop(tls);
    h.stop().await?;
    assert!(
        wrote.is_empty(),
        "disabled direction's pending install reached OS"
    );
    Ok(())
}

#[path = "support/file_outbox.rs"]
mod file_outbox;
