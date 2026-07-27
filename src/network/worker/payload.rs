//! Per-[`PayloadFormat`] request body serialisation and `Content-Type`
//! selection for outbound `NetworkCmd::Fetch` requests.

use crate::core::errors::MizuError;
use crate::core::types::Value;
use crate::parser::logic::PayloadFormat;

/// Returns the `Content-Type` header value for `format`.
///
/// `form` intentionally carries no `charset` parameter — that matches what a
/// typical HTTP server expects for `application/x-www-form-urlencoded`,
/// unlike `text/plain` where an explicit `charset=utf-8` is conventional.
/// `application/yaml` is the IANA-registered media type from RFC 9512, not
/// the older unofficial `text/yaml` / `application/x-yaml`.
pub(super) fn content_type_for(format: PayloadFormat) -> &'static str {
    match format {
        PayloadFormat::Json => "application/json",
        PayloadFormat::Form => "application/x-www-form-urlencoded",
        PayloadFormat::Text => "text/plain; charset=utf-8",
        PayloadFormat::Yaml => "application/yaml",
    }
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
///   (including [`Value::Int`]) is rejected rather than implicitly
///   stringified.
/// * `Form` — the payload must be a flat [`Value::Record`] of scalar
///   (`Bool`/`Int`/`String`/`Null`) fields, percent-encoded via the `url`
///   crate's `form_urlencoded` module.
pub(super) fn serialize_payload(value: &Value, format: PayloadFormat) -> Result<Vec<u8>, MizuError> {
    match format {
        PayloadFormat::Json => serde_json::to_vec(&crate::core::types::to_json(value))
            .map_err(|e| MizuError::Network(format!("request payload serialisation failed: {e}"))),
        PayloadFormat::Yaml => {
            let json_val = crate::core::types::to_json(value);
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
    }
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
    for (key, field_value) in fields.iter() {
        let encoded_value = match field_value {
            Value::Bool(_) | Value::Int(_) | Value::String(_) => field_value.to_string(),
            Value::Null => String::new(),
            Value::List(_) | Value::Record(_) => {
                return Err(MizuError::ExecutionError(format!(
                    "`as form` payload field `{key}` must be a scalar \
                     (bool/int/string/null), not a nested list/record"
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
mod tests {
    use super::*;
    use std::sync::Arc;

    fn record(fields: Vec<(&str, Value)>) -> Value {
        let mut v: Vec<(Arc<str>, Value)> =
            fields.into_iter().map(|(k, val)| (Arc::from(k), val)).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        Value::Record(Arc::from(v))
    }

    #[test]
    fn json_default_matches_manual_serialisation() {
        let value = Value::from("hello".to_string());
        let expected = serde_json::to_vec(&crate::core::types::to_json(&value)).unwrap();
        let got = serialize_payload(&value, PayloadFormat::Json).unwrap();
        assert_eq!(got, expected);
        assert_eq!(content_type_for(PayloadFormat::Json), "application/json");
    }

    #[test]
    fn text_requires_string() {
        assert!(serialize_payload(&Value::from("ok".to_string()), PayloadFormat::Text).is_ok());
        let err = serialize_payload(&Value::Int(100_000_000), PayloadFormat::Text).unwrap_err();
        assert!(matches!(err, MizuError::ExecutionError(_)));
    }

    #[test]
    fn form_encodes_scalars_and_rejects_nested() {
        let value = record(vec![
            ("q", Value::from("a b&c=d".to_string())),
            ("n", Value::Int(150_000_000)), // 1.5 scaled
            ("ok", Value::Bool(true)),
        ]);
        let bytes = serialize_payload(&value, PayloadFormat::Form).unwrap();
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("q=a+b%26c%3Dd") || s.contains("q=a%20b%26c%3Dd"));
        assert!(s.contains("n=1.5"));
        assert!(s.contains("ok=true"));

        let nested = record(vec![("bad", Value::List(Arc::new(vec![Value::Int(1)])))]);
        let err = serialize_payload(&nested, PayloadFormat::Form).unwrap_err();
        assert!(matches!(err, MizuError::ExecutionError(_)));
    }

    #[test]
    fn form_rejects_non_record() {
        let err = serialize_payload(&Value::from("x".to_string()), PayloadFormat::Form).unwrap_err();
        assert!(matches!(err, MizuError::ExecutionError(_)));
    }

    #[test]
    fn yaml_round_trips_through_json_intermediate() {
        let value = record(vec![
            ("name", Value::from("mizu".to_string())),
            ("count", Value::Int(300_000_000)),
        ]);
        let bytes = serialize_payload(&value, PayloadFormat::Yaml).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let parsed: serde_json::Value = serde_yaml_bw::from_str(&text).unwrap();
        let expected = crate::core::types::to_json(&value);
        assert_eq!(parsed, expected);
        assert_eq!(content_type_for(PayloadFormat::Yaml), "application/yaml");
    }
}
