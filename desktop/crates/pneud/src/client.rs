//! Test-tool phone transport. Sends are ephemeral; only identity, pairing and
//! pause state and clipboard clock persist. This is deliberately not a production retry/outbox client.
use crate::{config::atomic_private, identity::Identity};
use anyhow::{Context, Result, bail, ensure};
use pneu_proto::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{io::AsyncReadExt, net::TcpStream, time::Instant};
use tokio_rustls::{TlsConnector, client::TlsStream};

/// An authenticated protocol rejection, preserving the exact frame for test tools.
#[derive(Debug)]
pub struct PcRejection(pub ErrorMessage);
impl std::fmt::Display for PcRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.code == ErrorCode::Unauthorized {
            write!(
                f,
                "PC ERROR unauthorized: Pairing lost or pairing admission denied"
            )
        } else {
            write!(f, "PC ERROR {:?}", self.0.code)
        }
    }
}
impl std::error::Error for PcRejection {}

#[derive(Clone, Serialize, Deserialize)]
pub struct Binding {
    pub fingerprint: Hash,
    pub device_id: Id,
    pub addresses: Vec<String>,
}
pub struct Phone {
    pub identity: Identity,
    pub directory: PathBuf,
}
pub struct PhoneConnection {
    writer: tokio::io::WriteHalf<TlsStream<TcpStream>>,
    incoming: tokio::sync::mpsc::Receiver<pneu_proto::Result<Option<Message>>>,
    reader_task: tokio::task::JoinHandle<()>,
    pub pc: Hello,
    directory: PathBuf,
    received: VecDeque<(Id, crate::store::TextReceipt)>,
    last_clipboard: Option<u64>,
    last_sent: Instant,
    ping: Option<(Nonce, Instant)>,
}
impl Phone {
    pub fn new(directory: PathBuf) -> Result<Self> {
        let identity = Identity::load_or_create(&directory)?;
        Ok(Self {
            identity,
            directory,
        })
    }
    pub fn paused(&self) -> Result<bool> {
        paused(&self.directory)
    }
    pub fn set_paused(&self, paused: bool) -> Result<()> {
        atomic_private(
            &self.directory.join("paused.json"),
            &serde_json::to_vec(&paused)?,
        )
    }
    fn pending(&self) -> Result<Option<PairingUri>> {
        match crate::storage::read_bounded(&self.directory.join("pending-pairing.json"), 65536) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
    pub async fn pair(&self, uri: &PairingUri) -> Result<PhoneConnection> {
        uri.validate()?;
        ensure!(uri.version == 1, "unsupported pairing version");
        if let Some(pending) = self.pending()?
            && pending == *uri
        {
            return self.recover_pairing(&pending).await;
        }
        ensure!(
            uri.expires_at > crate::now_ms(),
            "brand-new pairing invitation expired locally"
        );
        // Save the pin/address/token before sending any request. Never overwrite
        // an existing successful binding until the new pairing is accepted.
        atomic_private(
            &self.directory.join("pending-pairing.json"),
            &serde_json::to_vec(uri)?,
        )?;
        self.recover_pairing(uri).await
    }
    async fn recover_pairing(&self, uri: &PairingUri) -> Result<PhoneConnection> {
        let mut c = self
            .connect(&uri.addresses, uri.fingerprint.clone(), None)
            .await?;
        c.write_with_deadline(
            &Message::PairRequest(PairRequest {
                token: uri.token.clone(),
            }),
            Duration::from_secs(10),
        )
        .await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut buffered = Vec::new();
        loop {
            match c
                .read(deadline.saturating_duration_since(Instant::now()))
                .await?
            {
                Message::PairResult(result) if result.accepted => break,
                Message::PairResult(result) => {
                    remove_state(&self.directory.join("pending-pairing.json"))?;
                    bail!("pairing rejected: {:?}", result.code);
                }
                m @ Message::Clipboard(_) => {
                    ensure!(buffered.len() < 2, "too much data before pairing result");
                    buffered.push(m);
                }
                Message::Error(error) => bail!("pairing closed: {:?}", error.code),
                _ => bail!("unexpected pairing reply"),
            }
        }
        let binding = Binding {
            fingerprint: uri.fingerprint.clone(),
            device_id: c.pc.device_id.clone(),
            addresses: uri.addresses.clone(),
        };
        atomic_private(
            &self.directory.join("binding.json"),
            &serde_json::to_vec(&binding)?,
        )?;
        remove_state(&self.directory.join("pending-pairing.json"))?;
        for message in buffered {
            c.handle(message).await?;
        }
        Ok(c)
    }
    pub async fn reconnect(&self) -> Result<PhoneConnection> {
        if let Some(pending) = self.pending()? {
            return self.recover_pairing(&pending).await;
        }
        let binding: Binding = serde_json::from_slice(&crate::storage::read_bounded(
            &self.directory.join("binding.json"),
            65536,
        )?)?;
        self.connect(
            &binding.addresses,
            binding.fingerprint,
            Some(&binding.device_id),
        )
        .await
    }
    pub async fn unpair(&self) -> Result<()> {
        if let Ok(mut connection) = self.reconnect().await {
            connection.unpair().await?;
        }
        forget_peer(&self.directory)?;
        Ok(())
    }
    pub async fn connect(
        &self,
        addresses: &[String],
        pin: Hash,
        expected_id: Option<&Id>,
    ) -> Result<PhoneConnection> {
        let config = crate::tls::client_config(&self.identity, pin)?;
        let connector = TlsConnector::from(Arc::new(config));
        let mut last = None;
        for address in addresses {
            let result: Result<PhoneConnection> = async {
                let tcp = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(address))
                    .await??;
                tcp.set_nodelay(true)?;
                let mut tls = tokio::time::timeout(
                    Duration::from_secs(10),
                    connector.connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp),
                )
                .await??;
                let sent = crate::transport::send(
                    &mut tls,
                    &Message::Hello(Hello {
                        version: 1,
                        device_id: self.identity.device_id.clone(),
                        name: "Pneu simulator".into(),
                        capabilities: vec!["clipboard_text".into()],
                        paused: Some(self.paused()?),
                    }),
                    Duration::from_secs(10),
                )
                .await;
                // A fatal frame may already be waiting even if HELLO hit a closed socket.
                let pc = match tokio::time::timeout(Duration::from_secs(10), read_frame(&mut tls))
                    .await??
                {
                    Some(Message::Hello(h)) => h,
                    Some(Message::Error(error)) => return Err(PcRejection(error).into()),
                    _ => {
                        sent?;
                        bail!("expected PC HELLO");
                    }
                };
                sent?;
                ensure!(pc.version == 1, "PC version mismatch");
                if let Some(expected) = expected_id {
                    ensure!(*expected == pc.device_id, "PC device binding mismatch");
                }
                let (mut reader, writer) = tokio::io::split(tls);
                let (tx, incoming) = tokio::sync::mpsc::channel(8);
                let reader_task = tokio::spawn(async move {
                    loop {
                        let frame = read_frame(&mut reader).await;
                        let ended = !matches!(frame, Ok(Some(_)));
                        if tx.send(frame).await.is_err() || ended {
                            break;
                        }
                    }
                });
                Ok(PhoneConnection {
                    writer,
                    incoming,
                    reader_task,
                    pc,
                    directory: self.directory.clone(),
                    received: VecDeque::new(),
                    last_clipboard: None,
                    last_sent: Instant::now(),
                    ping: None,
                })
            }
            .await;
            match result {
                Ok(c) => return Ok(c),
                Err(e) if e.downcast_ref::<PcRejection>().is_some() => return Err(e),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no endpoints")))
    }
    /// Hash without copying or retaining the source. Each command creates a fresh ID.
    pub async fn file_metadata(
        &self,
        path: &Path,
        kind: FileKind,
        mime: String,
    ) -> Result<FileBegin> {
        let mut file = tokio::fs::File::open(path).await?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut buf = vec![0; CHUNK_SIZE];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            size += n as u64;
            ensure!(size <= MAX_FILE_SIZE, "file too large");
            hash.update(&buf[..n]);
        }
        let begin = FileBegin {
            transfer_id: Id::new(crate::random_hex(16))?,
            file_name: path
                .file_name()
                .context("filename")?
                .to_str()
                .context("UTF-8 filename")?
                .into(),
            mime,
            size,
            sha256: Hash::new(hex::encode(hash.finalize()))?,
            kind,
            created_at: crate::now_ms(),
            source_media_id: None,
        };
        encode_frame(&Message::FileBegin(begin.clone()))?;
        Ok(begin)
    }
    pub fn text(&self, text: String, origin: ClipboardOrigin) -> Result<Clipboard> {
        let mut text = Clipboard {
            transfer_id: Id::new(crate::random_hex(16))?,
            text,
            created_at: crate::now_ms(),
            origin,
        };
        encode_frame(&Message::Clipboard(text.clone()))?;
        // Independent simulator commands share this small persistent clock.
        // Lock a separate file so atomic replacement cannot change the lock inode.
        let lock = std::fs::File::from(rustix::fs::open(
            self.directory.join("clipboard-clock.lock"),
            rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from_raw_mode(0o600),
        )?);
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
        let clock = self.directory.join("clipboard-clock.json");
        let last: u64 = match crate::storage::read_bounded(&clock, 65536) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => 0,
            Err(e) => return Err(e),
        };
        text.created_at = text
            .created_at
            .max(last.checked_add(1).context("clipboard clock exhausted")?);
        ensure!(
            text.created_at <= MAX_TIMESTAMP,
            "clipboard clock exhausted"
        );
        atomic_private(&clock, &serde_json::to_vec(&text.created_at)?)?;
        Ok(text)
    }
}
impl PhoneConnection {
    pub async fn write(&mut self, message: &Message) -> Result<()> {
        self.write_with_deadline(message, Duration::from_secs(60))
            .await
    }
    async fn write_with_deadline(&mut self, message: &Message, timeout: Duration) -> Result<()> {
        crate::transport::send(&mut self.writer, message, timeout).await?;
        self.last_sent = Instant::now();
        Ok(())
    }
    fn control_deadline(&self, deadline: Instant) -> Instant {
        self.ping
            .as_ref()
            .map(|(_, due)| deadline.min(*due))
            .unwrap_or(deadline)
    }
    fn check_deadlines(&self, deadline: Instant) -> Result<()> {
        ensure!(Instant::now() < deadline, "PC read deadline");
        ensure!(
            self.ping
                .as_ref()
                .is_none_or(|(_, due)| Instant::now() < *due),
            "PONG timeout"
        );
        Ok(())
    }
    async fn write_until(&mut self, message: &Message, deadline: Instant) -> Result<()> {
        self.check_deadlines(deadline)?;
        let due = self.control_deadline(deadline);
        self.write_with_deadline(message, due.saturating_duration_since(Instant::now()))
            .await
    }
    pub async fn read(&mut self, timeout: Duration) -> Result<Message> {
        self.read_until(Instant::now() + timeout).await
    }
    async fn read_until(&mut self, deadline: Instant) -> Result<Message> {
        loop {
            // Queued traffic never wins over a deadline that already expired.
            self.check_deadlines(deadline)?;
            let keepalive = self
                .ping
                .as_ref()
                .map(|(_, due)| *due)
                .unwrap_or(self.last_sent + Duration::from_secs(60));
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline.min(keepalive)) => {
                    self.check_deadlines(deadline)?;
                    let nonce = Nonce::new(crate::random_hex(8))?;
                    self.write_until(&Message::Ping(Ping { nonce: nonce.clone() }), deadline).await?;
                    self.ping = Some((nonce, Instant::now()+Duration::from_secs(30)));
                },
                frame = self.incoming.recv() => {
                    self.check_deadlines(deadline)?;
                    let message = frame.context("PC reader closed")??.context("PC disconnected")?;
                    match message {
                        Message::Ping(p) => self.write_until(&Message::Pong(p), deadline).await?,
                        Message::Pong(p) => {
                            if self.ping.is_none() { return Ok(Message::Pong(p)); }
                            ensure!(self.ping.as_ref().is_some_and(|(nonce,_)| *nonce == p.nonce), "unsolicited PONG");
                            self.ping = None;
                        },
                        other => return Ok(other),
                    }
                },
            }
        }
    }
    pub async fn send_status(&mut self, paused: bool) -> Result<()> {
        atomic_private(
            &self.directory.join("paused.json"),
            &serde_json::to_vec(&paused)?,
        )?;
        self.write(&Message::Status(Status { paused })).await
    }
    pub async fn unpair(&mut self) -> Result<()> {
        forget_peer(&self.directory)?;
        self.reader_task.abort();
        let _ = crate::transport::close(
            &mut self.writer,
            &[
                Message::Unpair(Unpair {}),
                Message::Error(ErrorMessage {
                    code: ErrorCode::Closing,
                    message: None,
                    supported_version: None,
                }),
            ],
        )
        .await;
        Ok(())
    }
    pub async fn close(&mut self) -> Result<()> {
        self.reader_task.abort();
        crate::transport::close(&mut self.writer, &[]).await
    }
    pub async fn send_file(&mut self, begin: FileBegin, path: &Path) -> Result<Ack> {
        ensure!(
            self.pc.capabilities.iter().any(|c| c == "files"),
            "PC does not receive files"
        );
        ensure!(
            begin.kind != FileKind::ClipboardImage
                || self.pc.capabilities.iter().any(|c| c == "clipboard_image"),
            "PC does not receive clipboard images"
        );
        self.write(&Message::FileBegin(begin.clone())).await?;
        let mut file = tokio::fs::File::open(path).await?;
        let mut offset = 0;
        loop {
            let mut data = vec![0; CHUNK_SIZE];
            let n = file.read(&mut data).await?;
            if n == 0 {
                break;
            }
            data.truncate(n);
            self.write(&Message::FileChunk(FileChunk {
                transfer_id: begin.transfer_id.clone(),
                offset,
                data,
            }))
            .await?;
            offset += n as u64;
            let control_deadline = Instant::now() + Duration::from_secs(60);
            loop {
                self.check_deadlines(control_deadline)?;
                let Ok(frame) = self.incoming.try_recv() else {
                    break;
                };
                match frame?.context("PC disconnected")? {
                    Message::Ping(p) => {
                        self.write_until(&Message::Pong(p), control_deadline)
                            .await?
                    }
                    Message::Pong(p) => {
                        ensure!(
                            self.ping.as_ref().is_some_and(|(n, _)| *n == p.nonce),
                            "unsolicited PONG"
                        );
                        self.ping = None;
                    }
                    m => self.handle_until(m, control_deadline).await?,
                }
            }
        }
        self.write(&Message::FileEnd(FileEnd {
            transfer_id: begin.transfer_id.clone(),
        }))
        .await?;
        self.receipt(&begin.transfer_id).await
    }
    pub async fn send_text(&mut self, text: Clipboard) -> Result<Ack> {
        ensure!(
            self.pc.capabilities.iter().any(|c| c == "clipboard_text"),
            "PC does not receive clipboard text"
        );
        self.write(&Message::Clipboard(text.clone())).await?;
        self.receipt(&text.transfer_id).await
    }
    async fn receipt(&mut self, id: &Id) -> Result<Ack> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            self.check_deadlines(deadline)?;
            match self.read_until(deadline).await? {
                Message::Ack(a) if a.transfer_id == *id => return Ok(a),
                Message::Nack(n) if n.transfer_id == *id => {
                    bail!("NACK {:?} retryable={}", n.code, n.retryable)
                }
                m => self.handle_until(m, deadline).await?,
            }
        }
    }
    pub async fn handle(&mut self, message: Message) -> Result<()> {
        self.handle_until(message, Instant::now() + Duration::from_secs(60))
            .await
    }
    async fn handle_until(&mut self, message: Message, deadline: Instant) -> Result<()> {
        self.check_deadlines(deadline)?;
        match message {
            Message::Ping(p) => self.write_until(&Message::Pong(p), deadline).await?,
            Message::Clipboard(text) => {
                if paused(&self.directory)? {
                    // Resolve in-flight PC traffic without applying it or
                    // advancing history; it may be replayed after resume.
                    self.write_until(
                        &Message::Ack(Ack {
                            transfer_id: text.transfer_id,
                            deduplicated: true,
                        }),
                        deadline,
                    )
                    .await?;
                    return Ok(());
                }
                let digest = crate::store::TextReceipt::from(&text);
                let previous = self
                    .received
                    .iter()
                    .find(|(id, _)| *id == text.transfer_id)
                    .map(|(_, meta)| meta);
                let duplicate =
                    previous.is_some() || self.last_clipboard.is_some_and(|t| text.created_at <= t);
                if previous.is_some_and(|p| *p != digest) {
                    self.write_until(
                        &Message::Nack(Nack {
                            transfer_id: text.transfer_id,
                            code: NackCode::IdConflict,
                            retryable: false,
                            message: None,
                        }),
                        deadline,
                    )
                    .await?;
                } else {
                    if !duplicate {
                        self.last_clipboard = Some(text.created_at);
                        self.received.push_back((text.transfer_id.clone(), digest));
                        if self.received.len() > crate::store::RECENT_TEXT_LIMIT {
                            self.received.pop_front();
                        }
                        println!("CLIPBOARD received {}", text.transfer_id);
                    }
                    self.write_until(
                        &Message::Ack(Ack {
                            transfer_id: text.transfer_id,
                            deduplicated: duplicate,
                        }),
                        deadline,
                    )
                    .await?;
                }
            }
            Message::Unpair(_) => {
                forget_peer(&self.directory)?;
                self.close().await?;
                bail!("PC unpaired");
            }
            Message::Error(e) => bail!("PC closed: {:?}", e.code),
            _ => bail!("unexpected PC frame"),
        }
        Ok(())
    }
    pub async fn listen(&mut self, seconds: u64) -> Result<()> {
        let end = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < end {
            match self.read_until(end).await {
                Ok(message) => self.handle_until(message, end).await?,
                Err(_) if Instant::now() >= end => break,
                Err(e) => return Err(e),
            }
        }
        self.close().await
    }
}
fn paused(directory: &Path) -> Result<bool> {
    match crate::storage::read_bounded(&directory.join("paused.json"), 65536) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}
fn remove_state(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => crate::config::sync_parent(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn forget_peer(directory: &Path) -> Result<()> {
    remove_state(&directory.join("pending-pairing.json"))?;
    remove_state(&directory.join("binding.json"))
}
impl Drop for PhoneConnection {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::{Admission, ClientVerifier};
    use std::sync::{Mutex, atomic::AtomicBool};
    use tokio::{net::TcpListener, task::JoinHandle};
    use tokio_rustls::TlsAcceptor;

    #[derive(Clone, Copy)]
    enum Scenario {
        ReceiptChallenge,
        IdleReceipt,
        Listen,
        Deadline,
        PingFlood,
        ClipboardOrder,
        PausedClipboard,
    }
    async fn mock_pc(
        directory: &Path,
        scenario: Scenario,
        id: Id,
    ) -> Result<(PhoneConnection, JoinHandle<Result<()>>)> {
        let phone = Phone::new(directory.join("phone"))?;
        let pc = Identity::load_or_create(&directory.join("pc"))?;
        let admission = Arc::new(Mutex::new(Admission::default()));
        admission.lock().unwrap().peers.insert(
            phone.identity.fingerprint.clone(),
            phone.identity.device_id.clone(),
        );
        let config = crate::tls::server_config(
            &pc,
            Arc::new(ClientVerifier::new(
                admission,
                Arc::new(AtomicBool::new(false)),
            )),
        )?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let pin = pc.fingerprint.clone();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await?;
            let mut tls = TlsAcceptor::from(Arc::new(config)).accept(tcp).await?;
            ensure!(
                matches!(read_frame(&mut tls).await?, Some(Message::Hello(_))),
                "phone HELLO"
            );
            crate::transport::send(
                &mut tls,
                &Message::Hello(Hello {
                    version: 1,
                    device_id: pc.device_id,
                    name: "Mock PC".into(),
                    capabilities: vec!["clipboard_text".into()],
                    paused: None,
                }),
                Duration::from_secs(2),
            )
            .await?;
            if matches!(scenario, Scenario::Deadline) {
                crate::transport::send(
                    &mut tls,
                    &Message::Ping(Ping {
                        nonce: Nonce::new(crate::random_hex(8))?,
                    }),
                    Duration::from_secs(2),
                )
                .await?;
                crate::transport::send(
                    &mut tls,
                    &Message::Ack(Ack {
                        transfer_id: id,
                        deduplicated: false,
                    }),
                    Duration::from_secs(2),
                )
                .await?;
                ensure!(
                    read_frame(&mut tls).await?.is_none(),
                    "client should close without answering expired frames"
                );
                return Ok(());
            }
            if matches!(scenario, Scenario::PingFlood) {
                let until = Instant::now() + Duration::from_millis(100);
                while Instant::now() < until {
                    let nonce = Nonce::new(crate::random_hex(8))?;
                    if crate::transport::send(
                        &mut tls,
                        &Message::Ping(Ping {
                            nonce: nonce.clone(),
                        }),
                        Duration::from_secs(1),
                    )
                    .await
                    .is_err()
                    {
                        return Ok(());
                    }
                    match read_frame(&mut tls).await {
                        Ok(Some(Message::Pong(p))) => {
                            ensure!(p.nonce == nonce, "matching flood PONG")
                        }
                        Ok(None) | Err(_) => return Ok(()),
                        _ => bail!("expected flood PONG"),
                    }
                }
                bail!("continuous PING bypassed read deadline");
            }
            if matches!(scenario, Scenario::ClipboardOrder) {
                for (timestamp, deduplicated) in [(2, false), (1, true), (3, false), (3, true)] {
                    let text = Clipboard {
                        transfer_id: Id::new(crate::random_hex(16))?,
                        text: "private".into(),
                        created_at: timestamp,
                        origin: ClipboardOrigin::Share,
                    };
                    crate::transport::send(
                        &mut tls,
                        &Message::Clipboard(text.clone()),
                        Duration::from_secs(1),
                    )
                    .await?;
                    ensure!(
                        matches!(read_frame(&mut tls).await?, Some(Message::Ack(a)) if a.transfer_id == text.transfer_id && a.deduplicated == deduplicated),
                        "simulator LWW receipt"
                    );
                }
                ensure!(read_frame(&mut tls).await?.is_none(), "client closed");
                return Ok(());
            }
            if matches!(scenario, Scenario::PausedClipboard) {
                let text = Clipboard {
                    transfer_id: id,
                    text: "private".into(),
                    created_at: 9,
                    origin: ClipboardOrigin::Clipboard,
                };
                ensure!(
                    matches!(
                        read_frame(&mut tls).await?,
                        Some(Message::Status(Status { paused: true }))
                    ),
                    "pause status"
                );
                crate::transport::send(
                    &mut tls,
                    &Message::Clipboard(text.clone()),
                    Duration::from_secs(1),
                )
                .await?;
                ensure!(
                    matches!(read_frame(&mut tls).await?, Some(Message::Ack(a)) if a.transfer_id == text.transfer_id && a.deduplicated),
                    "ignored clipboard receipt"
                );
                ensure!(
                    matches!(
                        read_frame(&mut tls).await?,
                        Some(Message::Status(Status { paused: false }))
                    ),
                    "resume status"
                );
                crate::transport::send(
                    &mut tls,
                    &Message::Clipboard(text.clone()),
                    Duration::from_secs(1),
                )
                .await?;
                ensure!(
                    matches!(read_frame(&mut tls).await?, Some(Message::Ack(a)) if a.transfer_id == text.transfer_id && !a.deduplicated),
                    "clipboard replay after resume"
                );
                ensure!(read_frame(&mut tls).await?.is_none(), "client closed");
                return Ok(());
            }
            if !matches!(scenario, Scenario::Listen) {
                ensure!(
                    matches!(read_frame(&mut tls).await?, Some(Message::Clipboard(text)) if text.transfer_id == id),
                    "text before receipt"
                );
            }
            if matches!(scenario, Scenario::IdleReceipt) {
                let Some(Message::Ping(ping)) = read_frame(&mut tls).await? else {
                    bail!("missing idle PING");
                };
                crate::transport::send(&mut tls, &Message::Pong(ping), Duration::from_secs(2))
                    .await?;
            } else {
                let nonce = Nonce::new(crate::random_hex(8))?;
                crate::transport::send(
                    &mut tls,
                    &Message::Ping(Ping {
                        nonce: nonce.clone(),
                    }),
                    Duration::from_secs(2),
                )
                .await?;
                ensure!(
                    matches!(read_frame(&mut tls).await?, Some(Message::Pong(p)) if p.nonce == nonce),
                    "matching challenge PONG"
                );
            }
            if !matches!(scenario, Scenario::Listen) {
                crate::transport::send(
                    &mut tls,
                    &Message::Ack(Ack {
                        transfer_id: id,
                        deduplicated: false,
                    }),
                    Duration::from_secs(2),
                )
                .await?;
            }
            ensure!(read_frame(&mut tls).await?.is_none(), "client closed");
            Ok(())
        });
        let connection = phone.connect(&[address], pin, None).await?;
        Ok((connection, task))
    }
    #[tokio::test]
    async fn receipt_wait_answers_challenges_and_generates_idle_ping() -> Result<()> {
        for scenario in [Scenario::ReceiptChallenge, Scenario::IdleReceipt] {
            let directory = tempfile::tempdir()?;
            let id = Id::new(crate::random_hex(16))?;
            let (mut connection, pc) = mock_pc(directory.path(), scenario, id.clone()).await?;
            let text = Clipboard {
                transfer_id: id.clone(),
                text: "private text".into(),
                created_at: 0,
                origin: ClipboardOrigin::Clipboard,
            };
            let ack = if matches!(scenario, Scenario::IdleReceipt) {
                connection.write(&Message::Clipboard(text)).await?;
                connection.last_sent = Instant::now() - Duration::from_secs(61);
                connection.receipt(&id).await?
            } else {
                connection.send_text(text).await?
            };
            assert_eq!(ack.transfer_id, id);
            assert!(connection.ping.is_none());
            connection.close().await?;
            tokio::time::timeout(Duration::from_secs(2), pc).await???;
        }
        Ok(())
    }
    #[tokio::test]
    async fn listener_answers_peer_ping() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (mut connection, pc) = mock_pc(
            directory.path(),
            Scenario::Listen,
            Id::new(crate::random_hex(16))?,
        )
        .await?;
        connection.listen(1).await?;
        tokio::time::timeout(Duration::from_secs(2), pc).await???;
        Ok(())
    }
    #[tokio::test]
    async fn queued_frames_and_control_writes_cannot_bypass_expired_receipt_or_pong_deadlines()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (mut connection, pc) = mock_pc(
            directory.path(),
            Scenario::Deadline,
            Id::new(crate::random_hex(16))?,
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            while connection.incoming.len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(
            connection
                .read_until(Instant::now() - Duration::from_millis(1))
                .await
                .is_err()
        );
        assert_eq!(
            connection.incoming.len(),
            2,
            "expired read consumed queued frames"
        );
        let nonce = Nonce::new(crate::random_hex(8))?;
        connection.ping = Some((nonce.clone(), Instant::now() - Duration::from_millis(1)));
        assert!(connection.read(Duration::from_secs(1)).await.is_err());
        assert_eq!(
            connection.incoming.len(),
            2,
            "expired PONG accepted other traffic"
        );
        assert!(
            connection
                .write_until(
                    &Message::Pong(Ping { nonce }),
                    Instant::now() + Duration::from_secs(60)
                )
                .await
                .is_err()
        );
        let pong_due = Instant::now() + Duration::from_millis(5);
        connection.ping = Some((Nonce::new(crate::random_hex(8))?, pong_due));
        assert_eq!(
            connection.control_deadline(Instant::now() + Duration::from_secs(60)),
            pong_due
        );
        let receipt_due = Instant::now() + Duration::from_millis(1);
        assert_eq!(connection.control_deadline(receipt_due), receipt_due);
        connection.close().await?;
        tokio::time::timeout(Duration::from_secs(2), pc).await???;
        Ok(())
    }
    #[tokio::test]
    async fn continuous_ping_traffic_does_not_extend_fixed_read_deadline() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (mut connection, pc) = mock_pc(
            directory.path(),
            Scenario::PingFlood,
            Id::new(crate::random_hex(16))?,
        )
        .await?;
        let started = Instant::now();
        assert!(connection.read(Duration::from_millis(20)).await.is_err());
        assert!(started.elapsed() < Duration::from_millis(90));
        connection.close().await?;
        tokio::time::timeout(Duration::from_secs(2), pc).await???;
        Ok(())
    }
    #[tokio::test]
    async fn paused_simulator_acknowledges_inflight_text_without_advancing_history() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (mut connection, pc) = mock_pc(
            directory.path(),
            Scenario::PausedClipboard,
            Id::new(crate::random_hex(16))?,
        )
        .await?;
        connection.send_status(true).await?;
        let message = connection.read(Duration::from_secs(1)).await?;
        connection.handle(message).await?;
        assert!(connection.last_clipboard.is_none() && connection.received.is_empty());
        connection.send_status(false).await?;
        let message = connection.read(Duration::from_secs(1)).await?;
        connection.handle(message).await?;
        assert_eq!(connection.last_clipboard, Some(9));
        assert_eq!(connection.received.len(), 1);
        connection.close().await?;
        tokio::time::timeout(Duration::from_secs(2), pc).await???;
        Ok(())
    }
    #[test]
    fn clipboard_clock_increases_across_commands_and_backward_wall_time() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let phone = Phone::new(directory.path().join("phone"))?;
        let clock = phone.directory.join("clipboard-clock.json");
        let future = crate::now_ms() + 1_000_000;
        atomic_private(&clock, &serde_json::to_vec(&future)?)?;
        let first = phone.text("first".into(), ClipboardOrigin::Clipboard)?;
        let second =
            Phone::new(phone.directory.clone())?.text("second".into(), ClipboardOrigin::Share)?;
        assert_eq!(first.created_at, future + 1);
        assert_eq!(second.created_at, future + 2);
        assert_eq!(
            serde_json::from_slice::<u64>(&crate::storage::read_bounded(&clock, 65536)?)?,
            second.created_at
        );
        let mut threads = Vec::new();
        for _ in 0..8 {
            let path = phone.directory.clone();
            threads.push(std::thread::spawn(move || {
                Phone::new(path)?.text("concurrent".into(), ClipboardOrigin::Clipboard)
            }));
        }
        let mut timestamps = threads
            .into_iter()
            .map(|t| t.join().unwrap().map(|t| t.created_at))
            .collect::<Result<Vec<_>>>()?;
        timestamps.sort_unstable();
        assert_eq!(timestamps, (future + 3..=future + 10).collect::<Vec<_>>());
        atomic_private(&clock, &serde_json::to_vec(&MAX_TIMESTAMP)?)?;
        assert!(
            phone
                .text("exhausted".into(), ClipboardOrigin::Clipboard)
                .is_err()
        );
        assert_eq!(
            serde_json::from_slice::<u64>(&crate::storage::read_bounded(&clock, 65536)?)?,
            MAX_TIMESTAMP
        );
        Ok(())
    }
    #[tokio::test]
    async fn simulator_receiving_text_uses_lww_instead_of_fifo() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (mut connection, pc) = mock_pc(
            directory.path(),
            Scenario::ClipboardOrder,
            Id::new(crate::random_hex(16))?,
        )
        .await?;
        for _ in 0..4 {
            let message = connection.read(Duration::from_secs(1)).await?;
            connection.handle(message).await?;
        }
        assert_eq!(connection.last_clipboard, Some(3));
        assert_eq!(connection.received.len(), 2);
        connection.close().await?;
        tokio::time::timeout(Duration::from_secs(2), pc).await???;
        Ok(())
    }
}
