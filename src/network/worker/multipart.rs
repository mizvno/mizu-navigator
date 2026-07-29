//! `multipart/form-data` encoding for `as multipart` `NetworkCall` payloads.
//!
//! Reads any [`crate::core::types::Value::FileHandle`] field's bytes here —
//! and only here — in bounded chunks, off the UI/evaluator thread, at the
//! moment the request is actually sent. No other code path in this project
//! reads a selected file's contents at all (see `SECURITY-INVARIANTS.md`'s
//! S1 entry, extended to file selections by this module).

use std::path::Path;

use crate::core::errors::MizuError;
use crate::core::types::Value;

/// Hard ceiling on the total outbound multipart request body, enforced
/// *while reading* (not by trusting a single upfront `stat()`) so a file
/// that grows between the size check and the full read cannot bypass the
/// budget — mirrors `fetch.rs`'s `check_response_body_budget`, applied
/// outbound instead of inbound.
///
/// Deliberately a distinct, explicitly-named constant rather than reusing
/// `MAX_RESPONSE_BODY_BYTES`'s value: inbound and outbound limits are
/// independent design decisions that happen to currently agree in
/// magnitude. 32 MiB comfortably covers common attachment sizes (images,
/// PDFs, small archives, documents) without holding an unbounded amount of
/// memory for a single upload.
pub(super) const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Checks that appending `incoming_len` bytes to a body of `current_len`
/// bytes stays within [`MAX_REQUEST_BODY_BYTES`].
fn check_request_body_budget(current_len: usize, incoming_len: usize) -> Result<(), MizuError> {
    if current_len.saturating_add(incoming_len) > MAX_REQUEST_BODY_BYTES {
        return Err(MizuError::SecurityViolation(format!(
            "request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte limit; upload aborted"
        )));
    }
    Ok(())
}

/// Appends `bytes` to `body`, charging them against `total` first so a
/// budget violation is caught before the (potentially huge) append happens.
fn push_checked(body: &mut Vec<u8>, total: &mut usize, bytes: &[u8]) -> Result<(), MizuError> {
    check_request_body_budget(*total, bytes.len())?;
    *total += bytes.len();
    body.extend_from_slice(bytes);
    Ok(())
}

/// Generates a random multipart boundary token: 24 CSPRNG-drawn bytes,
/// hex-encoded (never derived from document content or anything tainted).
///
/// Mirrors `core::storage`'s existing `ChaCha8Rng::from_rng(OsRng)` pattern
/// for a fast per-call CSPRNG seeded from the OS.
pub(super) fn generate_boundary() -> Result<String, MizuError> {
    use aes_gcm::aead::OsRng;
    use rand_core::{RngCore, SeedableRng};
    let mut rng = rand_chacha::ChaCha8Rng::from_rng(OsRng)
        .map_err(|e| MizuError::ExecutionError(format!("boundary RNG seed failed: {e}")))?;
    let mut raw = [0u8; 24];
    rng.fill_bytes(&mut raw);
    Ok(format!("mizu{}", hex::encode(raw)))
}

/// Validates `s` for safe use as an HTTP quoted-string token (a
/// `Content-Disposition` `name=`/`filename=` value): rejects (does not
/// silently strip) any control character or unescaped `"` — the same
/// "never let a value go straight into protocol structure" discipline
/// Phase 1's custom-header-value validation already applies, applied here
/// to filenames instead of header values.
fn sanitize_disposition_token(s: &str, what: &str) -> Result<String, MizuError> {
    if s.chars().any(|c| c.is_control()) {
        return Err(MizuError::ExecutionError(format!(
            "multipart {what} `{s}` contains a control character and cannot be \
             used in a Content-Disposition header"
        )));
    }
    if s.contains('"') {
        return Err(MizuError::ExecutionError(format!(
            "multipart {what} `{s}` contains an unescaped `\"` and cannot be \
             used in a Content-Disposition header"
        )));
    }
    Ok(s.to_string())
}

/// Extension → MIME type table for multipart file parts.
///
/// Deliberately a static, explicit table — never file-magic-byte sniffing.
/// Content-sniffing for MIME is the exact mechanism behind a well-known
/// historical browser vulnerability class (MIME-confusion XSS; the reason
/// `X-Content-Type-Options: nosniff` exists industry-wide), and it
/// contradicts this project's existing reject-don't-guess posture (see
/// `MizuUri::parse`'s control-character handling). An extension absent from
/// this table falls back to `application/octet-stream`, never a guess.
const MIME_TABLE: &[(&str, &str)] = &[
    ("txt", "text/plain"),
    ("csv", "text/csv"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("css", "text/css"),
    ("json", "application/json"),
    ("xml", "application/xml"),
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("tar", "application/x-tar"),
    ("7z", "application/x-7z-compressed"),
    ("rar", "application/vnd.rar"),
    ("doc", "application/msword"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    ("xls", "application/vnd.ms-excel"),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("ppt", "application/vnd.ms-powerpoint"),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("svg", "image/svg+xml"),
    ("ico", "image/x-icon"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
    ("avi", "video/x-msvideo"),
];

/// The explicit fallback for any extension absent from [`MIME_TABLE`], or a
/// path with no extension at all.
const MIME_FALLBACK: &str = "application/octet-stream";

/// Looks up `path`'s MIME type by its extension (case-insensitive), never by
/// inspecting file contents. See [`MIME_TABLE`]'s doc comment.
pub(super) fn mime_for_path(path: &Path) -> &'static str {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return MIME_FALLBACK;
    };
    let ext_lower = ext.to_ascii_lowercase();
    MIME_TABLE
        .iter()
        .find(|(table_ext, _)| *table_ext == ext_lower)
        .map(|(_, mime)| *mime)
        .unwrap_or(MIME_FALLBACK)
}

/// Reads `path` in bounded chunks directly into `body`, charging every chunk
/// against `total` via [`push_checked`] as it's read — a file that grows
/// between any prior size check and this read cannot bypass the budget,
/// and a file exceeding it aborts mid-transfer (the partial `body` is
/// discarded by the caller, since this returns `Err`).
async fn read_file_bounded_into(
    path: &Path,
    body: &mut Vec<u8>,
    total: &mut usize,
) -> Result<(), MizuError> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(MizuError::IoError)?;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut chunk).await.map_err(MizuError::IoError)?;
        if n == 0 {
            break;
        }
        push_checked(body, total, &chunk[..n])?;
    }
    Ok(())
}

/// Encodes `value` (must be a [`Value::Record`]) as a `multipart/form-data`
/// body using `boundary`.
///
/// Field shape → part mapping:
/// * `Bool | Int | String | Null` → a text part
///   (`Content-Type: text/plain; charset=utf-8`).
/// * `List | Record` (nested) → a JSON part (`Content-Type: application/json`),
///   reusing the same `Value` → `serde_json::Value` conversion `json`/`yaml`
///   payloads already use.
/// * `FileHandle` → a file part, `filename=` from the handle's (sanitised)
///   display filename, `Content-Type` from [`mime_for_path`]. The only
///   place this project reads a selected file's bytes — see this module's
///   doc comment.
///
/// Any other shape (e.g. a `FileHandle` nested two levels deep, inside a
/// nested `List`/`Record` field) is a runtime error — no best-effort
/// encoding.
pub(super) async fn encode_multipart(value: &Value, boundary: &str) -> Result<Vec<u8>, MizuError> {
    let Value::Record(fields) = value else {
        return Err(MizuError::ExecutionError(
            "`as multipart` payload must be a record".to_string(),
        ));
    };

    let mut body: Vec<u8> = Vec::new();
    let mut total = 0usize;

    for field in fields.iter() {
        let key = &field.key;
        let field_value = &field.value;
        let name = sanitize_disposition_token(key, "field name")?;
        push_checked(
            &mut body,
            &mut total,
            format!("--{boundary}\r\n").as_bytes(),
        )?;

        match field_value {
            Value::Bool(_) | Value::Int(_) | Value::String(_) | Value::Null => {
                let text = match field_value {
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                push_checked(
                    &mut body,
                    &mut total,
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"\r\n\
                         Content-Type: text/plain; charset=utf-8\r\n\r\n"
                    )
                    .as_bytes(),
                )?;
                push_checked(&mut body, &mut total, text.as_bytes())?;
                push_checked(&mut body, &mut total, b"\r\n")?;
            }
            Value::List(_) | Value::Record(_) => {
                let json_val = crate::core::types::to_json(field_value)?;
                let json_bytes = serde_json::to_vec(&json_val).map_err(|e| {
                    MizuError::Network(format!("multipart JSON part serialisation failed: {e}"))
                })?;
                push_checked(
                    &mut body,
                    &mut total,
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"\r\n\
                         Content-Type: application/json\r\n\r\n"
                    )
                    .as_bytes(),
                )?;
                push_checked(&mut body, &mut total, &json_bytes)?;
                push_checked(&mut body, &mut total, b"\r\n")?;
            }
            Value::FileHandle(handle) => {
                let filename = sanitize_disposition_token(&handle.filename, "filename")?;
                let mime = mime_for_path(&handle.path);
                push_checked(
                    &mut body,
                    &mut total,
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n\
                         Content-Type: {mime}\r\n\r\n"
                    )
                    .as_bytes(),
                )?;
                read_file_bounded_into(&handle.path, &mut body, &mut total).await?;
                push_checked(&mut body, &mut total, b"\r\n")?;
            }
        }
    }

    push_checked(
        &mut body,
        &mut total,
        format!("--{boundary}--\r\n").as_bytes(),
    )?;
    Ok(body)
}

#[cfg(test)]
mod tests {
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
}
