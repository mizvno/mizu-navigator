//! Tests for the hand-rolled YAML emitter.
//!
//! Round-trips every case through `serde_yaml_bw::from_str` — a real,
//! independent YAML parser — as a dev-only correctness oracle. That crate
//! is intentionally kept out of `[dependencies]` (see `Cargo.toml`): this
//! is the one place it is still allowed to appear, as a test-only checker
//! for the emitter that replaced it in production.

use super::to_yaml_string;
use serde_json::json;

fn round_trips(value: serde_json::Value) {
    let text = to_yaml_string(&value);
    let parsed: serde_json::Value = serde_yaml_bw::from_str(&text)
        .unwrap_or_else(|e| panic!("emitted YAML failed to parse back: {e}\n---\n{text}"));
    assert_eq!(parsed, value, "round-trip mismatch\n---\n{text}");
}

#[test]
fn scalars_round_trip() {
    round_trips(json!(null));
    round_trips(json!(true));
    round_trips(json!(false));
    round_trips(json!(0));
    round_trips(json!(-42));
    round_trips(json!(300_000_000));
    round_trips(json!(3.5));
    round_trips(json!(""));
    round_trips(json!("mizu"));
}

#[test]
fn empty_collections_round_trip() {
    round_trips(json!([]));
    round_trips(json!({}));
}

#[test]
fn flat_object_and_array_round_trip() {
    round_trips(json!({"name": "mizu", "count": 300_000_000}));
    round_trips(json!(["a", "b", "c"]));
}

#[test]
fn nested_object_round_trips() {
    round_trips(json!({
        "user": {"name": "a", "tags": ["x", "y"]},
        "active": true,
    }));
}

#[test]
fn nested_array_round_trips() {
    round_trips(json!([[1, 2], [3, 4]]));
    round_trips(json!([{"a": 1}, {"b": 2}]));
}

/// The "Norway problem" this emitter's always-quote policy exists to avoid:
/// a plain (unquoted) YAML `no`/`off`/`null`/`2024-01-01`/`1.0` is parsed
/// back as something other than a string by YAML 1.1 parsers. Every one of
/// these strings must still round-trip as a *string*.
#[test]
fn ambiguous_plain_scalar_strings_stay_strings() {
    for s in [
        "no", "yes", "off", "on", "null", "~", "true", "false", "2024-01-01", "1.0", "1_000",
        "0x1A",
    ] {
        round_trips(json!(s));
        round_trips(json!({ s: s }));
    }
}

/// Strings containing YAML-significant characters (quotes, backslashes,
/// colons, newlines, leading/trailing whitespace) must still round-trip
/// exactly.
#[test]
fn special_characters_round_trip() {
    for s in [
        "has: a colon",
        "has \"double quotes\"",
        "has 'single quotes'",
        "has\\a backslash",
        "has\na newline",
        "has\ta tab",
        "  leading and trailing  ",
        "- looks like a sequence item",
        "# looks like a comment",
        "multi\nline\nstring",
    ] {
        round_trips(json!(s));
    }
}

/// A control character below the explicitly-escaped set (`\n`/`\t`/`\r`)
/// must still produce valid, round-trippable YAML via the `\xHH` fallback.
#[test]
fn other_control_characters_round_trip() {
    round_trips(json!("bell\u{0007}here"));
    round_trips(json!("null-byte\u{0000}here"));
}

/// Non-ASCII content must be written literally (no escaping needed inside
/// a double-quoted YAML scalar) and round-trip unchanged.
#[test]
fn unicode_round_trips() {
    round_trips(json!("héllo wörld"));
    round_trips(json!("日本語"));
    round_trips(json!("emoji: 🎉"));
}

/// A bare top-level scalar (no wrapping object/array) is a valid, minimal
/// YAML document — must still round-trip.
#[test]
fn bare_top_level_scalar_round_trips() {
    round_trips(json!("just a string"));
    round_trips(json!(42));
}
