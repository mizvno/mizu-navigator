//! Text interpolation: `Evaluator::interpolate_into[_with_overlay]`, the
//! byte-capped `CappedBuffer` writer they format into, and the
//! `interpolation_overflow` error helper.

use std::collections::HashMap;

use crate::core::errors::MizuError;

use super::super::interner::FrozenInterner;
use super::super::value::Value;
use super::compare::resolve_dot_path;
use super::types::{Evaluator, MAX_INTERPOLATED_BYTES};

impl Evaluator {
    /// Renders raw text formatting interpolations directly into a pre-allocated buffer.
    #[inline]
    pub fn interpolate_into(
        &self,
        raw_text: &str,
        interner: &FrozenInterner,
        buffer: &mut String,
    ) -> Result<(), MizuError> {
        self.interpolate_into_with_overlay(raw_text, interner, None, buffer)
    }

    /// Core interpolation engine. Uses lean, manual string slicing with `str::find`
    /// for fast bulk memory scanning.
    pub(crate) fn interpolate_into_with_overlay(
        &self,
        raw_text: &str,
        interner: &FrozenInterner,
        overlay: Option<&HashMap<String, Value>>,
        buffer: &mut String,
    ) -> Result<(), MizuError> {
        use std::fmt::Write;
        // Every write below goes through this cap, so no single value — and
        // no accumulation of values — can hand the renderer an unbounded run.
        // See `MAX_INTERPOLATED_BYTES`. The overflow is rejected at the point
        // of the offending write, before the bytes are copied, so the peak
        // allocation stays bounded even when the source value is enormous.
        let buffer = &mut CappedBuffer {
            buf: buffer,
            cap: MAX_INTERPOLATED_BYTES,
        };
        let mut rest = raw_text;

        while let Some(idx) = rest.find(|c| c == '{' || c == '\\') {
            buffer.push_str(&rest[..idx])?;

            let c = rest.as_bytes()[idx];
            if c == b'\\' {
                rest = &rest[idx + 1..];
                if let Some(next_char) = rest.chars().next() {
                    if next_char == '\\' || next_char == '{' || next_char == '}' {
                        buffer.push(next_char)?;
                        rest = &rest[next_char.len_utf8()..];
                    } else {
                        buffer.push('\\')?;
                    }
                } else {
                    buffer.push('\\')?;
                }
            } else if c == b'{' {
                rest = &rest[idx + 1..];
                // Check for double curly brace `{{` which could mean escaped brace, but
                // Mizu format actually uses `\{` for escaping. For now we just look for `}`.
                if let Some(end_idx) = rest.find('}') {
                    let var_name = &rest[..end_idx];

                    if var_name.contains('.') {
                        const MAX_RECORD_DEPTH: usize = 64;
                        let mut parts = var_name.splitn(MAX_RECORD_DEPTH, '.');
                        let root = parts.next().unwrap_or("");

                        let handled = match overlay.and_then(|map| map.get(root)) {
                            Some(root_val) => match resolve_dot_path(root_val, parts.clone()) {
                                Some(leaf) => {
                                    write!(buffer, "{}", leaf).map_err(interpolation_overflow)?;
                                    true
                                }
                                None => false,
                            },
                            None => false,
                        };

                        if !handled {
                            match self.get_value_by_name(root, interner) {
                                None => {
                                    write!(buffer, "{{{}}}", var_name)
                                        .map_err(interpolation_overflow)?;
                                }
                                Some(root_val) => match resolve_dot_path(root_val, parts) {
                                    Some(leaf) => {
                                        write!(buffer, "{}", leaf)
                                            .map_err(interpolation_overflow)?;
                                    }
                                    None => {
                                        tracing::warn!(
                                            "interpolation: path `{}` could not be resolved",
                                            var_name
                                        );
                                        write!(buffer, "{{{}}}", var_name)
                                            .map_err(interpolation_overflow)?;
                                    }
                                },
                            }
                        }
                    } else {
                        match overlay.and_then(|map| map.get(var_name)) {
                            Some(val) => {
                                write!(buffer, "{}", val).map_err(interpolation_overflow)?;
                            }
                            None => {
                                if let Some(val) = self.get_value_by_name(var_name, interner) {
                                    write!(buffer, "{}", val).map_err(interpolation_overflow)?;
                                } else {
                                    write!(buffer, "{{{}}}", var_name)
                                        .map_err(interpolation_overflow)?;
                                    tracing::warn!("Variable binding missing: {}", var_name);
                                }
                            }
                        }
                    }
                    rest = &rest[end_idx + 1..];
                } else {
                    buffer.push('{')?;
                }
            }
        }
        buffer.push_str(rest)?;
        Ok(())
    }
}

/// The error every over-budget interpolation write turns into.
///
/// A free function rather than an inline closure so all six write sites report
/// the limit identically, and so the `std::fmt::Error` — which carries no
/// information of its own — is never surfaced to a caller as if it were an
/// ordinary formatting failure.
fn interpolation_overflow(_: std::fmt::Error) -> MizuError {
    MizuError::SecurityViolation(format!(
        "interpolated text exceeds the {MAX_INTERPOLATED_BYTES}-byte render budget"
    ))
}

/// A `&mut String` that refuses writes past `cap` instead of growing.
///
/// The refusal happens *before* the bytes are copied, which is the point: the
/// values being formatted here are attacker-sized (a network response is
/// bounded only by the 32 MiB transfer cap), and a check applied after the
/// write would still pay the full copy — once per text node, on every layout
/// pass. Implementing [`std::fmt::Write`] rather than special-casing
/// [`Value`]'s shape means the bound holds for every `Display` impl, including
/// `List`/`Record` payloads that reach the buffer through many small writes.
struct CappedBuffer<'a> {
    buf: &'a mut String,
    cap: usize,
}

impl CappedBuffer<'_> {
    /// Remaining capacity, saturating rather than wrapping if the buffer was
    /// handed in already over budget.
    #[inline]
    fn remaining(&self) -> usize {
        self.cap.saturating_sub(self.buf.len())
    }

    fn push_str(&mut self, s: &str) -> Result<(), MizuError> {
        if s.len() > self.remaining() {
            return Err(interpolation_overflow(std::fmt::Error));
        }
        self.buf.push_str(s);
        Ok(())
    }

    fn push(&mut self, c: char) -> Result<(), MizuError> {
        if c.len_utf8() > self.remaining() {
            return Err(interpolation_overflow(std::fmt::Error));
        }
        self.buf.push(c);
        Ok(())
    }
}

impl std::fmt::Write for CappedBuffer<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if s.len() > self.remaining() {
            return Err(std::fmt::Error);
        }
        self.buf.push_str(s);
        Ok(())
    }
}
