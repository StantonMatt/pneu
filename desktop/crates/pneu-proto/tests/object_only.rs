use pneu_proto::*;
use serde_json::{Value, json};

#[test]
fn peer_settings_self_direction_is_rejected_on_decode_and_encode() {
    let device = Id::from_bytes([1; 16]);
    for feature in ["clipboard", "automatic_media"] {
        let snapshot = PeerSettings {
            transfer_id: Id::from_bytes([2; 16]),
            settings: vec![PeerSetting {
                key: format!("{device}/{device}/{feature}"),
                enabled: false,
                hlc: 0,
                writer_device: device.clone(),
            }],
        };
        let message = Message::PeerSettings(snapshot.clone());
        assert!(snapshot.settings[0].validate().is_err());
        assert!(encode_frame(&message).is_err());
        // Build raw bytes to exercise the receive codec independently of encode.
        let payload = serde_json::to_vec(&snapshot).unwrap();
        let mut wire = ((payload.len() + 1) as u32).to_be_bytes().to_vec();
        wire.push(0x21);
        wire.extend(payload);
        assert!(decode_frame(&wire).is_err());
    }
}

fn fixture(kind: &str) -> Value {
    let vectors: Value =
        serde_json::from_str(include_str!("../../../../docs/protocol-vectors/v1.json")).unwrap();
    vectors["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["decoded"]["type"] == kind && v["decoded"]["payload"]["kind"] != "shared")
        .unwrap()["decoded"]["payload"]
        .clone()
}

macro_rules! object_only {
    ($test:ident, $ty:ty, $kind:literal, [$($field:literal),*]) => {
        #[test]
        fn $test() {
            let object = fixture($kind);
            let value: $ty = serde_json::from_value(object.clone()).unwrap();
            let encoded = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<$ty>(&encoded).unwrap(), value);
            let array = json!([$(&object[$field]),*]);
            assert!(serde_json::from_str::<$ty>(&array.to_string()).is_err(), stringify!($ty));
            assert!(serde_json::from_value::<$ty>(array.clone()).is_err());
            assert!(serde_json::from_value::<Message>(json!({"type": $kind, "payload": array})).is_err());
            for invalid in ["null", "true", "1", r#""object""#] {
                assert!(serde_json::from_str::<$ty>(invalid).is_err());
            }
        }
    };
}

object_only!(
    hello_requires_object,
    Hello,
    "hello",
    ["version", "device_id", "name", "capabilities"]
);
object_only!(
    pair_request_requires_object,
    PairRequest,
    "pair_request",
    ["token"]
);
object_only!(
    pair_result_requires_object,
    PairResult,
    "pair_result",
    ["accepted"]
);
object_only!(ping_requires_object, Ping, "ping", ["nonce"]);
object_only!(pong_requires_object, Ping, "pong", ["nonce"]);
object_only!(status_requires_object, Status, "status", ["paused"]);
object_only!(unpair_requires_object, Unpair, "unpair", []);
object_only!(
    file_begin_requires_object,
    FileBegin,
    "file_begin",
    [
        "transfer_id",
        "file_name",
        "mime",
        "size",
        "sha256",
        "kind",
        "created_at",
        "source_media_id"
    ]
);
// FILE_CHUNK uses binary on wire; its fixture/debug JSON still requires an object.
object_only!(
    file_chunk_requires_object,
    FileChunk,
    "file_chunk",
    ["transfer_id", "offset", "data"]
);
object_only!(
    file_end_requires_object,
    FileEnd,
    "file_end",
    ["transfer_id"]
);
object_only!(
    ack_requires_object,
    Ack,
    "ack",
    ["transfer_id", "deduplicated"]
);
object_only!(
    nack_requires_object,
    Nack,
    "nack",
    ["transfer_id", "code", "retryable", "message"]
);
object_only!(
    clipboard_requires_object,
    Clipboard,
    "clipboard",
    ["transfer_id", "text", "created_at"]
);
object_only!(
    peer_settings_requires_object,
    PeerSettings,
    "peer_settings",
    ["transfer_id", "settings"]
);
object_only!(
    error_requires_object,
    ErrorMessage,
    "error",
    ["code", "message", "supported_version"]
);

#[test]
fn peer_setting_requires_object() {
    let object = fixture("peer_settings")["settings"][0].clone();
    let value: PeerSetting = serde_json::from_value(object.clone()).unwrap();
    value.validate().unwrap();
    assert_eq!(serde_json::to_value(&value).unwrap(), object);
    let array = json!([
        object["key"],
        object["enabled"],
        object["hlc"],
        object["writer_device"]
    ]);
    assert!(serde_json::from_str::<PeerSetting>(&array.to_string()).is_err());
    assert!(serde_json::from_value::<PeerSetting>(array).is_err());
}

#[test]
fn nested_setting_array_is_rejected_by_frame_and_fixture_decoders() {
    let mut invalid = fixture("peer_settings");
    let entry = &invalid["settings"][0];
    invalid["settings"][0] = json!([
        entry["key"],
        entry["enabled"],
        entry["hlc"],
        entry["writer_device"]
    ]);
    let payload = serde_json::to_vec(&invalid).unwrap();
    let mut frame = ((payload.len() + 1) as u32).to_be_bytes().to_vec();
    frame.push(0x21);
    frame.extend_from_slice(&payload);
    assert!(decode_frame(&frame).is_err());
    assert!(
        serde_json::from_value::<Message>(json!({"type": "peer_settings", "payload": invalid}))
            .is_err()
    );
}

#[test]
fn scalar_hex_types_reject_arrays() {
    assert!(serde_json::from_str::<Id>(r#"["00112233445566778899aabbccddeeff"]"#).is_err());
    assert!(serde_json::from_str::<Hash>(&format!("[\"{}\"]", "b".repeat(64))).is_err());
    assert!(serde_json::from_str::<Nonce>(r#"["0000000000000001"]"#).is_err());
}
