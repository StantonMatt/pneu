#![cfg(unix)]
#[path = "../src/test_support.rs"]
mod test_support;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::{StreamExt, stream::FuturesUnordered};
use pneu_proto::*;
use pneud::{Config, Core, Paths, actions::PcActions};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
struct Mock;
#[async_trait]
impl PcActions for Mock {
    async fn clipboard_text(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn clipboard_image(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn notify(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn open(&self, _: &Path) -> Result<()> {
        Ok(())
    }
}
async fn core() -> Result<(tempfile::TempDir, Arc<Core>, TcpListener)> {
    let dir = test_support::scratch()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
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
    Ok((
        dir,
        Core::new(paths, config, Arc::new(Mock)).await?,
        listener,
    ))
}
async fn until(mut f: impl FnMut() -> bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(6), async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}
async fn wait_established(db: &rusqlite::Connection, device: &Id) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let established: bool = db.query_row(
                "SELECT established FROM peers WHERE id=?",
                [device.as_str()],
                |r| r.get(0),
            )?;
            if established {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}
fn hello(c: &Core, addresses: Vec<String>) -> Hello {
    Hello {
        setup: None,
        version: 1,
        device_id: c.identity.device_id.clone(),
        name: c.config().name,
        role: Some(DeviceRole::Computer),
        addresses: Some(addresses),
        paused: None,
        capabilities: vec!["mesh".into(), "clipboard_text".into()],
    }
}
async fn read_frame<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    tls: &mut T,
) -> Result<Option<Message>> {
    loop {
        match pneu_proto::read_frame(tls).await? {
            Some(Message::PeerSettings(snapshot)) => {
                write_frame(
                    tls,
                    &Message::Ack(Ack {
                        transfer_id: snapshot.transfer_id,
                        deduplicated: false,
                    }),
                )
                .await?
            }
            message => return Ok(message),
        }
    }
}
async fn raw(
    a: &Core,
    b: &Core,
    address: &str,
    addresses: Vec<String>,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(address).await?;
    let config = pneud::tls::client_config(&b.identity, a.identity.fingerprint.clone())?;
    let mut tls = TlsConnector::from(Arc::new(config))
        .connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)
        .await?;
    write_frame(&mut tls, &Message::Hello(hello(b, addresses))).await?;
    match read_frame(&mut tls).await? {
        Some(Message::Hello(_)) => {}
        x => anyhow::bail!("HELLO expected: {x:?}"),
    };
    Ok(tls)
}
// Shared loopback ports can receive host HTTP/TLS probes before the intended
// dialer. Inspect candidates concurrently so a silent probe cannot block it;
// after selecting pneu.local, propagate handshake failures without retrying.
async fn accept_proxy(
    proxy: &TcpListener,
    peer: &Core,
) -> Result<tokio_rustls::server::TlsStream<TcpStream>> {
    let verifier = Arc::new(pneud::tls::ClientVerifier::new(
        Default::default(),
        Default::default(),
    ));
    let config = Arc::new(pneud::tls::server_config(&peer.identity, verifier)?);
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut candidates = FuturesUnordered::new();
        loop {
            tokio::select! {
                accepted = proxy.accept() => {
                    let (tcp, _) = accepted?;
                    candidates.push(tokio_rustls::LazyConfigAcceptor::new(
                        rustls::server::Acceptor::default(), tcp,
                    ));
                }
                Some(hello) = candidates.next(), if !candidates.is_empty() => {
                    let Ok(hello) = hello else { continue };
                    if hello.client_hello().server_name() == Some("pneu.local") {
                        return Ok(hello.into_stream(config.clone()).await?);
                    }
                }
            }
        }
    })
    .await?
}

async fn wait_proxy_closed(tls: &mut tokio_rustls::server::TlsStream<TcpStream>) -> Result<()> {
    // These speculative dials receive ERROR before remote HELLO. Their error
    // classification is synchronous after dropping the socket, so on this
    // single-thread runtime observing closure also orders the state assertions.
    match tokio::time::timeout(Duration::from_secs(3), pneu_proto::read_frame(tls)).await? {
        Ok(None) => Ok(()),
        Err(ProtocolError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(())
        }
        other => anyhow::bail!("speculative dial should close after ERROR: {other:?}"),
    }
}

#[tokio::test]
async fn proxy_accept_skips_foreign_and_stalled_connections() -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let (_da, a, _la) = core().await?;
    let (_db, b, _lb) = core().await?;
    let proxy = TcpListener::bind("127.0.0.1:0").await?;
    let address = proxy.local_addr()?;
    let mut plaintext = TcpStream::connect(address).await?;
    plaintext.write_all(b"GET / HTTP/1.1\r\n\r\n").await?;
    let silent = TcpStream::connect(address).await?;
    let mut partial = TcpStream::connect(address).await?;
    partial.write_all(&[0x16, 0x03]).await?;
    let mut foreign = TcpStream::connect(address).await?;
    let config = Arc::new(pneud::tls::client_config(
        &a.identity,
        b.identity.fingerprint.clone(),
    )?);
    let mut probe = rustls::ClientConnection::new(
        config.clone(),
        rustls::pki_types::ServerName::try_from("localhost")?,
    )?;
    let mut hello = Vec::new();
    probe.write_tls(&mut hello)?;
    foreign.write_all(&hello).await?;
    let dialer = async {
        let tcp = TcpStream::connect(address).await?;
        Ok::<_, anyhow::Error>(
            TlsConnector::from(config)
                .connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)
                .await?,
        )
    };
    let (accepted, dialed) = tokio::try_join!(accept_proxy(&proxy, &b), dialer)?;
    assert_eq!(accepted.get_ref().1.server_name(), Some("pneu.local"));
    drop((accepted, dialed, plaintext, silent, partial, foreign));
    a.shutdown_actions().await?;
    b.shutdown_actions().await?;
    Ok(())
}

#[tokio::test]
async fn lost_result_reverse_dial_keeps_recoverable_binding() -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    let aa = la.local_addr()?.to_string();
    let ba = lb.local_addr()?.to_string();
    let uri = a.start_pairing_with_addresses(vec![aa.clone()]).await?;
    let sa = tokio::spawn(a.clone().serve(la, false));
    let sb = tokio::spawn(b.clone().serve(lb, false));
    let mut tls = raw(&a, &b, &aa, vec![ba]).await?;
    write_frame(
        &mut tls,
        &Message::PairRequest(PairRequest {
            token: uri.token.clone(),
        }),
    )
    .await?;
    assert!(matches!(read_frame(&mut tls).await?,Some(Message::PairResult(r)) if r.accepted));
    assert_eq!(a.paired_devices(), vec![b.identity.device_id.clone()]);
    // The acceptor committed, but the dialer never receives/persists this success.
    drop(tls);
    // Observe the failed reverse dial before checking that its speculative
    // Unauthorized did not remove the acceptor's recoverable binding.
    until(|| {
        a.ui_snapshot()
            .devices
            .iter()
            .any(|d| d.device == b.identity.device_id && d.state == "unauthorized")
    })
    .await?;
    assert_eq!(a.paired_devices(), vec![b.identity.device_id.clone()]);
    let mut recovered = raw(&a, &b, &aa, vec![]).await?;
    write_frame(
        &mut recovered,
        &Message::PairRequest(PairRequest {
            token: uri.token.clone(),
        }),
    )
    .await?;
    assert!(matches!(read_frame(&mut recovered).await?,Some(Message::PairResult(r)) if r.accepted));
    drop(recovered);

    a.request_shutdown();
    b.request_shutdown();
    sa.await??;
    sb.await??;
    Ok(())
}
#[tokio::test]
async fn quota_denial_is_retryable_and_same_invitation_succeeds() -> Result<()> {
    admission_denial(false).await
}
#[tokio::test]
async fn provisional_slot_denial_is_retryable_and_same_invitation_succeeds() -> Result<()> {
    admission_denial(true).await
}
async fn admission_denial(hold_slot: bool) -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    let (_dc, c, lc) = core().await?;
    drop(lc);
    let aa = la.local_addr()?.to_string();
    let uri = a.start_pairing_with_addresses(vec![aa.clone()]).await?;
    let sa = tokio::spawn(a.clone().serve(la, false));
    // A provisional handshake consumes the one-per-second unknown-client allowance.
    let tls = raw(&a, &c, &aa, vec![]).await?;
    let held = if hold_slot {
        // Expire the rate limit while retaining the provisional TLS slot.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        Some(tls)
    } else {
        drop(tls);
        None
    };
    let preview = b.join(&uri.build()?).await?;
    let attempt = preview["attempt"].as_str().unwrap();
    b.confirm_join(attempt).await?;
    let sb = tokio::spawn(b.clone().serve(lb, false));
    until(|| b.join_status(attempt).unwrap()["state"] == "unreachable").await?;
    assert!(!b.paths.data.join("join-pending.json").exists());
    assert!(b.join(&uri.build()?).await.is_err());
    assert!(a.pairing_status().1.is_some());
    assert!(a.paired_devices().is_empty());
    drop(held);
    until(|| b.join_status(attempt).unwrap()["state"] == "paired").await?;
    assert_eq!(b.paired_devices(), vec![a.identity.device_id.clone()]);
    assert_eq!(a.paired_devices(), vec![b.identity.device_id.clone()]);
    a.request_shutdown();
    b.request_shutdown();
    sa.await??;
    sb.await??;
    c.shutdown_actions().await?;
    Ok(())
}

#[tokio::test]
async fn mismatch_preserves_in_memory_attempt_until_local_cancel() -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    let (_dc, c, lc) = core().await?;
    drop(la);
    let uri = a
        .start_pairing_with_addresses(vec![lc.local_addr()?.to_string()])
        .await?;
    let first = b.join(&uri.build()?).await?;
    let old = first["attempt"].as_str().unwrap();
    b.confirm_join(old).await?;
    let valid = c
        .start_pairing_with_addresses(vec![lc.local_addr()?.to_string()])
        .await?;
    let sc = tokio::spawn(c.clone().serve(lc, false));
    let sb = tokio::spawn(b.clone().serve(lb, false));
    until(|| b.join_status(old).unwrap()["state"] == "fingerprint_mismatch").await?;
    assert!(!b.paths.data.join("join-pending.json").exists());
    assert!(b.join(&valid.build()?).await.is_err());
    b.confirm_join(old).await?;
    until(|| b.join_status(old).unwrap()["state"] == "fingerprint_mismatch").await?;
    b.cancel_join(old).await?;
    assert_eq!(b.join_status(old)?["state"], "cancelled");
    let next = b.join(&valid.build()?).await?;
    b.confirm_join(next["attempt"].as_str().unwrap()).await?;
    until(|| b.join_status(next["attempt"].as_str().unwrap()).unwrap()["state"] == "paired")
        .await?;
    assert_eq!(b.paired_devices(), vec![c.identity.device_id.clone()]);
    assert_eq!(c.paired_devices(), vec![b.identity.device_id.clone()]);
    assert!(a.paired_devices().is_empty());
    b.request_shutdown();
    c.request_shutdown();
    sb.await??;
    sc.await??;
    a.shutdown_actions().await?;
    Ok(())
}

#[tokio::test]
async fn delayed_speculative_denial_preserves_completed_join() -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    let proxy = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_addr = proxy.local_addr()?.to_string();
    let aa = la.local_addr()?.to_string();
    let uri = a.start_pairing_with_addresses(vec![aa.clone()]).await?;
    let sa = tokio::spawn(a.clone().serve(la, false));
    let sb = tokio::spawn(b.clone().serve(lb, false));
    let mut original = raw(&a, &b, &aa, vec![proxy_addr]).await?;
    write_frame(
        &mut original,
        &Message::PairRequest(PairRequest {
            token: uri.token.clone(),
        }),
    )
    .await?;
    assert!(matches!(read_frame(&mut original).await?,Some(Message::PairResult(r)) if r.accepted));
    drop(original);
    let mut denied = accept_proxy(&proxy, &b).await?;
    assert!(matches!(
        read_frame(&mut denied).await?,
        Some(Message::Hello(_))
    ));
    assert!(b.paired_devices().is_empty());
    let preview = b.join(&uri.build()?).await?;
    let attempt = preview["attempt"].as_str().unwrap();
    b.confirm_join(attempt).await?;
    until(|| b.join_status(attempt).unwrap()["state"] == "paired").await?;
    // The manual client below owns B's transport. Stop B's supervisor so its
    // redial cannot replace that client while the READY proof is in flight.
    b.request_shutdown();
    sb.await??;
    until(|| a.connected_devices().is_empty()).await?;
    // B has now persisted trust. Its READY PING is valid durable-readiness proof.
    let mut ready = raw(&a, &b, &aa, vec![]).await?;
    let nonce = Nonce::new(pneud::random_hex(8))?;
    write_frame(
        &mut ready,
        &Message::Ping(Ping {
            nonce: nonce.clone(),
        }),
    )
    .await?;
    let reply = read_frame(&mut ready).await?;
    assert!(
        matches!(&reply,Some(Message::Pong(p)) if p.nonce==nonce),
        "READY reply: {reply:?}"
    );
    let db = rusqlite::Connection::open(a.paths.data.join("state.sqlite3"))?;
    // Keepalive is independent of the asynchronous readiness write.
    wait_established(&db, &b.identity.device_id).await?;
    write_frame(
        &mut denied,
        &Message::Error(ErrorMessage {
            code: ErrorCode::Unauthorized,
            message: None,
            supported_version: None,
        }),
    )
    .await?;
    wait_proxy_closed(&mut denied).await?;
    assert_eq!(a.paired_devices(), vec![b.identity.device_id.clone()]);
    assert!(a.connected_devices().contains(&b.identity.device_id));

    drop((ready, denied, db));
    a.request_shutdown();
    sa.await??;
    Ok(())
}
#[tokio::test]
async fn computer_install_failure_rolls_back_and_retry_installs_routes() -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    let aa = la.local_addr()?.to_string();
    let uri = a.start_pairing_with_addresses(vec![aa]).await?;
    let db = rusqlite::Connection::open(b.paths.data.join("state.sqlite3"))?;
    db.execute_batch("CREATE TRIGGER fail_routes BEFORE INSERT ON peers WHEN NEW.role='computer' BEGIN SELECT RAISE(ABORT,'injected route failure'); END;")?;
    let sa = tokio::spawn(a.clone().serve(la, false));
    let sb = tokio::spawn(b.clone().serve(lb, false));
    let preview = b.join(&uri.build()?).await?;
    let attempt = preview["attempt"].as_str().unwrap();
    b.confirm_join(attempt).await?;
    until(|| b.join_status(attempt).unwrap()["state"] == "unreachable").await?;
    assert!(b.paired_devices().is_empty());
    assert_eq!(
        db.query_row("SELECT count(*) FROM peers", [], |r| r.get::<_, i64>(0))?,
        0
    );
    db.execute_batch("DROP TRIGGER fail_routes;")?;
    b.confirm_join(attempt).await?;
    until(|| b.join_status(attempt).unwrap()["state"] == "paired").await?;
    let (role, addresses): (String, String) = db.query_row(
        "SELECT role,addresses FROM peers WHERE id=?",
        [a.identity.device_id.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(role, "computer");
    assert_ne!(addresses, "[]");
    // Pairing commits before registration, and setup can close the initial
    // session for a fresh HELLO. Wait for a live registration, not elapsed time.
    until(|| b.connected_devices().contains(&a.identity.device_id)).await?;
    assert!(b.connected_devices().contains(&a.identity.device_id));

    drop(db);
    a.request_shutdown();
    b.request_shutdown();
    sa.await??;
    sb.await??;
    Ok(())
}
#[tokio::test]
async fn delayed_version_error_cannot_override_inbound_recovery() -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    let proxy = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_addr = proxy.local_addr()?.to_string();
    let aa = la.local_addr()?.to_string();
    let uri = a.start_pairing_with_addresses(vec![aa.clone()]).await?;
    let sa = tokio::spawn(a.clone().serve(la, false));
    let sb = tokio::spawn(b.clone().serve(lb, false));
    let mut original = raw(&a, &b, &aa, vec![proxy_addr]).await?;
    write_frame(
        &mut original,
        &Message::PairRequest(PairRequest {
            token: uri.token.clone(),
        }),
    )
    .await?;
    assert!(matches!(read_frame(&mut original).await?,Some(Message::PairResult(r)) if r.accepted));
    drop(original);
    let mut denied = accept_proxy(&proxy, &b).await?;
    assert!(matches!(
        read_frame(&mut denied).await?,
        Some(Message::Hello(_))
    ));
    assert!(b.paired_devices().is_empty());
    let preview = b.join(&uri.build()?).await?;
    let attempt = preview["attempt"].as_str().unwrap();
    b.confirm_join(attempt).await?;
    until(|| b.join_status(attempt).unwrap()["state"] == "paired").await?;
    // The manual client below owns B's transport. Stop B's supervisor so its
    // redial cannot replace that client while the READY proof is in flight.
    b.request_shutdown();
    sb.await??;
    until(|| a.connected_devices().is_empty()).await?;
    // B has now persisted trust. Its READY PING is valid durable-readiness proof.
    let mut ready = raw(&a, &b, &aa, vec![]).await?;
    let nonce = Nonce::new(pneud::random_hex(8))?;
    write_frame(
        &mut ready,
        &Message::Ping(Ping {
            nonce: nonce.clone(),
        }),
    )
    .await?;
    let reply = read_frame(&mut ready).await?;
    assert!(
        matches!(&reply,Some(Message::Pong(p)) if p.nonce==nonce),
        "READY reply: {reply:?}"
    );
    let db = rusqlite::Connection::open(a.paths.data.join("state.sqlite3"))?;
    // Keepalive is independent of the asynchronous readiness write.
    wait_established(&db, &b.identity.device_id).await?;
    write_frame(
        &mut denied,
        &Message::Error(ErrorMessage {
            code: ErrorCode::VersionMismatch,
            message: None,
            supported_version: Some(1),
        }),
    )
    .await?;
    wait_proxy_closed(&mut denied).await?;
    assert_eq!(a.ui_snapshot().devices[0].state, "connected");
    assert_eq!(a.paired_devices(), vec![b.identity.device_id.clone()]);
    assert_eq!(a.connected_devices(), vec![b.identity.device_id.clone()]);

    drop((ready, denied, db));
    a.request_shutdown();
    sa.await??;
    Ok(())
}

#[tokio::test]
async fn optional_hello_write_failure_retries_without_disabling_redial() -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    let aa = la.local_addr()?.to_string();
    let uri = a.start_pairing_with_addresses(vec![aa.clone()]).await?;
    let db = rusqlite::Connection::open(b.paths.data.join("state.sqlite3"))?;
    db.execute_batch("CREATE TRIGGER fail_hello BEFORE UPDATE OF role ON peers BEGIN SELECT RAISE(ABORT,'injected HELLO failure'); END;")?;
    // Keep the same port reserved while A is offline.
    let reservation = la.into_std()?;
    let la = TcpListener::from_std(reservation.try_clone()?)?;
    let sa = tokio::spawn(a.clone().serve(la, false));
    let sb = tokio::spawn(b.clone().serve(lb, false));
    let preview = b.join(&uri.build()?).await?;
    let attempt = preview["attempt"].as_str().unwrap();
    b.confirm_join(attempt).await?;
    until(|| {
        b.join_status(attempt).unwrap()["state"] == "paired" && !b.connected_devices().is_empty()
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (role, addresses): (String, String) = db.query_row(
        "SELECT role,addresses FROM peers WHERE id=?",
        [a.identity.device_id.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(role, "computer");
    assert_ne!(addresses, "[]");
    db.execute_batch("UPDATE peers SET name=''; DROP TRIGGER fail_hello;")?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let name: String = db.query_row(
                "SELECT name FROM peers WHERE id=?",
                [a.identity.device_id.as_str()],
                |r| r.get(0),
            )?;
            if name == a.config().name {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    a.request_shutdown();
    sa.await??;
    until(|| b.connected_devices().is_empty()).await?;
    let listener = TcpListener::from_std(reservation)?;
    let redial = tokio::time::timeout(Duration::from_secs(4), async {
        let mut redial = accept_proxy(&listener, &a).await?;
        assert!(matches!(read_frame(&mut redial).await?, Some(Message::Hello(h)) if h.device_id == b.identity.device_id));
        Ok::<_, anyhow::Error>(redial)
    })
    .await??;
    drop(redial);
    assert_eq!(b.join_status(attempt)?["state"], "paired");

    drop((db, listener));
    b.request_shutdown();
    sb.await??;
    Ok(())
}
