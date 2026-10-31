use pneu_proto::*;
use serde::Deserialize;
#[derive(Deserialize)]
struct Vector {
    name: String,
    payload: String,
    prefix_hex: String,
}
fn vectors() -> Vec<Vector> {
    serde_json::from_str(include_str!("../../../protocol-vectors/setup.json")).unwrap()
}
fn frame(kind: u8, raw: &str) -> Vec<u8> {
    let mut bytes = ((raw.len() + 1) as u32).to_be_bytes().to_vec();
    bytes.push(kind);
    bytes.extend_from_slice(raw.as_bytes());
    bytes
}
#[tokio::test]
async fn setup_spec_golden_frames_and_roundtrips() {
    for (v, prefix) in vectors().into_iter().zip([
        "000003e222",
        "0000036422",
        "000003e222",
        "000003e222",
        "000003e322",
        "000004e322",
    ]) {
        assert_eq!(v.prefix_hex, prefix, "{}", v.name);
        let raw = frame(0x22, &v.payload);
        assert_eq!(hex::encode(&raw[..5]), prefix);
        let m = decode_frame(&raw).unwrap();
        assert_eq!(decode_frame(&encode_frame(&m).unwrap()).unwrap(), m);
        assert_eq!(read_frame(&mut raw.as_slice()).await.unwrap(), Some(m));
    }
}
#[test]
fn setup_nested_strictness_and_unsigned_clocks() {
    let valid = vectors().remove(0).payload;
    for replacement in [
        "1,\"hlc\":2",
        "1e0",
        "1.0",
        "-0",
        "18446744073709551616",
        "null",
        "\"1\"",
    ] {
        let bad = valid.replace("\"hlc\":1800000000003", &format!("\"hlc\":{replacement}"));
        assert!(decode_frame(&frame(0x22, &bad)).is_err(), "{bad}");
    }
    for (from, to) in [
        ("\"reach\":{", "\"reach\":{\"hlc\":1,"),
        ("\"relay\":[]", "\"relay\":[{}]"),
        ("\"name\":\"Laptop\"", "\"name\":null"),
        ("\"name\":\"Laptop\"", "\"name\":\"\\u0000\""),
        ("\"name\":\"Laptop\"", "\"name\":\"\\ud800\""),
        ("\"status\":\"member\"", "\"status\":\"other\""),
        (
            "\"setup_id\":\"00000000000000000000000000000000\"",
            "\"setup_id\":null",
        ),
    ] {
        assert!(decode_frame(&frame(0x22, &valid.replace(from, to))).is_err());
    }
    for hlc in [0, u64::MAX] {
        assert!(
            decode_frame(&frame(
                0x22,
                &valid.replace("1800000000003", &hlc.to_string())
            ))
            .is_ok()
        );
    }
    for (name, good) in [("a".repeat(129), false), ("é".repeat(64), true)] {
        assert_eq!(
            decode_frame(&frame(
                0x22,
                &valid.replace(
                    "\"name\":\"Laptop\"",
                    &format!("\"name\":\"{name}\",\"unknown\":true")
                )
            ))
            .is_ok(),
            good
        );
    }
}
#[test]
fn setup_hello_capability_dependencies_and_scope() {
    let base = r#"{"version":1,"device_id":"11111111111111111111111111111111","name":"A","capabilities":["mesh","setup"],"role":"computer","setup":{"scope":"enabled"}}"#;
    assert!(decode_frame(&frame(1, base)).is_ok());
    for bad in [
        base.replace("\"mesh\",", ""),
        base.replace(",\"role\":\"computer\"", ""),
        base.replace(",\"setup\":{\"scope\":\"enabled\"}", ""),
        base.replace(
            "\"scope\":\"enabled\"",
            "\"scope\":\"enabled\",\"setup_id\":null",
        ),
        base.replace(
            "\"scope\":\"enabled\"",
            "\"scope\":\"ask\",\"scope\":\"enabled\"",
        ),
        base.replace(
            "\"scope\":\"enabled\"",
            "\"scope\":\"disabled\",\"setup_id\":\"00000000000000000000000000000000\"",
        ),
    ] {
        assert!(decode_frame(&frame(1, &bad)).is_err(), "{bad}");
    }
}
#[test]
fn setup_roster_limits_identity_uniqueness_and_reach_rules() {
    let Message::Members(original) =
        decode_frame(&frame(0x22, &vectors().remove(0).payload)).unwrap()
    else {
        unreachable!()
    };
    for edit in 0..7 {
        let mut m = original.clone();
        let duplicate = m.members[0].clone();
        let c = &mut m.members[2];
        match edit {
            0 => c.status = MemberStatus::Removed,
            1 => c.role = DeviceRole::Phone,
            2 => c.reach.as_mut().unwrap().writer_device = Id::from_bytes([7; 16]),
            3 => {
                let a = c.reach.as_ref().unwrap().addresses[0].clone();
                c.reach.as_mut().unwrap().addresses.push(a);
            }
            4 => c.fingerprint = duplicate.fingerprint,
            5 => c.device_id = duplicate.device_id,
            _ => m.members.push(duplicate),
        }
        assert!(m.validate().is_err());
    }
    let template = original.members[0].clone();
    for (count, good) in [(17, true), (18, false), (64, true), (65, false)] {
        let m = Members {
            members: (0..count)
                .map(|n| {
                    let mut v = template.clone();
                    v.device_id = Id::from_bytes([n; 16]);
                    v.fingerprint = Hash::new(format!("{n:064x}")).unwrap();
                    if count > 18 {
                        v.status = MemberStatus::Removed;
                    }
                    v
                })
                .collect(),
            ..original.clone()
        };
        assert_eq!(m.validate().is_ok(), good);
    }
    for (count, good) in [(4, true), (5, false)] {
        let m = Members {
            members: (0..count)
                .map(|n| {
                    let mut v = template.clone();
                    v.status = MemberStatus::Removed;
                    v.fingerprint = Hash::new(format!("{n:064x}")).unwrap();
                    v
                })
                .collect(),
            ..original.clone()
        };
        assert_eq!(m.validate().is_ok(), good);
    }
}
#[test]
fn setup_encoded_payload_boundary() {
    let valid = vectors().remove(0).payload;
    let prefix = format!("{},\"unknown\":\"", &valid[..valid.len() - 1]);
    for (size, good) in [(65536, true), (65537, false)] {
        let raw = format!("{prefix}{}\"}}", "x".repeat(size - prefix.len() - 2));
        assert_eq!(raw.len(), size);
        assert_eq!(decode_frame(&frame(0x22, &raw)).is_ok(), good);
    }
}

#[test]
fn shared_readmission_fixtures_use_existing_members_wire_format() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../protocol-vectors/readmission.json")).unwrap();
    for (name, fixture) in vectors["fixtures"].as_object().unwrap() {
        let raw = serde_json::to_string(&fixture["payload"]).unwrap();
        let message = decode_frame(&frame(0x22, &raw)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            decode_frame(&encode_frame(&message).unwrap()).unwrap(),
            message,
            "{name}"
        );
    }
}
