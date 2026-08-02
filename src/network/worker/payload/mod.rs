//! Per-[`PayloadFormat`] request body serialisation and `Content-Type`
//! selection for outbound `NetworkCmd::Fetch` requests.

use crate::core::errors::MizuError;
use crate::core::types::Value;
use crate::parser::logic::PayloadFormat;

/// Returns the static `Content-Type` header value for the non-`Multipart`
/// formats.
///
/// `form` intentionally carries no `charset` parameter — that matches what a
/// typical HTTP server expects for `application/x-www-form-urlencoded`,
/// unlike `text/plain` where an explicit `charset=utf-8` is conventional.
/// `application/yaml` is the IANA-registered media type from RFC 9512, not
/// the older unofficial `text/yaml` / `application/x-yaml`.
///
/// `Multipart`'s real `Content-Type` includes a per-request random boundary
/// (RFC 2046) and can't be a fixed `&'static str` — [`serialize_payload`]
/// builds it directly instead of calling this function for that format; the
/// value returned here for `Multipart` is a placeholder, never sent on the
/// wire, that exists only so this match stays exhaustive.
fn content_type_for(format: PayloadFormat) -> &'static str {
    match format {
        PayloadFormat::Json => "application/json",
        PayloadFormat::Form => "application/x-www-form-urlencoded",
        PayloadFormat::Text => "text/plain; charset=utf-8",
        PayloadFormat::Yaml => "application/yaml",
        PayloadFormat::Multipart => "multipart/form-data",
    }
}

/// A serialised request body paired with the `Content-Type` it must be sent
/// with. Bundled together because `Multipart`'s `Content-Type` carries a
/// boundary generated during serialisation — the two can't be computed
/// independently for that format the way they can for the other four.
#[derive(Debug)]
pub(super) struct SerializedRequestBody {
    pub(super) bytes: Vec<u8>,
    pub(super) content_type: String,
}

/// Serialises `value` into a request body per `format`.
///
/// Returns a plain [`MizuError`] (never panics) on a shape violation, so the
/// caller can abort before any network I/O is attempted — no request is ever
/// sent for a payload that fails validation here.
///
/// * `Json` — byte-for-byte identical to the pre-`PayloadFormat` behaviour:
///   any [`Value`] shape, via the existing [`crate::core::types::to_json`]
///   conversion.
/// * `Yaml` — accepts the same shapes as `Json`; reuses the same
///   `Value` → `serde_json::Value` conversion and serialises that
///   intermediate representation as YAML instead of JSON.
/// * `Text` — the payload must be exactly [`Value::String`]; anything else
///   (including [`Value::Decimal`]) is rejected rather than implicitly
///   stringified.
/// * `Form` — the payload must be a flat [`Value::Record`] of scalar
///   (`Bool`/`Int`/`String`/`Null`) fields, percent-encoded via the `url`
///   crate's `form_urlencoded` module.
/// * `Multipart` — the payload must be a [`Value::Record`]; see
///   `super::multipart` for the per-field encoding rules. The only format
///   that reads a [`Value::FileHandle`]'s bytes (bounded, streamed, and
///   budget-checked while reading — see `multipart::MAX_REQUEST_BODY_BYTES`).
pub(super) async fn serialize_payload(
    value: &Value,
    format: PayloadFormat,
) -> Result<SerializedRequestBody, MizuError> {
    if format == PayloadFormat::Multipart {
        let boundary = super::multipart::generate_boundary()?;
        let bytes = super::multipart::encode_multipart(value, &boundary).await?;
        return Ok(SerializedRequestBody {
            bytes,
            content_type: format!("multipart/form-data; boundary={boundary}"),
        });
    }

    let bytes = match format {
        PayloadFormat::Json => {
            let json_val = crate::core::types::to_json(value)?;
            serde_json::to_vec(&json_val).map_err(|e| {
                MizuError::Network(format!("request payload serialisation failed: {e}"))
            })
        }
        PayloadFormat::Yaml => {
            let json_val = crate::core::types::to_json(value)?;
            serde_yaml_bw::to_string(&json_val)
                .map(String::into_bytes)
                .map_err(|e| {
                    MizuError::Network(format!("request payload YAML serialisation failed: {e}"))
                })
        }
        PayloadFormat::Text => match value {
            Value::String(s) => Ok(s.as_bytes().to_vec()),
            other => Err(MizuError::ExecutionError(format!(
                "`as text` payload must be a string, got {other:?}"
            ))),
        },
        PayloadFormat::Form => serialize_form(value),
        PayloadFormat::Multipart => unreachable!("handled above"),
    }?;

    Ok(SerializedRequestBody {
        bytes,
        content_type: content_type_for(format).to_string(),
    })
}

/// Encodes a flat record of scalar fields as
/// `application/x-www-form-urlencoded`.
fn serialize_form(value: &Value) -> Result<Vec<u8>, MizuError> {
    let Value::Record(fields) = value else {
        return Err(MizuError::ExecutionError(
            "`as form` payload must be a record".to_string(),
        ));
    };
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(fields.len());
    for field in fields.iter() {
        let key = &field.key;
        let field_value = &field.value;
        let encoded_value = match field_value {
            Value::Bool(_) | Value::Int(_) | Value::Decimal(_) | Value::String(_) => {
                field_value.to_string()
            }
            Value::Null => String::new(),
            Value::List(_) | Value::Record(_) => {
                return Err(MizuError::ExecutionError(format!(
                    "`as form` payload field `{key}` must be a scalar \
                     (bool/int/string/null), not a nested list/record"
                )));
            }
            Value::FileHandle(_) => {
                return Err(MizuError::ExecutionError(format!(
                    "`as form` payload field `{key}` cannot be a file selection; \
                     use `as multipart` to upload a file"
                )));
            }
        };
        pairs.push((key.to_string(), encoded_value));
    }
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .finish();
    Ok(encoded.into_bytes())
}

#[cfg(test)]
mod tests;
