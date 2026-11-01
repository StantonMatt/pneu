use pneu_proto::*;
use serde_json::{Value, json};

fn begin(bytes: &[u8], extra: u64) -> FileBegin {
    serde_json::from_value(json!({
        "transfer_id":"00112233445566778899aabbccddeeff", "file_name":"Tree", "mime":FOLDER_MIME,
        "size":bytes.len() as u64 + extra,"sha256":"0".repeat(64),"kind":"file","created_at":0,
        "resume":true,"folder":{"manifest_len":bytes.len()}
    }))
    .unwrap()
}
fn accepts_manifest(bytes: &[u8], extra: u64) -> bool {
    FolderManifest::parse(bytes, &begin(bytes, extra)).is_ok()
}
#[test]
fn folder_structural_paths_and_boundaries() {
    for good in [
        ".pneu-staging",
        "CON",
        "xx:ads",
        "%2e%2e",
        "A/empty",
        "a/empty",
        "é",
    ] {
        validate_folder_path(good).unwrap();
    }
    for bad in [
        "",
        "/a",
        "a/",
        "a//b",
        ".",
        "..",
        "a/../b",
        "a\\b",
        "a\0b",
        "C:",
        "C:/x",
        "x/D:file",
        "//server/share",
        "\\\\?\\C:\\x",
    ] {
        assert!(validate_folder_path(bad).is_err(), "{bad:?}");
    }
    for n in [255, 256] {
        assert_eq!(validate_folder_path(&"a".repeat(n)).is_ok(), n == 255);
    }
    assert!(validate_folder_path(&"é".repeat(128)).is_err());
    assert!(validate_folder_path(&"é".repeat(127)).is_ok());
    assert!(validate_folder_path(&["x"; 32].join("/")).is_ok());
    assert!(validate_folder_path(&["x"; 33].join("/")).is_err());
    // Exactly 4096 bytes with 17 components; then one byte over, each component still valid.
    let path = format!("{}/{}", vec!["a".repeat(255); 16].join("/"), "b");
    assert_eq!(path.len(), 4097);
    assert!(validate_folder_path(&path).is_err());
    let path = format!("{}/{}", vec!["a".repeat(254); 16].join("/"), "b".repeat(16));
    assert_eq!(path.len(), 4096);
    assert!(validate_folder_path(&path).is_ok());
}
#[test]
fn manifest_schema_integer_and_tree_rejections() {
    for raw in [
        r#"[]"#,
        r#"{"entries":null}"#,
        r#"{"entries":[[]]}"#,
        r#"{"entries":[{"path":"x","path":"y","type":"directory"}]}"#,
        r#"{"entries":[{"path":"x","type":"directory","sha256":null}]}"#,
        r#"{"entries":[{"path":"x","type":"directory","size":null}]}"#,
        r#"{"entries":[{"path":"x","type":"file","size":0,"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"},{"path":"x/y","type":"directory"}]}"#,
        r#"{"entries":[{"path":"x","type":"file","size":-0,"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}]}"#,
        r#"{"entries":[{"path":"\ud800","type":"directory"}]}"#,
    ] {
        assert!(!accepts_manifest(raw.as_bytes(), 0), "{raw}");
    }
    assert!(!accepts_manifest(b"{\"entries\":[]}\xff", 0));
    let empty = b"{\"entries\":[]}";
    assert!(accepts_manifest(empty, 0));
    assert!(!accepts_manifest(empty, 1));
    let mut b = begin(empty, 0);
    b.folder.as_mut().unwrap().manifest_len -= 1;
    assert!(FolderManifest::parse(empty, &b).is_err());
    let unknown = br#"{"entries":[{"path":"x","type":"directory","future":null}],"future":true}"#;
    assert!(accepts_manifest(unknown, 0));
    for count in [10_000, 10_001] {
        let entries: Vec<Value> = (0..count)
            .map(|n| json!({"path":n.to_string(),"type":"directory"}))
            .collect();
        let raw = serde_json::to_vec(&json!({"entries":entries})).unwrap();
        assert_eq!(accepts_manifest(&raw, 0), count == 10_000);
    }
    // Unknown fields count toward exact encoded bytes; at-limit JSON is accepted.
    let mut raw = b"{\"entries\":[],\"future\":\"".to_vec();
    raw.resize(MAX_MANIFEST_BYTES - 2, b'x');
    raw.extend_from_slice(b"\"}");
    assert!(accepts_manifest(&raw, 0));
    raw.insert(raw.len() - 2, b'x');
    assert!(!accepts_manifest(&raw, 0));
}
#[test]
fn manifest_checked_sum_and_maximum_stream() {
    let hash = Hash::new("0".repeat(64)).unwrap();
    let mut m = FolderManifest {
        entries: vec![FolderEntry {
            path: "x".into(),
            entry_type: FolderEntryType::File,
            size: Some(MAX_FILE_SIZE - 1),
            sha256: Some(hash.clone()),
        }],
    };
    assert!(m.validate(1, MAX_FILE_SIZE).is_ok());
    m.entries[0].size = Some(MAX_FILE_SIZE);
    assert!(m.validate(1, MAX_FILE_SIZE).is_err());
    m.entries[0].size = Some(u64::MAX);
    assert!(m.validate(1, MAX_FILE_SIZE).is_err());
    m.entries[0].size = Some(1);
    m.entries.push(FolderEntry {
        path: "y".into(),
        entry_type: FolderEntryType::File,
        size: Some(MAX_FILE_SIZE),
        sha256: Some(hash),
    });
    assert!(m.validate(1, MAX_FILE_SIZE).is_err());
}
#[test]
fn file_identity_excludes_only_transport_fields() {
    let bytes = br#"{"entries":[]}"#;
    let a = begin(bytes, 0);
    let mut b = a.clone();
    for resume in [None, Some(false), Some(true)] {
        b.resume = resume;
        assert!(a.content_eq(&b));
    }
    b.folder.as_mut().unwrap().manifest_len += 1;
    assert!(!a.content_eq(&b));
    b = a.clone();
    b.file_name = "Other".into();
    assert!(!a.content_eq(&b));
    b = a.clone();
    b.sensitive = true;
    assert!(!a.content_eq(&b));
    let mut value = serde_json::to_value(&a).unwrap();
    value["future"] = json!(true);
    assert!(a.content_eq(&serde_json::from_value(value).unwrap()));
}
#[test]
fn ready_contextual_bounds_and_capability_dependencies() {
    let r = FileReady {
        transfer_id: Id::from_bytes([0; 16]),
        offset: 4,
    };
    Message::FileReady(r.clone()).validate().unwrap();
    assert!(r.validate_offset(3).is_err());
    r.validate_offset(4).unwrap();
    for (caps, valid) in [
        (vec![CAP_FILE_RESUME], false),
        (vec!["files", CAP_FILE_RESUME], true),
        (vec!["files", CAP_FOLDER], false),
        (vec!["files", CAP_FILE_RESUME, CAP_FOLDER], true),
    ] {
        let h: Hello = serde_json::from_value(
            json!({"version":1,"device_id":"0".repeat(32),"name":"X","capabilities":caps}),
        )
        .unwrap();
        assert_eq!(Message::Hello(h).validate().is_ok(), valid);
    }
}

#[test]
fn large_unknown_array_is_consumed_within_a_bounded_time() {
    // Four million primitive values are discarded, never stored in a JSON tree.
    let raw = format!(
        "{{\"entries\":[],\"future\":[{}0]}}",
        "0,".repeat((MAX_MANIFEST_BYTES - 32) / 2)
    );
    assert!(raw.len() <= MAX_MANIFEST_BYTES);
    let begin = begin(raw.as_bytes(), 0);
    let start = std::time::Instant::now();
    assert!(
        FolderManifest::parse(raw.as_bytes(), &begin)
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(15));
}
#[test]
fn entry_limit_rejects_before_deserializing_excess_entry() {
    let mut raw = String::from("{\"entries\":[");
    for n in 0..MAX_MANIFEST_ENTRIES {
        if n != 0 {
            raw.push(',');
        }
        raw.push_str(&format!("{{\"path\":\"{n}\",\"type\":\"directory\"}}"));
    }
    // If entry 10001 were deserialized, this would fail with a type error.
    raw.push_str(",false]}");
    let error = FolderManifest::parse(raw.as_bytes(), &begin(raw.as_bytes(), 0)).unwrap_err();
    assert!(
        error.to_string().contains("manifest entry count"),
        "{error}"
    );
}
