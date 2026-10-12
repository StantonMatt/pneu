use pneu_proto::*;
use tokio::io::AsyncWriteExt;

fn id() -> Id {
    Id::from_bytes([0x42; 16])
}
fn chunk(size: usize, offset: u64) -> Message {
    Message::FileChunk(FileChunk {
        transfer_id: id(),
        offset,
        data: vec![0xa5; size],
    })
}
fn ping() -> Message {
    Message::Ping(Ping {
        nonce: Nonce::new("0000000000000001").unwrap(),
    })
}

#[test]
fn chunk_size_and_offset_boundaries() {
    let max = chunk(CHUNK_SIZE, MAX_FILE_SIZE - CHUNK_SIZE as u64);
    let wire = encode_frame(&max).unwrap();
    assert_eq!(wire.len(), MAX_FRAME_SIZE + 4);
    assert_eq!(&wire[..4], &(MAX_FRAME_SIZE as u32).to_be_bytes());
    assert_eq!(decode_frame(&wire).unwrap(), max);
    assert!(encode_frame(&chunk(0, 0)).is_err());
    assert!(encode_frame(&chunk(CHUNK_SIZE + 1, 0)).is_err());
    assert!(encode_frame(&chunk(1, MAX_FILE_SIZE)).is_err());
    assert!(encode_frame(&chunk(1, u64::MAX)).is_err());
    assert!(encode_frame(&chunk(CHUNK_SIZE, MAX_FILE_SIZE - CHUNK_SIZE as u64 + 1)).is_err());
}

#[test]
fn control_limit_counts_encoded_bytes() {
    let payload = br#"{"nonce":"0000000000000001"}"#;
    let mut body = payload.to_vec();
    body.resize(MAX_CONTROL_PAYLOAD, b' ');
    let mut wire = ((body.len() + 1) as u32).to_be_bytes().to_vec();
    wire.push(0x04);
    wire.extend_from_slice(&body);
    assert_eq!(decode_frame(&wire).unwrap(), ping());
    body.push(b' ');
    wire = ((body.len() + 1) as u32).to_be_bytes().to_vec();
    wire.push(0x04);
    wire.extend_from_slice(&body);
    assert!(decode_frame(&wire).is_err());

    // Text bytes are within the clipboard limit, but JSON escapes exceed 64 KiB.
    let message = Message::Clipboard(Clipboard {
        transfer_id: id(),
        text: "\u{0001}".repeat(MAX_CLIPBOARD_BYTES),
        created_at: 0,
        origin: pneu_proto::ClipboardOrigin::Clipboard,
    });
    assert!(message.validate().is_ok());
    assert!(encode_frame(&message).is_err());
    let message = Message::Clipboard(Clipboard {
        transfer_id: id(),
        text: "é".repeat(MAX_CLIPBOARD_BYTES / 2 + 1),
        created_at: 0,
        origin: pneu_proto::ClipboardOrigin::Clipboard,
    });
    assert!(message.validate().is_err());
}

#[tokio::test]
async fn fragmented_and_coalesced_streams() {
    let messages = [
        ping(),
        chunk(7, 0),
        Message::Pong(Ping {
            nonce: Nonce::new("ffffffffffffffff").unwrap(),
        }),
    ];
    let wire: Vec<_> = messages
        .iter()
        .flat_map(|m| encode_frame(m).unwrap())
        .collect();
    // Capacity smaller than a frame forces fragmented reads and write backpressure.
    let (mut writer, mut reader) = tokio::io::duplex(3);
    let task = tokio::spawn(async move {
        for byte in wire {
            writer.write_all(&[byte]).await.unwrap();
        }
        writer.shutdown().await.unwrap();
    });
    for expected in messages {
        assert_eq!(read_frame(&mut reader).await.unwrap(), Some(expected));
    }
    assert_eq!(read_frame(&mut reader).await.unwrap(), None);
    task.await.unwrap();

    let wire = [
        encode_frame(&ping()).unwrap(),
        encode_frame(&ping()).unwrap(),
    ]
    .concat();
    let mut reader = wire.as_slice();
    assert_eq!(read_frame(&mut reader).await.unwrap(), Some(ping()));
    assert_eq!(read_frame(&mut reader).await.unwrap(), Some(ping()));
    assert_eq!(read_frame(&mut reader).await.unwrap(), None);
    assert!(decode_frame(&wire).is_err()); // complete-frame API rejects concatenation
}

#[tokio::test]
async fn limits_are_rejected_before_payload_reads() {
    let mut oversized: &[u8] = &u32::MAX.to_be_bytes();
    assert!(matches!(
        read_frame(&mut oversized).await,
        Err(ProtocolError::InvalidLength(_))
    ));
    let header = [
        ((MAX_CONTROL_PAYLOAD + 2) as u32).to_be_bytes().to_vec(),
        vec![0x04],
    ]
    .concat();
    assert!(matches!(
        read_frame(&mut header.as_slice()).await,
        Err(ProtocolError::InvalidLength(_))
    ));
    let unknown = [0, 0, 0, 100, 0xfe];
    assert!(matches!(
        read_frame(&mut unknown.as_slice()).await,
        Err(ProtocolError::UnknownType(0xfe))
    ));
}

#[tokio::test]
async fn every_partial_frame_is_an_error() {
    for message in [ping(), chunk(5, 0)] {
        let frame = encode_frame(&message).unwrap();
        for end in 1..frame.len() {
            assert!(
                read_frame(&mut &frame[..end]).await.is_err(),
                "truncation at {end}"
            );
        }
    }
    assert_eq!(read_frame(&mut &[][..]).await.unwrap(), None);
}

#[test]
fn address_and_uri_edge_cases() {
    for address in [
        "192.168.1.2:47821",
        "100.67.144.28:65535",
        "pc.local:1",
        "[::1]:47821",
    ] {
        validate_address(address).unwrap();
    }
    for address in [
        "host:0",
        "host:65536",
        "host:+1",
        "host: 1",
        "host",
        "user@host:47821",
        "host/path:47821",
        "[fe80::1%eth0]:47821",
        "host.:47821",
        "-host:47821",
        "host_:47821",
    ] {
        assert!(validate_address(address).is_err(), "{address}");
    }
    let mut uri = PairingUri {
        version: 1,
        name: "PC & teléfono + = ?".into(),
        fingerprint: Hash::new("a".repeat(64)).unwrap(),
        addresses: vec!["pc.local:47821".into()],
        token: Hash::new("b".repeat(64)).unwrap(),
        expires_at: MAX_TIMESTAMP,
    };
    assert_eq!(PairingUri::parse(&uri.build().unwrap()).unwrap(), uri);
    uri.addresses = vec!["pc.local:47821".into(); 9];
    assert!(uri.build().is_err());
    uri.addresses.clear();
    assert!(uri.build().is_err());
}
