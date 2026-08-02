//! Token-level value parsers: dimensions, colors, backgrounds, font
//! family/weight, and the `justify`/`align` keyword sets.

use crate::core::errors::MizuError;

use super::types::*;
use taffy::style::{AlignItems, JustifyContent};

/// Returns the number of leading space characters in `line`.
///
/// Only space (`U+0020`) is counted.  Tabs are deliberately excluded since the
/// Mizu spec mandates space-based indentation.
#[inline]
pub(super) fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Parses a [`MizuDimension`] from a token that is either a plain `f32`
/// (pixels) or an `f32` followed immediately by `%` (percent).
pub(super) fn parse_dimension(
    token: &str,
    prop: &str,
    line_num: usize,
) -> Result<MizuDimension, MizuError> {
    // Order matters only in that each suffix must be tried before falling
    // through to the bare-pixel case; the four viewport suffixes are
    // mutually exclusive (none is a suffix of another) so their relative
    // order doesn't matter.
    let unit_error = |unit: &str, token: &str| {
        MizuError::ParseError(format!(
            "line {line_num}: invalid `{unit}` value `{token}` for `{prop}`; \
             expected a number followed by `{unit}`, e.g. `50{unit}`"
        ))
    };
    if let Some(v) = token.strip_suffix('%') {
        v.parse::<f32>().map(MizuDimension::Percent).map_err(|_| {
            MizuError::ParseError(format!(
                "line {line_num}: invalid percentage `{token}` for `{prop}`; \
                 expected a number followed by `%`, e.g. `50%`"
            ))
        })
    } else if let Some(v) = token.strip_suffix("vmin") {
        v.parse::<f32>()
            .map(MizuDimension::ViewportMin)
            .map_err(|_| unit_error("vmin", token))
    } else if let Some(v) = token.strip_suffix("vmax") {
        v.parse::<f32>()
            .map(MizuDimension::ViewportMax)
            .map_err(|_| unit_error("vmax", token))
    } else if let Some(v) = token.strip_suffix("vw") {
        v.parse::<f32>()
            .map(MizuDimension::ViewportWidth)
            .map_err(|_| unit_error("vw", token))
    } else if let Some(v) = token.strip_suffix("vh") {
        v.parse::<f32>()
            .map(MizuDimension::ViewportHeight)
            .map_err(|_| unit_error("vh", token))
    } else {
        token
            .parse::<f32>()
            .map(MizuDimension::Pixels)
            .map_err(|_| {
                MizuError::ParseError(format!(
                    "line {line_num}: invalid number `{token}` for `{prop}`; \
                 expected a numeric pixel value, e.g. `20`"
                ))
            })
    }
}

/// Parses a plain `f32` value for scalar properties (`font-size`,
/// `border-radius`).
pub(super) fn parse_f32(token: &str, prop: &str, line_num: usize) -> Result<f32, MizuError> {
    token.parse::<f32>().map_err(|_| {
        MizuError::ParseError(format!(
            "line {line_num}: invalid number `{token}` for `{prop}`; \
             expected a numeric value, e.g. `14`"
        ))
    })
}

/// Parses a Mizu hex colour literal into a [`MizuColor`].
///
/// ## Accepted formats
///
/// | Format       | Example        | Meaning                            |
/// |--------------|----------------|------------------------------------|
/// | `#rgb`       | `#fff`         | 3-digit short form, fully opaque   |
/// | `#rrggbb`    | `#ff0000`      | 6-digit standard form              |
/// | `#rrggbbaa`  | `#00000080`    | 8-digit with alpha channel         |
///
/// ## Validation
///
/// * The token must start with `#`.
/// * All remaining characters must be ASCII hex digits (`0-9`, `a-f`, `A-F`).
/// * The hex body after `#` must be exactly 3, 6, or 8 characters long.
pub(super) fn parse_color(token: &str, line_num: usize) -> Result<MizuColor, MizuError> {
    if token.starts_with("rgba(") && token.ends_with(")") {
        let inner = &token[5..token.len() - 1];
        let mut parts = inner.split(',').map(|s| s.trim());
        let r = parts
            .next()
            .and_then(|s| s.parse::<u8>().ok())
            .ok_or_else(|| {
                MizuError::ParseError(format!("line {line_num}: invalid rgba format"))
            })?;
        let g = parts
            .next()
            .and_then(|s| s.parse::<u8>().ok())
            .ok_or_else(|| {
                MizuError::ParseError(format!("line {line_num}: invalid rgba format"))
            })?;
        let b = parts
            .next()
            .and_then(|s| s.parse::<u8>().ok())
            .ok_or_else(|| {
                MizuError::ParseError(format!("line {line_num}: invalid rgba format"))
            })?;
        let a_f = parts
            .next()
            .and_then(|s| s.parse::<f32>().ok())
            .ok_or_else(|| {
                MizuError::ParseError(format!("line {line_num}: invalid rgba format"))
            })?;
        return Ok(MizuColor::rgba(
            r,
            g,
            b,
            (a_f * 255.0).clamp(0.0, 255.0) as u8,
        ));
    }

    let hex = token.strip_prefix('#').ok_or_else(|| {
        MizuError::ParseError(format!(
            "line {line_num}: colour value must start with `#` or `rgba(`, got `{token}`"
        ))
    })?;

    // Validate all characters are hex digits before slicing.
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(MizuError::ParseError(format!(
            "line {line_num}: invalid hex colour `{token}`: \
             contains non-hexadecimal characters"
        )));
    }

    match hex.len() {
        // #rgb → expand each nibble to a byte (e.g. #f0a → #ff00aa).
        3 => {
            let r = expand_nibble(hex.as_bytes()[0]);
            let g = expand_nibble(hex.as_bytes()[1]);
            let b = expand_nibble(hex.as_bytes()[2]);
            Ok(MizuColor::rgb(r, g, b))
        }
        // #rrggbb — standard 6-digit form.
        6 => {
            let r = parse_hex_byte(&hex[0..2], token, line_num)?;
            let g = parse_hex_byte(&hex[2..4], token, line_num)?;
            let b = parse_hex_byte(&hex[4..6], token, line_num)?;
            Ok(MizuColor::rgb(r, g, b))
        }
        // #rrggbbaa — 8-digit form with alpha.
        8 => {
            let r = parse_hex_byte(&hex[0..2], token, line_num)?;
            let g = parse_hex_byte(&hex[2..4], token, line_num)?;
            let b = parse_hex_byte(&hex[4..6], token, line_num)?;
            let a = parse_hex_byte(&hex[6..8], token, line_num)?;
            Ok(MizuColor::rgba(r, g, b, a))
        }
        _ => Err(MizuError::ParseError(format!(
            "line {line_num}: invalid hex colour `{token}`: \
             length must be 3 (#rgb), 6 (#rrggbb), or 8 (#rrggbbaa)"
        ))),
    }
}

pub(super) fn parse_background(token: &str, line_num: usize) -> Result<MizuBackground, MizuError> {
    if token.starts_with("linear-gradient(") && token.ends_with(")") {
        let inner = &token[16..token.len() - 1];
        let mut parts = inner.split(',');
        let angle_str = parts.next().ok_or_else(|| {
            MizuError::ParseError(format!("line {line_num}: linear-gradient missing angle"))
        })?;
        let angle = angle_str
            .trim()
            .strip_suffix("deg")
            .unwrap_or(angle_str.trim())
            .parse::<f32>()
            .map_err(|_| {
                MizuError::ParseError(format!("line {line_num}: linear-gradient invalid angle"))
            })?;
        let start_str = parts.next().ok_or_else(|| {
            MizuError::ParseError(format!(
                "line {line_num}: linear-gradient missing start color"
            ))
        })?;
        let end_str = parts.next().ok_or_else(|| {
            MizuError::ParseError(format!(
                "line {line_num}: linear-gradient missing end color"
            ))
        })?;
        let start = parse_color(start_str.trim(), line_num)?;
        let end = parse_color(end_str.trim(), line_num)?;
        return Ok(MizuBackground::LinearGradient { angle, start, end });
    }

    Ok(MizuBackground::Solid(parse_color(token, line_num)?))
}

/// Expands a single hex nibble byte (ASCII) to a full byte by repeating it.
/// e.g. `'f'` (0x66) → 0xFF.
#[inline]
pub(super) fn expand_nibble(nibble: u8) -> u8 {
    // Build a two-character string and parse it; since we've already validated
    // that the character is a hex digit, this cannot fail.
    let repeated = [nibble, nibble];
    // SAFETY: `repeated` contains only ASCII bytes so it is valid UTF-8.
    // We use from_utf8_unchecked — but wait, we can't use unsafe.
    // Instead: nibble is ASCII, so this slice is valid UTF-8.
    let s = std::str::from_utf8(&repeated).unwrap_or("00"); // infallible: both bytes are ASCII hex digits
    u8::from_str_radix(s, 16).unwrap_or(0) // infallible: valid 2-digit hex
}

/// Parses a two-character hex string slice into a `u8`.
///
/// This function is only called after the full hex string has been validated to
/// contain only hex digits, so the `from_str_radix` call is infallible in
/// practice.  We still propagate a `ParseError` as a safety net to satisfy the
/// zero-`unwrap` policy.
pub(super) fn parse_hex_byte(s: &str, token: &str, line_num: usize) -> Result<u8, MizuError> {
    u8::from_str_radix(s, 16).map_err(|_| {
        MizuError::ParseError(format!(
            "line {line_num}: internal error parsing hex byte `{s}` in `{token}`"
        ))
    })
}

/// Parses `font-family` against the fixed three-generic allowlist
/// (`sans-serif`, `serif`, `monospace`). Accepts the value quoted or bare —
/// either way, only those three exact tokens are ever accepted. See
/// [`MizuFontFamily`] for the security rationale.
pub(super) fn parse_font_family(value: &str, line_num: usize) -> Result<MizuFontFamily, MizuError> {
    match value.trim_matches('"') {
        "sans-serif" => Ok(MizuFontFamily::SansSerif),
        "serif" => Ok(MizuFontFamily::Serif),
        "monospace" => Ok(MizuFontFamily::Monospace),
        _ => Err(MizuError::ParseError(format!(
            "line {line_num}: invalid value `{value}` for `font-family`; \
             only the generic families `sans-serif`, `serif`, `monospace` are \
             accepted — a concrete font name, URL, or @font-face is never a \
             valid Mizu value (fixed allowlist, not a suggestion list)"
        ))),
    }
}

/// Parses `font-weight`: the keywords `normal` (400) / `bold` (700), or a
/// bare numeric weight in the CSS range `100..=900` (e.g. `550`).
pub(super) fn parse_font_weight(value: &str, line_num: usize) -> Result<f32, MizuError> {
    match value {
        "normal" => Ok(400.0),
        "bold" => Ok(700.0),
        _ => {
            let weight = value.parse::<f32>().map_err(|_| {
                MizuError::ParseError(format!(
                    "line {line_num}: invalid value `{value}` for `font-weight`; \
                     valid values: normal, bold, or a number 100-900"
                ))
            })?;
            if !(100.0..=900.0).contains(&weight) {
                return Err(MizuError::ParseError(format!(
                    "line {line_num}: invalid numeric `font-weight` value `{value}`; \
                     must be between 100 and 900"
                )));
            }
            Ok(weight)
        }
    }
}

/// Maps a Mizu `justify` value string to [`JustifyContent`].
pub(super) fn parse_justify_content(
    value: &str,
    line_num: usize,
) -> Result<JustifyContent, MizuError> {
    match value {
        "start" => Ok(JustifyContent::Start),
        "end" => Ok(JustifyContent::End),
        "center" => Ok(JustifyContent::Center),
        "stretch" => Ok(JustifyContent::Stretch),
        "space-between" => Ok(JustifyContent::SpaceBetween),
        "space-around" => Ok(JustifyContent::SpaceAround),
        "space-evenly" => Ok(JustifyContent::SpaceEvenly),
        _ => Err(MizuError::ParseError(format!(
            "line {line_num}: invalid value `{value}` for `justify`; \
             valid values: start, end, center, stretch, \
             space-between, space-around, space-evenly"
        ))),
    }
}

/// Maps a Mizu `align` value string to [`AlignItems`].
pub(super) fn parse_align_items(value: &str, line_num: usize) -> Result<AlignItems, MizuError> {
    match value {
        "start" => Ok(AlignItems::Start),
        "end" => Ok(AlignItems::End),
        "center" => Ok(AlignItems::Center),
        "stretch" => Ok(AlignItems::Stretch),
        "baseline" => Ok(AlignItems::Baseline),
        _ => Err(MizuError::ParseError(format!(
            "line {line_num}: invalid value `{value}` for `align`; \
             valid values: start, end, center, stretch, baseline"
        ))),
    }
}
