use anyhow::Result;
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
    block_notice: std::sync::atomic::AtomicBool,
    notice_entered: tokio::sync::Notify,
    notice_release: tokio::sync::Notify,
    receipt_notices: Mutex<Vec<String>>,
    failure_saved: Mutex<Vec<bool>>,
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
struct Harness {
    dir: tempfile::TempDir,
    core: Arc<Core>,
    task: JoinHandle<Result<()>>,
    actions: Arc<Actions>,
    address: String,
}
impl Harness {
    async fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let actions = Arc::new(Actions::default());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
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
        let task = tokio::spawn(core.clone().serve(listener, false));
        Ok(Self {
            dir,
            core,
            task,
            actions,
            address,
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
        tokio::time::timeout(Duration::from_secs(15), self.task).await??
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
    let meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
    assert!(!c.send_file(meta.clone(), &source).await?.deduplicated);
    h.wait_images(1).await?;
    let paths = h.saved()?;
    assert_eq!(paths.len(), 1);
    assert_eq!(std::fs::read(&paths[0])?, bytes);
    assert!(c.send_file(meta.clone(), &source).await?.deduplicated);
    assert_eq!(h.saved()?.len(), 1);
    assert_eq!(h.actions.images.lock().unwrap().len(), 1);
    assert_eq!(h.actions.notices.lock().unwrap().len(), 1);
    let text = Clipboard {
        transfer_id: Id::new(pneud::random_hex(16))?,
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
    let mut meta = metadata(Id::new(pneud::random_hex(16))?, &bytes);
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
    phone.identity.device_id = Id::new(pneud::random_hex(16))?;
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
    let meta = metadata(Id::new(pneud::random_hex(16))?, b"correct");
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
        "clipboard_clock",
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
    let empty = metadata(Id::new(pneud::random_hex(16))?, b"");
    begin_chunks(&mut c, &empty, b"").await?;
    assert!(matches!(
        end(&mut c, empty.transfer_id).await?,
        Message::Ack(_)
    ));
    let mut config = h.core.config();
    config.max_file_bytes = 0;
    h.core.update_config(config).await?;
    let meta = metadata(Id::new(pneud::random_hex(16))?, b"oversize");
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
    let meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
            .starts_with("received-")
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
    let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
        transfer_id: Id::new(pneud::random_hex(16))?,
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
    let listener = TcpListener::bind(&h.address).await?;
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
    let meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let next = metadata(Id::new(pneud::random_hex(16))?, b"next");
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
        matches!(replacement.read(Duration::from_secs(1)).await?,Message::Pong(p) if p.nonce==nonce)
    );
    h.actions
        .block_image
        .store(false, std::sync::atomic::Ordering::SeqCst);
    h.actions.image_release.notify_one();
    h.wait_images(1).await?;
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
        transfer_id: Id::new(pneud::random_hex(16))?,
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
        assert!(!c.send_text(text.clone()).await?.deduplicated);
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
    assert_eq!(h.actions.text.lock().unwrap().len(), 2);
    // Omission and explicit clipboard have the same semantic default for dedupe.
    let text = Clipboard {
        transfer_id: Id::new(pneud::random_hex(16))?,
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
        transfer_id: Id::new(pneud::random_hex(16))?,
        text: "received".into(),
        created_at: 0,
        origin: ClipboardOrigin::Share,
    };
    c.send_text(text).await?;
    let complete = metadata(Id::new(pneud::random_hex(16))?, b"saved");
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
    assert_eq!(device_rows(&h, device, "outgoing")?, 1);
    phone.set_paused(true)?;
    let mut events = h.core.subscribe();
    c.send_status(true).await?;
    event_matching(&mut events, |e| {
        matches!(e, pneud::Event::PhonePaused { paused: true, .. })
    })
    .await?;
    let partial = metadata(Id::new(pneud::random_hex(16))?, b"unfinished");
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
        "clipboard_clock",
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
    let meta = metadata(Id::new(pneud::random_hex(16))?, b"retry bytes");
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
    let listener = TcpListener::bind(&h.address).await?;
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
    let meta = metadata(Id::new(pneud::random_hex(16))?, &bytes);
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
    let bytes = b"\x89PNG\r\n\x1a\nimage";
    for index in 0..10 {
        let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
        meta.kind = FileKind::ClipboardImage;
        meta.file_name = format!("explicit-{index}.png");
        begin_chunks(&mut c, &meta, bytes).await?;
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
    let bytes = b"\x89PNG\r\n\x1a\nimage";
    for index in 0..67 {
        let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
        meta.file_name = if index == 66 {
            "latest.png".into()
        } else {
            format!("automatic-{index}.png")
        };
        begin_chunks(&mut c, &meta, bytes).await?;
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
        let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
        meta.kind = FileKind::ClipboardImage;
        meta.file_name = format!("explicit-{index}.png");
        begin_chunks(&mut c, &meta, bytes).await?;
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
            .ends_with("automatic-0.png")
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
    h.actions
        .block_text
        .store(true, std::sync::atomic::Ordering::SeqCst);
    c.write(&Message::Clipboard(
        phone.text("blocked".into(), ClipboardOrigin::Clipboard)?,
    ))
    .await?;
    tokio::time::timeout(Duration::from_secs(2), h.actions.text_entered.notified()).await?;
    let meta = metadata(Id::new(pneud::random_hex(16))?, b"file alongside text");
    begin_chunks(&mut c, &meta, b"file alongside text").await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    let mut events = h.core.subscribe();
    let id = h
        .core
        .send_clipboard(phone.identity.device_id.clone(), "reverse".into())
        .await?;
    let message = c.read(Duration::from_secs(2)).await?;
    assert!(matches!(&message,Message::Clipboard(t) if t.transfer_id==id));
    c.handle(message).await?;
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
    let first = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let second = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let bytes = b"\x89PNG\r\n\x1a\nimage";
    for name in ["clipboard", "clipboard.bin", "clipboard.exe"] {
        let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
        meta.kind = FileKind::ClipboardImage;
        meta.file_name = name.into();
        begin_chunks(&mut c, &meta, bytes).await?;
        assert!(matches!(
            end(&mut c, meta.transfer_id).await?,
            Message::Ack(_)
        ));
    }
    // Ordinary image copying has the same content rule, with viewer safety
    // independently checking the extension.
    let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
    meta.file_name = "screenshot.bin".into();
    begin_chunks(&mut c, &meta, bytes).await?;
    assert!(matches!(
        end(&mut c, meta.transfer_id).await?,
        Message::Ack(_)
    ));
    h.wait_images(4).await?;
    let invalid = b"executable bytes with an image filename and MIME";
    let meta = metadata(Id::new(pneud::random_hex(16))?, invalid);
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
async fn new_pairing_replaces_old_and_cancels_its_running_and_queued_actions() -> Result<()> {
    let h = Harness::new().await?;
    let old = h.phone("old")?;
    let next = h.phone("next")?;
    let mut c = h.pair(&old).await?;
    let bytes = b"\x89PNG\r\n\x1a\nimage";
    h.actions
        .block_image
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for index in 0..2 {
        let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let unfinished = metadata(Id::new(pneud::random_hex(16))?, bytes);
    begin_chunks(&mut c, &unfinished, bytes).await?;
    event_matching(&mut transfer_events, |event|matches!(event,pneud::Event::TransferProgress {transfer_id,..} if *transfer_id==unfinished.transfer_id)).await?;
    assert!(h.core.ui_snapshot().receiving);
    tokio::time::sleep(Duration::from_secs(1)).await;
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
        "clipboard_clock",
        "clipboard_recent",
        "outgoing",
    ] {
        assert_eq!(device_rows(&h, &old.identity.device_id, table)?, 0);
    }
    let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
            let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
                transfer_id: Id::new(pneud::random_hex(16))?,
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
        let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let first = metadata(Id::new(pneud::random_hex(16))?, bytes);
    begin_chunks(&mut c, &first, bytes).await?;
    assert!(matches!(
        end(&mut c, first.transfer_id).await?,
        Message::Ack(_)
    ));
    tokio::time::timeout(Duration::from_secs(2), h.actions.image_entered.notified()).await?;
    let source = h.dir.path().join("latest.png");
    std::fs::write(&source, bytes)?;
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
        let meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let bytes = b"\x89PNG\r\n\x1a\nimage";
    for index in 0..3 {
        let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
        meta.file_name = format!("{index}-{}.png", "é".repeat(122));
        begin_chunks(&mut c, &meta, bytes).await?;
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
        .saved()?
        .into_iter()
        .find(|p| p.file_name().unwrap().to_string_lossy().contains("-2-"))
        .unwrap();
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
    let message = c.read(Duration::from_secs(2)).await?;
    assert!(matches!(&message,Message::Clipboard(t) if t.transfer_id == id));
    c.handle(message).await?;
    event_matching(
        &mut events,
        |e| matches!(e,pneud::Event::ClipboardSent {transfer_id,..} if *transfer_id == id),
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
        let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let mut meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    c.send_file(metadata(Id::new(pneud::random_hex(16))?, bytes), &source)
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
    let initial = h
        .core
        .send_clipboard(phone.identity.device_id.clone(), "hold this receipt".into())
        .await?;
    assert!(
        matches!(read_frame(&mut tls).await?,Some(Message::Clipboard(text)) if text.transfer_id==initial)
    );
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
        1,
        "image preparation must retain the explicit outbox"
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
    let initial = h
        .core
        .send_clipboard(
            phone.identity.device_id.clone(),
            "hold another receipt".into(),
        )
        .await?;
    assert!(
        matches!(read_frame(&mut tls).await?, Some(Message::Clipboard(text)) if text.transfer_id == initial)
    );
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
        1,
        "automatic preparation must preserve the explicit send awaiting ACK"
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
    assert_eq!(device_rows(&h, &device, "outgoing")?, 1);
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
    let meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
    let meta = metadata(Id::new(pneud::random_hex(16))?, bytes);
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
