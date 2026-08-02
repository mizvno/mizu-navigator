//! Tests for the payload module.

use super::*;
use std::sync::Arc;

fn record(fields: Vec<(&str, Value)>) -> Value {
    Value::record_from_unsorted(fields)
}

#[tokio::test]
async fn json_default_matches_manual_serialisation() {
    let value = Value::from("hello".to_string());
    let expected = serde_json::to_vec(&crate::core::types::to_json(&value).unwrap()).unwrap();
    let got = serialize_payload(&value, PayloadFormat::Json)
        .await
        .unwrap();
    assert_eq!(got.bytes, expected);
    assert_eq!(got.content_type, "application/json");
}

#[tokio::test]
async fn text_requires_string() {
    assert!(
        serialize_payload(&Value::from("ok".to_string()), PayloadFormat::Text)
            .await
            .is_ok()
    );
    let err = serialize_payload(&Value::Decimal(100_000_000), PayloadFormat::Text)
        .await
        .unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

#[tokio::test]
async fn form_encodes_scalars_and_rejects_nested() {
    let value = record(vec![
        ("q", Value::from("a b&c=d".to_string())),
        ("n", Value::Decimal(150_000_000)), // 1.5 scaled
        ("ok", Value::Bool(true)),
    ]);
    let got = serialize_payload(&value, PayloadFormat::Form)
        .await
        .unwrap();
    let s = String::from_utf8(got.bytes).unwrap();
    assert!(s.contains("q=a+b%26c%3Dd") || s.contains("q=a%20b%26c%3Dd"));
    assert!(s.contains("n=1.5"));
    assert!(s.contains("ok=true"));
    assert_eq!(got.content_type, "application/x-www-form-urlencoded");

    let nested = record(vec![(
        "bad",
        Value::List(Arc::new(vec![Value::Decimal(1)])),
    )]);
    let err = serialize_payload(&nested, PayloadFormat::Form)
        .await
        .unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

#[tokio::test]
async fn form_rejects_non_record() {
    let err = serialize_payload(&Value::from("x".to_string()), PayloadFormat::Form)
        .await
        .unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

#[tokio::test]
async fn yaml_round_trips_through_json_intermediate() {
    let value = record(vec![
        ("name", Value::from("mizu".to_string())),
        ("count", Value::Decimal(300_000_000)),
    ]);
    let got = serialize_payload(&value, PayloadFormat::Yaml)
        .await
        .unwrap();
    let text = String::from_utf8(got.bytes).unwrap();
    let parsed: serde_json::Value = serde_yaml_bw::from_str(&text).unwrap();
    let expected = crate::core::types::to_json(&value).unwrap();
    assert_eq!(parsed, expected);
    assert_eq!(got.content_type, "application/yaml");
}

#[tokio::test]
async fn multipart_content_type_carries_a_boundary() {
    let value = record(vec![("field", Value::from("x".to_string()))]);
    let got = serialize_payload(&value, PayloadFormat::Multipart)
        .await
        .unwrap();
    assert!(
        got.content_type
            .starts_with("multipart/form-data; boundary=")
    );
}
