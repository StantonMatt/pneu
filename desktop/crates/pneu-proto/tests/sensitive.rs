use pneu_proto::*;

#[test]
fn sensitive_is_optional_boolean_and_only_true_is_serialized()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let old = serde_json::json!({"transfer_id":"11111111111111111111111111111111","text":"secret","created_at":1});
    let mut text: Clipboard = serde_json::from_value(old.clone())?;
    assert!(!text.sensitive);
    assert!(serde_json::to_value(&text)?.get("sensitive").is_none());
    text.sensitive = true;
    assert_eq!(serde_json::to_value(&text)?["sensitive"], true);
    assert!(
        serde_json::from_value::<Clipboard>({
            let mut v = old;
            v["sensitive"] = serde_json::Value::Null;
            v
        })
        .is_err()
    );
    let mut image: FileBegin = serde_json::from_value(
        serde_json::json!({"transfer_id":"11111111111111111111111111111111","file_name":"clipboard.png","mime":"image/png","size":1,"sha256":"a".repeat(64),"kind":"clipboard_image","created_at":1}),
    )?;
    assert!(!image.sensitive);
    assert!(serde_json::to_value(&image)?.get("sensitive").is_none());
    image.sensitive = true;
    assert_eq!(serde_json::to_value(&image)?["sensitive"], true);
    assert!(serde_json::from_value::<FileBegin>(serde_json::to_value(&image)?)?.sensitive);
    Ok(())
}
