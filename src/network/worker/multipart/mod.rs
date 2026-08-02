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
            Value::Bool(_) | Value::Int(_) | Value::Decimal(_) | Value::String(_) | Value::Null => {
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
mod tests;
