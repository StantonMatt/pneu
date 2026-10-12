use pneu_proto::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Deserialize)]
struct Vectors {
    protocol_version: u32,
    frames: Vec<FrameVector>,
    transfers: Vec<TransferVector>,
    pairing_uris: Vec<PairVector>,
    invalid_frames: Vec<InvalidFrame>,
    invalid_pairing_uris: Vec<InvalidUri>,
    fingerprints: Vec<FingerprintVector>,
}
#[derive(Deserialize)]
struct FrameVector {
    name: String,
    frame_hex: String,
    decoded: Message,
}
#[derive(Deserialize)]
struct TransferVector {
    name: String,
    frames: Vec<String>,
    data_hex: String,
    size: u64,
    sha256: String,
}
#[derive(Deserialize)]
struct PairVector {
    name: String,
    uri: String,
    canonical_uri: String,
    decoded: PairingUri,
}
#[derive(Deserialize)]
struct InvalidFrame {
    name: String,
    frame_hex: String,
}
#[derive(Deserialize)]
struct InvalidUri {
    name: String,
    uri: String,
}
#[derive(Deserialize)]
struct FingerprintVector {
    der_hex: String,
    sha256: String,
}

fn vectors() -> Vectors {
    // This is the only JSON fixture in this directory; all entries are tested.
    serde_json::from_str(include_str!("../../../../docs/protocol-vectors/v1.json")).unwrap()
}

#[test]
fn every_frame_decodes_and_roundtrips() {
    let vectors = vectors();
    assert_eq!(vectors.protocol_version, PROTOCOL_VERSION);
    let mut ids = std::collections::BTreeSet::new();
    for vector in vectors.frames {
        let raw = hex::decode(&vector.frame_hex).unwrap();
        let decoded = decode_frame(&raw).unwrap_or_else(|e| panic!("{}: {e}", vector.name));
        assert_eq!(decoded, vector.decoded, "{}", vector.name);
        let encoded = encode_frame(&decoded).unwrap();
        assert_eq!(
            decode_frame(&encoded).unwrap(),
            vector.decoded,
            "{}",
            vector.name
        );
        if matches!(decoded, Message::FileChunk(_)) {
            assert_eq!(encoded, raw);
        }
        ids.insert(decoded.type_id());
    }
    assert_eq!(
        ids.into_iter().collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 32, 127]
    );
}

#[test]
fn every_invalid_frame_is_rejected() {
    for vector in vectors().invalid_frames {
        assert!(
            decode_frame(&hex::decode(vector.frame_hex).unwrap()).is_err(),
            "{}",
            vector.name
        );
    }
}

#[tokio::test]
async fn every_vector_passes_the_async_codec() {
    let vectors = vectors();
    for vector in vectors.frames {
        let raw = hex::decode(vector.frame_hex).unwrap();
        let mut input = raw.as_slice();
        assert_eq!(
            read_frame(&mut input).await.unwrap(),
            Some(vector.decoded.clone()),
            "{}",
            vector.name
        );
        assert_eq!(read_frame(&mut input).await.unwrap(), None);
        let mut output = Vec::new();
        write_frame(&mut output, &vector.decoded).await.unwrap();
        assert_eq!(decode_frame(&output).unwrap(), vector.decoded);
    }
    for vector in vectors.invalid_frames {
        let raw = hex::decode(vector.frame_hex).unwrap();
        assert!(
            read_frame(&mut raw.as_slice()).await.is_err(),
            "{}",
            vector.name
        );
    }
}

#[test]
fn all_pairing_and_fingerprint_vectors() {
    let vectors = vectors();
    for vector in vectors.pairing_uris {
        let parsed =
            PairingUri::parse(&vector.uri).unwrap_or_else(|e| panic!("{}: {e}", vector.name));
        assert_eq!(parsed, vector.decoded, "{}", vector.name);
        assert_eq!(
            parsed.build().unwrap(),
            vector.canonical_uri,
            "{}",
            vector.name
        );
        assert_eq!(PairingUri::parse(&parsed.build().unwrap()).unwrap(), parsed);
    }
    for vector in vectors.invalid_pairing_uris {
        assert!(PairingUri::parse(&vector.uri).is_err(), "{}", vector.name);
    }
    for vector in vectors.fingerprints {
        assert_eq!(
            certificate_fingerprint(&hex::decode(vector.der_hex).unwrap()).as_str(),
            vector.sha256
        );
    }
}

#[test]
fn all_transfer_vectors_reassemble_and_verify() {
    let vectors = vectors();
    let frames: HashMap<_, _> = vectors
        .frames
        .iter()
        .map(|f| (f.name.as_str(), &f.decoded))
        .collect();
    for transfer in vectors.transfers {
        let mut bytes = Vec::new();
        let mut begin = None;
        let mut ended = false;
        let mut acked = false;
        for name in &transfer.frames {
            match frames[name.as_str()] {
                Message::FileBegin(m) => {
                    assert!(begin.is_none());
                    begin = Some(m);
                }
                Message::FileChunk(m) => {
                    assert!(!ended);
                    assert_eq!(m.transfer_id, begin.unwrap().transfer_id);
                    assert_eq!(m.offset, bytes.len() as u64);
                    bytes.extend_from_slice(&m.data);
                }
                Message::FileEnd(m) => {
                    assert!(!ended);
                    assert_eq!(m.transfer_id, begin.unwrap().transfer_id);
                    ended = true;
                }
                Message::Ack(m) => {
                    assert!(ended && !acked);
                    assert_eq!(m.transfer_id, begin.unwrap().transfer_id);
                    assert!(!m.deduplicated);
                    acked = true;
                }
                _ => panic!("unexpected message in {}", transfer.name),
            }
        }
        let begin = begin.unwrap();
        assert!(ended && acked);
        assert_eq!(bytes, hex::decode(transfer.data_hex).unwrap());
        assert_eq!(bytes.len() as u64, transfer.size);
        assert_eq!(begin.size, transfer.size);
        assert_eq!(hex::encode(Sha256::digest(&bytes)), transfer.sha256);
        assert_eq!(begin.sha256.as_str(), transfer.sha256);
    }
}
