//! Tests for the multipart module.

use super::*;
use crate::core::types::FileHandleData;
use std::sync::Arc;

fn record(fields: Vec<(&str, Value)>) -> Value {
    Value::record_from_unsorted(fields)
}

/// Returns the `TempDir` guard alongside the written file's path — the
/// caller must keep the guard alive for as long as the path is used, or
/// the directory (and file) is deleted on drop.
fn write_temp_file(filename: &str, contents: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(filename);
    std::fs::write(&path, contents).unwrap();
    (dir, path)
}

struct ParsedField {
    name: Option<String>,
    file_name: Option<String>,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// Parses `body` back with a real multipart parser (`multer`), fully
/// consuming each field's bytes before requesting the next — `multer`
/// requires a field to be drained (or dropped) before the next
/// `next_field()` call can acquire its internal lock.
async fn parse_back(body: &[u8], boundary: &str) -> Vec<ParsedField> {
    let owned = bytes::Bytes::copy_from_slice(body);
    let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(owned) });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut fields = Vec::new();
    while let Some(field) = multipart.next_field().await.unwrap() {
        let name = field.name().map(str::to_string);
        let file_name = field.file_name().map(str::to_string);
        let content_type = field.content_type().map(|m| m.essence_str().to_string());
        let bytes = field.bytes().await.unwrap().to_vec();
        fields.push(ParsedField {
            name,
            file_name,
            content_type,
            bytes,
        });
    }
    fields
}

#[test]
fn mime_for_path_falls_back_to_octet_stream_for_unknown_extension() {
    assert_eq!(mime_for_path(std::path::Path::new("a.png")), "image/png");
    assert_eq!(
        mime_for_path(std::path::Path::new("a.xyz-unknown")),
        MIME_FALLBACK
    );
    assert_eq!(
        mime_for_path(std::path::Path::new("no_extension")),
        MIME_FALLBACK
    );
    // Case-insensitivity.
    assert_eq!(mime_for_path(std::path::Path::new("A.PNG")), "image/png");
}

#[test]
fn boundary_generation_is_not_predictable() {
    let a = generate_boundary().unwrap();
    let b = generate_boundary().unwrap();
    assert_ne!(
        a, b,
        "two consecutive requests must use different boundaries"
    );
    assert!(
        a.len() > 16,
        "boundary must have real entropy, not a short/fixed token"
    );
}

#[tokio::test]
async fn mixed_text_and_file_fields_produce_a_well_formed_body() {
    let (_tmp_dir, file_path) = write_temp_file("cat.png", b"fake-png-bytes");
    let value = record(vec![
        ("caption", Value::from("hello world".to_string())),
        (
            "avatar",
            Value::FileHandle(Arc::new(FileHandleData {
                path: file_path,
                filename: "cat.png".to_string(),
            })),
        ),
    ]);

    let boundary = generate_boundary().unwrap();
    let body = encode_multipart(&value, &boundary).await.unwrap();

    let fields = parse_back(&body, &boundary).await;
    assert_eq!(fields.len(), 2);

    assert!(
        fields.iter().any(|f| f.name.as_deref() == Some("avatar")),
        "expected a field named `avatar`"
    );

    for field in &fields {
        match field.name.as_deref() {
            Some("caption") => {
                assert_eq!(field.file_name, None);
                assert_eq!(field.bytes, b"hello world");
            }
            Some("avatar") => {
                assert_eq!(field.file_name.as_deref(), Some("cat.png"));
                assert_eq!(field.content_type.as_deref(), Some("image/png"));
                assert_eq!(field.bytes, b"fake-png-bytes");
            }
            other => panic!("unexpected field name: {other:?}"),
        }
    }
}

#[tokio::test]
async fn nested_list_and_record_fields_become_json_parts() {
    let value = record(vec![(
        "tags",
        Value::List(Arc::new(vec![
            Value::from("a".to_string()),
            Value::from("b".to_string()),
        ])),
    )]);
    let boundary = generate_boundary().unwrap();
    let body = encode_multipart(&value, &boundary).await.unwrap();
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("Content-Type: application/json"));
    assert!(text.contains(r#"["a","b"]"#));
}

#[tokio::test]
async fn filename_with_control_character_is_rejected() {
    let (_tmp_dir, file_path) = write_temp_file("evil.txt", b"data");
    let value = record(vec![(
        "f",
        Value::FileHandle(Arc::new(FileHandleData {
            path: file_path,
            filename: "evil\r\nX-Injected: yes.txt".to_string(),
        })),
    )]);
    let boundary = generate_boundary().unwrap();
    let err = encode_multipart(&value, &boundary).await.unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

#[tokio::test]
async fn filename_with_unescaped_quote_is_rejected() {
    let (_tmp_dir, file_path) = write_temp_file("evil.txt", b"data");
    let value = record(vec![(
        "f",
        Value::FileHandle(Arc::new(FileHandleData {
            path: file_path,
            filename: r#"weird"name.txt"#.to_string(),
        })),
    )]);
    let boundary = generate_boundary().unwrap();
    let err = encode_multipart(&value, &boundary).await.unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

#[tokio::test]
async fn file_exceeding_budget_aborts_mid_transfer() {
    let oversized = vec![0u8; MAX_REQUEST_BODY_BYTES + 1024];
    let (_tmp_dir, file_path) = write_temp_file("huge.bin", &oversized);
    let value = record(vec![(
        "f",
        Value::FileHandle(Arc::new(FileHandleData {
            path: file_path,
            filename: "huge.bin".to_string(),
        })),
    )]);
    let boundary = generate_boundary().unwrap();
    let err = encode_multipart(&value, &boundary).await.unwrap_err();
    assert!(
        matches!(err, MizuError::SecurityViolation(_)),
        "an oversized file must abort with SecurityViolation, not succeed"
    );
}

#[tokio::test]
async fn non_record_payload_is_rejected() {
    let err = encode_multipart(&Value::from("x".to_string()), "b")
        .await
        .unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}
