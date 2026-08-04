//! Minimal, dependency-free YAML emitter for outgoing `as yaml` request
//! payloads (see [`super::serialize_payload`]).
//!
//! Written to replace `serde_yaml_bw`, whose emitter (not just its parser)
//! is backed by `unsafe-libyaml-norway`, a direct unsafe port of the C
//! `libyaml` library — see `docs/design/unsafe_dependency_audit.md` §1b.
//! Scope is deliberately narrow: this only ever serializes the
//! `serde_json::Value` produced by `crate::core::types::to_json`, not
//! arbitrary YAML (no anchors, aliases, tags, or multi-document streams).
//!
//! Every scalar (string and map key) is emitted double-quoted. This is a
//! deliberate simplification, not an oversight: YAML's *unquoted* plain
//! scalars have a well-known ambiguity class informally called "the Norway
//! problem" — an author's plain `no` is parsed back as the boolean `false`
//! (YAML 1.1), a plain `2024-01-01` as a timestamp, `1.0.0` as garbled
//! multi-dot content, etc. Always quoting sidesteps that whole bug class:
//! a double-quoted YAML scalar is unambiguously a string to any conformant
//! parser, regardless of what its content looks like.

#![forbid(unsafe_code)]

use serde_json::Value;

/// Serializes `value` to a YAML document (block style for non-empty
/// collections, flow style `[]`/`{}` for empty ones).
pub(super) fn to_yaml_string(value: &Value) -> String {
    let mut out = String::new();
    write_block(value, 0, &mut out);
    out
}

fn write_block(value: &Value, indent: usize, out: &mut String) {
    match value {
        Value::Array(items) if !items.is_empty() => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                    push_indent(indent, out);
                }
                out.push_str("- ");
                write_block(item, indent + 1, out);
            }
        }
        Value::Object(map) if !map.is_empty() => {
            for (i, (key, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                    push_indent(indent, out);
                }
                write_quoted_string(key, out);
                out.push(':');
                match val {
                    Value::Array(a) if !a.is_empty() => {
                        out.push('\n');
                        push_indent(indent + 1, out);
                        write_block(val, indent + 1, out);
                    }
                    Value::Object(o) if !o.is_empty() => {
                        out.push('\n');
                        push_indent(indent + 1, out);
                        write_block(val, indent + 1, out);
                    }
                    _ => {
                        out.push(' ');
                        write_block(val, indent + 1, out);
                    }
                }
            }
        }
        _ => write_scalar(value, out),
    }
}

/// Writes a scalar, or the flow-style empty form `[]`/`{}` for an empty
/// array/object (an empty collection has no block-style representation).
fn write_scalar(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        // `serde_json::Number`'s `Display` is already a bare, unambiguous
        // numeric literal — valid as a plain YAML scalar with no quoting.
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_quoted_string(s, out),
        Value::Array(_) => out.push_str("[]"),
        Value::Object(_) => out.push_str("{}"),
    }
}

fn push_indent(level: usize, out: &mut String) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

/// Writes `s` as a YAML double-quoted scalar. Backslash, double-quote, and
/// the common control characters get their standard short escape; any other
/// C0 control character gets YAML's `\xHH` hex escape. Everything else
/// (including the full non-control Unicode range) is written literally —
/// YAML double-quoted scalars, unlike plain scalars, need no further
/// escaping to be unambiguous.
fn write_quoted_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests;
