use pneu_proto::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn controls() -> Vec<(String, u8, Value)> {
    let vectors: Value =
        serde_json::from_str(include_str!("../../../../docs/protocol-vectors/v1.json")).unwrap();
    vectors["frames"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["decoded"]["type"] != "file_chunk")
        .map(|v| {
            (
                v["name"].as_str().unwrap().to_owned(),
                hex::decode(v["frame_hex"].as_str().unwrap()).unwrap()[4],
                v["decoded"]["payload"].clone(),
            )
        })
        .collect()
}

fn accepts(type_id: u8, payload: &str) -> bool {
    let mut wire = ((payload.len() + 1) as u32).to_be_bytes().to_vec();
    wire.push(type_id);
    wire.extend_from_slice(payload.as_bytes());
    decode_frame(&wire).is_ok()
}

fn same_type(a: &Value, b: &Value) -> bool {
    matches!(
        (a, b),
        (Value::Bool(_), Value::Bool(_))
            | (Value::Number(_), Value::Number(_))
            | (Value::String(_), Value::String(_))
            | (Value::Array(_), Value::Array(_))
            | (Value::Object(_), Value::Object(_))
    )
}

// Exercise the actual frame decoder, retaining raw JSON for duplicate fields and
// numeric spellings that a JSON value tree would otherwise normalize away.
#[test]
fn every_known_field_rejects_wrong_types_null_and_duplicates() {
    let mut covered = BTreeMap::<u8, BTreeSet<String>>::new();
    let mut nested = BTreeSet::new();
    for (name, type_id, payload) in controls() {
        assert!(accepts(type_id, &payload.to_string()), "{name}");
        let mut objects = vec![(false, payload.clone())];
        if type_id == 0x21 {
            objects.extend(
                payload["settings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned()
                    .map(|v| (true, v)),
            );
        }
        for (is_nested, object) in objects {
            let wrap = |value: &Value| {
                if is_nested {
                    let mut body = payload.clone();
                    body["settings"] = json!([value]);
                    body
                } else {
                    value.clone()
                }
            };
            for (field, value) in object.as_object().unwrap() {
                if is_nested {
                    nested.insert(field.clone());
                } else {
                    covered.entry(type_id).or_default().insert(field.clone());
                }
                let optional = !is_nested
                    && match type_id {
                        0x01 => matches!(field.as_str(), "paused" | "role" | "addresses"),
                        0x03 | 0x14 | 0x7f => field == "message",
                        0x10 => {
                            matches!(field.as_str(), "source_media_id" | "sensitive" | "folder")
                                || (field == "resume" && payload.get("folder").is_none())
                        }
                        0x33 => matches!(field.as_str(), "title" | "text" | "icon_png"),
                        0x20 => matches!(field.as_str(), "origin" | "sensitive"),
                        _ => false,
                    };
                let mut missing = object.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert_eq!(
                    accepts(type_id, &wrap(&missing).to_string()),
                    optional,
                    "missing {name}.{field}"
                );
                for wrong in [
                    json!(null),
                    json!(true),
                    json!(0),
                    json!("x"),
                    json!([]),
                    json!({}),
                ] {
                    if same_type(value, &wrong) {
                        continue;
                    }
                    let mut changed = object.clone();
                    changed[field] = wrong.clone();
                    assert!(
                        !accepts(type_id, &wrap(&changed).to_string()),
                        "{name}.{field}: {wrong}"
                    );
                }
                let text = object.to_string();
                let escaped = format!(r#""\u{:04x}{}""#, field.as_bytes()[0], &field[1..]);
                for key in [json!(field).to_string(), escaped] {
                    let duplicate = format!("{},{key}:{value}}}", &text[..text.len() - 1]);
                    let text = if is_nested {
                        format!(
                            r#"{{"transfer_id":{},"settings":[{duplicate}]}}"#,
                            payload["transfer_id"]
                        )
                    } else {
                        duplicate
                    };
                    assert!(!accepts(type_id, &text), "duplicate {name}.{field}");
                }
            }
            let text = object.to_string();
            let separator = if object.as_object().unwrap().is_empty() {
                ""
            } else {
                ","
            };
            let unknown = format!(
                r#"{}{separator}"future":{{"paused":null,"hlc":[true]}},"future":[null,1,"x"]}}"#,
                &text[..text.len() - 1]
            );
            let text = if is_nested {
                format!(
                    r#"{{"transfer_id":{},"settings":[{unknown}]}}"#,
                    payload["transfer_id"]
                )
            } else {
                unknown
            };
            assert!(accepts(type_id, &text), "unknown fields in {name}");
        }
    }
    let expected = [
        (
            0x01,
            "version device_id name capabilities paused role addresses",
        ),
        (0x02, "token"),
        (0x03, "accepted code message"),
        (0x04, "nonce"),
        (0x05, "nonce"),
        (0x06, "paused"),
        (
            0x10,
            "transfer_id file_name mime size sha256 kind created_at source_media_id hlc source_device sensitive resume folder",
        ),
        (0x12, "transfer_id"),
        (0x13, "transfer_id deduplicated"),
        (0x14, "transfer_id code retryable message"),
        (0x15, "transfer_id offset"),
        (0x16, "transfer_id"),
        (0x30, "level charging"),
        (0x31, "action"),
        (0x32, "ringing"),
        (
            0x33,
            "action key app_package app_label time title text icon_png",
        ),
        (
            0x20,
            "transfer_id text created_at origin hlc source_device sensitive",
        ),
        (0x21, "transfer_id settings"),
        (0x7f, "code message supported_version"),
    ];
    for (type_id, fields) in expected {
        assert_eq!(
            covered[&type_id],
            fields.split_whitespace().map(str::to_owned).collect()
        );
    }
    assert_eq!(
        nested,
        "key enabled hlc writer_device"
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    );
}

#[test]
fn unknown_fields_ignore_values_without_validating_them_as_known_fields() {
    for value in [
        "null".to_owned(),
        "1e9999".into(),
        "18446744073709551616".into(),
        r#""\ud800""#.into(),
        format!("{}0{}", "[".repeat(140), "]".repeat(140)),
    ] {
        for key in ["future", r"future\ud83d\ude00"] {
            assert!(
                accepts(0x07, &format!(r#"{{"{key}":{value}}}"#)),
                "{key}: {value}"
            );
        }
    }
}

#[test]
fn integer_tokens_and_bounds_for_every_numeric_field() {
    for (fixture, field, min, max) in [
        ("hello_phone", "version", 1, u32::MAX as u64),
        ("empty_begin", "size", 0, MAX_FILE_SIZE),
        ("empty_begin", "created_at", 0, MAX_TIMESTAMP),
        ("clipboard_image_mesh", "hlc", 0, u64::MAX),
        ("clipboard_clear", "created_at", 0, MAX_TIMESTAMP),
        ("clipboard_mesh", "hlc", 0, u64::MAX),
        ("peer_settings", "hlc", 0, u64::MAX),
        (
            "error_version_mismatch",
            "supported_version",
            1,
            u32::MAX as u64,
        ),
    ] {
        let (_, type_id, payload) = controls().into_iter().find(|v| v.0 == fixture).unwrap();
        let raw = |token: &str| {
            let mut body = payload.clone();
            let target = if type_id == 0x21 {
                &mut body["settings"][0]
            } else {
                &mut body
            };
            target[field] = json!("NUMBER_TOKEN");
            body.to_string().replace("\"NUMBER_TOKEN\"", token)
        };
        for token in [min.to_string(), "1".into(), max.to_string()] {
            assert!(accepts(type_id, &raw(&token)), "{fixture}.{field}: {token}");
        }
        for token in [
            "-0".into(),
            "-1".into(),
            "1.0".into(),
            "1e0".into(),
            "1E+0".into(),
            "\"1\"".into(),
            "01".into(),
            "+1".into(),
            (max as u128 + 1).to_string(),
        ] {
            assert!(
                !accepts(type_id, &raw(&token)),
                "{fixture}.{field}: {token}"
            );
        }
        if min == 1 {
            assert!(!accepts(type_id, &raw("0")));
        }
    }
}

#[test]
fn all_wire_enums_are_strings_in_frames_and_direct_deserialization() {
    macro_rules! check {
        ($ty:ty, [$($wire:literal),+]) => {
            $(
                let value: $ty = serde_json::from_value(json!($wire)).unwrap();
                assert_eq!(serde_json::to_value(value).unwrap(), json!($wire));
                for invalid in [json!({$wire: null}), json!({$wire: []}), json!([$wire]), json!(0), json!(true), json!(null)] {
                    assert!(serde_json::from_str::<$ty>(&invalid.to_string()).is_err(), "{}: {invalid}", stringify!($ty));
                    assert!(serde_json::from_value::<$ty>(invalid).is_err());
                }
            )+
        };
    }
    check!(DeviceRole, ["phone", "computer"]);
    check!(
        PairCode,
        [
            "invalid_token",
            "expired_token",
            "pairing_closed",
            "already_paired",
            "storage_failed"
        ]
    );
    check!(
        FileKind,
        [
            "screenshot",
            "photo",
            "recording",
            "file",
            "clipboard_image"
        ]
    );
    check!(
        NackCode,
        [
            "hash_mismatch",
            "size_mismatch",
            "storage_failed",
            "disk_full",
            "not_writable",
            "unsupported_kind",
            "id_conflict",
            "clipboard_unavailable"
        ]
    );
    check!(RingAction, ["start", "stop"]);
    check!(NotificationAction, ["clear", "remove", "upsert"]);
    check!(FolderEntryType, ["file", "directory"]);
    check!(ClipboardOrigin, ["clipboard", "share"]);
    check!(
        ErrorCode,
        [
            "version_mismatch",
            "unauthorized",
            "unexpected_message",
            "malformed_message",
            "frame_too_large",
            "unknown_message",
            "timeout",
            "busy",
            "connection_replaced",
            "closing"
        ]
    );
}

#[test]
fn string_constraints_and_array_elements() {
    for (name, type_id, payload) in controls() {
        let mut objects = vec![(false, payload.clone())];
        if type_id == 0x21 {
            objects.extend(
                payload["settings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned()
                    .map(|v| (true, v)),
            );
        }
        for (nested, object) in objects {
            for (field, value) in object.as_object().unwrap() {
                let check = |replacement: Value, valid: bool| {
                    let mut changed = object.clone();
                    changed[field] = replacement.clone();
                    let body = if nested {
                        let mut body = payload.clone();
                        body["settings"] = json!([changed]);
                        body
                    } else {
                        changed
                    };
                    assert_eq!(
                        accepts(type_id, &body.to_string()),
                        valid,
                        "{name}.{field}: {replacement}"
                    );
                };
                if value.is_string() {
                    check(json!("\0"), false);
                    let mut changed = object.clone();
                    changed[field] = json!("SURROGATE_TOKEN");
                    let mut body = payload.clone();
                    if nested {
                        body["settings"] = json!([changed]);
                    } else {
                        body = changed;
                    }
                    for token in [r#""\ud800""#, r#""\udc00""#, r#""\udc00\ud800""#] {
                        assert!(
                            !accepts(
                                type_id,
                                &body.to_string().replace("\"SURROGATE_TOKEN\"", token)
                            ),
                            "{name}.{field}: {token}"
                        );
                    }
                    let digits = match field.as_str() {
                        "device_id" | "transfer_id" | "source_device" | "writer_device" => Some(32),
                        "sha256" | "token" => Some(64),
                        "nonce" => Some(16),
                        _ => None,
                    };
                    if let Some(n) = digits {
                        for text in [
                            "a".repeat(n - 1),
                            "a".repeat(n + 1),
                            "A".repeat(n),
                            "g".repeat(n),
                        ] {
                            check(json!(text), false);
                        }
                    }
                    let bounds = match field.as_str() {
                        "name" => Some((1, 128)),
                        "file_name" => Some((1, 255)),
                        "mime" if payload.get("folder").is_none() => Some((1, 255)),
                        "app_package" => Some((1, 255)),
                        "app_label" => Some((1, 128)),
                        "title" => Some((0, 1024)),
                        "text" if type_id == 0x33 => Some((0, 8192)),
                        "source_media_id" => Some((1, 256)),
                        "message" => Some((0, 512)),
                        "text" => Some((0, MAX_CLIPBOARD_BYTES)),
                        _ => None,
                    };
                    if let Some((min, max)) = bounds {
                        let prefix = if field == "mime" { "image/" } else { "" };
                        check(
                            json!(if min == 0 {
                                "".into()
                            } else {
                                format!("{prefix}x")
                            }),
                            true,
                        );
                        let remaining = max - prefix.len();
                        let at_limit = format!(
                            "{prefix}{}{}",
                            "é".repeat(remaining / 2),
                            "x".repeat(remaining % 2)
                        );
                        check(json!(at_limit), true);
                        check(json!(format!("{at_limit}x")), false);
                        if min == 1 {
                            check(json!(""), false);
                        }
                    }
                    if field == "file_name" {
                        for bad in [".", "..", "a/b", "a\\b"] {
                            check(json!(bad), false);
                        }
                    }
                } else if let Some(array) = value.as_array() {
                    for wrong in [json!(null), json!(true), json!(0), json!([]), json!({})] {
                        check(json!([wrong]), false);
                    }
                    if field == "capabilities" {
                        for n in [16, 17] {
                            check(
                                json!((0..n).map(|i| format!("cap{i}")).collect::<Vec<_>>()),
                                n == 16,
                            );
                        }
                        check(json!(["é".repeat(32)]), true);
                        check(json!(["é".repeat(32) + "x"]), false);
                        check(json!([""]), false);
                        check(json!(["\0"]), false);
                        check(json!(["files", "files"]), false);
                        check(json!(["clipboard_image"]), false);
                    } else if field == "addresses" {
                        check(json!([]), true);
                        for n in [8, 9] {
                            check(
                                json!((0..n).map(|i| format!("pc{i}.local:1")).collect::<Vec<_>>()),
                                n == 8,
                            );
                        }
                    } else {
                        assert_eq!(field, "settings");
                        check(json!([]), false);
                        let mut too_many = array.clone();
                        too_many.resize(4, array[0].clone());
                        check(json!(too_many), false);
                    }
                }
            }
        }
    }
}

#[test]
fn hello_address_grammar() {
    let (_, type_id, mut payload) = controls()
        .into_iter()
        .find(|v| v.0 == "hello_mesh_computer")
        .unwrap();
    let max_host = format!(
        "{}.{}.{}.{}",
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61)
    );
    let valid = [
        "host:1",
        "host:65535",
        "HOST.local:00001",
        "192.0.2.1:1",
        "[::1]:1",
        "[2001:db8::1]:1",
        "[::ffff:192.0.2.1]:1",
        "[::192.0.2.1]:1",
    ];
    let invalid = [
        "host:0",
        "host:65536",
        "host:+1",
        "host: 1",
        "host",
        "host.:1",
        "-host:1",
        "host_:1",
        "user@host:1",
        "host/path:1",
        "123:1",
        "999.1.1.1:1",
        "01.2.3.4:1",
        "[127.0.0.1]:1",
        "::1:1",
        "[fe80::1%eth0]:1",
        "[1::2::3]:1",
        "[00000::1]:1",
        "[::ffff:192.168.001.1]:1",
        "[::192.168.001.1]:1",
        "[::ffff:1.2.3]:1",
        "[::1]:1junk",
        "höst:1",
        " host:1",
    ];
    for (address, expected) in valid
        .into_iter()
        .map(|s| (s.to_owned(), true))
        .chain(invalid.into_iter().map(|s| (s.to_owned(), false)))
        .chain([
            (format!("{max_host}:1"), true),
            (format!("{max_host}x:1"), false),
            (format!("{}.local:1", "x".repeat(64)), false),
        ])
    {
        payload["addresses"] = json!([address]);
        assert_eq!(
            accepts(type_id, &payload.to_string()),
            expected,
            "{address}"
        );
    }
}
