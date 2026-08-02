//! Entry points: `parse_style`/`parse_style_with_variants` (indentation-based
//! selector/property scanning) and `apply_property` (one declaration's
//! token(s) into the right `StyleRules` field).

use std::collections::HashMap;

use crate::core::errors::MizuError;

use super::types::*;
use super::values::*;
use taffy::style::{Display, FlexDirection};

/// Parses the `style_block` produced by [`super::split_source`] into a
/// `HashMap` keyed by class name (without the leading `.`).
///
/// The function detects the **baseline indentation level** dynamically from
/// the first non-empty line so that it works regardless of how many spaces the
/// splitter preserved from the original `.mizu` file.
///
/// # Errors
///
/// Returns [`MizuError::ParseError`] for any of the following:
///
/// * A line uses `:` or `;` (CSS syntax noise).
/// * A property appears outside any selector block.
/// * An unknown property name is encountered.
/// * A property value is syntactically invalid (bad number, bad hex, etc.).
/// * A hex colour contains non-hex characters or has an invalid length.
/// * A flex property uses an unsupported value.
/// * A property line is missing its value.
///
/// # Examples
///
/// ```
/// use mizu_core::parser::style::parse_style;
///
/// let block = "    .card\n        padding 20\n        background #ffffff\n";
/// let rules = parse_style(block).unwrap();
/// assert!(rules.contains_key("card"));
/// ```
pub fn parse_style(style_content: &str) -> Result<HashMap<String, StyleRules>, MizuError> {
    parse_style_with_variants(style_content).map(|(base, _variants)| base)
}

/// Like [`parse_style`], but also returns the document's `@min-width` /
/// `@max-width` / `@dark` / `@light` variant rule sets (ux-6) — see
/// `docs/design/responsive.md`. `parse_style` is a thin wrapper over this
/// function that discards the variants, kept as the stable, back-compatible
/// entry point for callers that only need base rules.
///
/// # Errors
///
/// Same conditions as [`parse_style`], plus: an unrecognised `@`-condition
/// token, or a `@min-width`/`@max-width` missing its numeric argument.
pub fn parse_style_with_variants(
    style_content: &str,
) -> Result<(HashMap<String, StyleRules>, Vec<StyleVariant>), MizuError> {
    let mut result: HashMap<String, StyleRules> = HashMap::new();
    let mut variants: Vec<StyleVariant> = Vec::new();
    let mut baseline: Option<usize> = None;
    let mut current_class: Option<String> = None;
    let mut current_conditions: Vec<VariantCondition> = Vec::new();
    let mut current_rules = StyleRules::default();
    // Accumulates non-structural (property-level) errors so all mistakes in a
    // block are reported in one pass rather than stopping at the first bad line.
    let mut prop_errors: Vec<MizuError> = Vec::new();

    for (raw_idx, line) in style_content.lines().enumerate() {
        let line_num = raw_idx + 1;
        let trimmed = line.trim();

        // ── Skip blank lines ──────────────────────────────────────────────────
        if trimmed.is_empty() {
            continue;
        }

        // ── Targeted: absolute URL in background-image ────────────────────────
        // Give the actionable message before the generic no-`:` rule below
        // catches the `://` and reports a confusing "syntax noise" error.
        if trimmed.starts_with("background-image") && trimmed.contains("://") {
            return Err(MizuError::ParseError(format!(
                "line {line_num}: absolute URLs are not allowed in background-image; \
                 use a local relative path"
            )));
        }

        // ── Reject CSS syntax noise immediately ───────────────────────────────
        // Colons and semicolons are never valid in Mizu style syntax.
        if trimmed.contains(':') || trimmed.contains(';') {
            return Err(MizuError::ParseError(format!(
                "line {line_num}: Mizu style syntax does not use `:` or `;`; \
                 write properties as `key value` without separators \
                 (found: `{trimmed}`)"
            )));
        }

        // ── Measure indentation ───────────────────────────────────────────────
        let indent = leading_spaces(line);

        // Set or read the baseline from the first non-empty line.
        let base = if let Some(b) = baseline {
            b
        } else {
            baseline = Some(indent);
            indent
        };

        if indent < base {
            return Err(MizuError::ParseError(format!(
                "line {line_num}: unexpected dedent — indentation ({indent} spaces) \
                 is less than the baseline ({base} spaces)"
            )));
        }

        // ── Root-level line (class selector) ──────────────────────────────────
        if indent == base {
            // Flush the previous class/variant into the result.
            if let Some(name) = current_class.take() {
                if current_conditions.is_empty() {
                    result.insert(name, current_rules);
                } else {
                    variants.push(StyleVariant {
                        selector: name,
                        conditions: std::mem::take(&mut current_conditions),
                        rules: current_rules,
                    });
                }
                current_rules = StyleRules::default();
            }

            // The selector is the first whitespace-separated token; any
            // remaining tokens are `@condition`s gating this rule set
            // (ux-6) — e.g. `.sidebar @max-width 599` or `.card @dark`.
            let mut token_iter = trimmed.split_whitespace();
            let selector_token = token_iter.next().unwrap_or("");
            let condition_tokens: Vec<&str> = token_iter.collect();

            let mut selector_name = selector_token.to_owned();
            if let Some(stripped) = selector_name.strip_prefix('.') {
                selector_name = stripped.to_owned();
                if selector_name.is_empty() {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: class name cannot be empty"
                    )));
                }
            } else if let Some(stripped) = selector_name.strip_prefix('#') {
                if stripped.is_empty() {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: id name cannot be empty"
                    )));
                }
                // Kept `#`-prefixed in the stored key (unlike class, which
                // strips its `.`) so an id selector can never collide with a
                // class or tag of the same bare name in the shared rules map
                // -- `#` is not a legal character in either.
            } else {
                let is_valid_tag = matches!(
                    selector_name.to_lowercase().as_str(),
                    "doc"
                        | "box"
                        | "text"
                        | "button"
                        | "input"
                        | "image"
                        | "markdown"
                        | "form"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                );
                if !is_valid_tag {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: selector `{selector_name}` must start with `.`"
                    )));
                }
            }

            current_conditions = parse_variant_conditions(&condition_tokens, line_num)?;
            current_class = Some(selector_name);

        // ── Property line (indent > base) ─────────────────────────────────────
        } else {
            if current_class.is_none() {
                return Err(MizuError::ParseError(format!(
                    "line {line_num}: property `{trimmed}` appears outside \
                     of any block (no selector has been seen yet)"
                )));
            }

            // Split into `key` and `value` on the first space.
            let mut parts = trimmed.splitn(2, ' ');

            let key = match parts.next() {
                Some(k) if !k.is_empty() => k,
                _ => {
                    prop_errors.push(MizuError::ParseError(format!(
                        "line {line_num}: empty property line"
                    )));
                    continue;
                }
            };

            let value_opt = parts.next().and_then(|s| {
                let s = s.trim();
                if s.is_empty() { None } else { Some(s) }
            });

            match value_opt {
                None => {
                    prop_errors.push(MizuError::ParseError(format!(
                        "line {line_num}: property `{key}` has no value"
                    )));
                }
                Some(value) => {
                    if let Err(e) = apply_property(key, value, &mut current_rules, line_num) {
                        prop_errors.push(e);
                    }
                }
            }
        }
    }

    // Flush the last class/variant.
    if let Some(name) = current_class {
        if current_conditions.is_empty() {
            result.insert(name, current_rules);
        } else {
            variants.push(StyleVariant {
                selector: name,
                conditions: current_conditions,
                rules: current_rules,
            });
        }
    }

    match prop_errors.len() {
        0 => Ok((result, variants)),
        1 => Err(prop_errors.remove(0)),
        _ => Err(MizuError::MultipleErrors(prop_errors)),
    }
}

/// Parses the `@condition` tokens trailing a selector (ux-6) — e.g.
/// `["@max-width", "600"]` or `["@dark"]`. Empty input is valid (an
/// unconditioned selector) and yields an empty `Vec`.
fn parse_variant_conditions(
    tokens: &[&str],
    line_num: usize,
) -> Result<Vec<VariantCondition>, MizuError> {
    let mut conditions = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "@dark" => {
                conditions.push(VariantCondition::Dark);
                i += 1;
            }
            "@light" => {
                conditions.push(VariantCondition::Light);
                i += 1;
            }
            kw @ ("@min-width" | "@max-width") => {
                let value = tokens.get(i + 1).ok_or_else(|| {
                    MizuError::ParseError(format!(
                        "line {line_num}: `{kw}` requires a pixel value, e.g. `{kw} 600`"
                    ))
                })?;
                let px = value.parse::<f32>().map_err(|_| {
                    MizuError::ParseError(format!(
                        "line {line_num}: invalid value `{value}` for `{kw}`; \
                         expected a number, e.g. `600`"
                    ))
                })?;
                conditions.push(if kw == "@min-width" {
                    VariantCondition::MinWidth(px)
                } else {
                    VariantCondition::MaxWidth(px)
                });
                i += 2;
            }
            other => {
                return Err(MizuError::ParseError(format!(
                    "line {line_num}: unknown variant condition `{other}`; \
                     valid: @min-width N, @max-width N, @dark, @light"
                )));
            }
        }
    }
    Ok(conditions)
}

/// Routes a single `key value` pair into the appropriate field of `rules`.
fn apply_property(
    key: &str,
    value: &str,
    rules: &mut StyleRules,
    line_num: usize,
) -> Result<(), MizuError> {
    match key {
        // ── Layout dimensions ─────────────────────────────────────────────────
        "width" => rules.width = Some(parse_dimension(value, key, line_num)?),
        "height" => rules.height = Some(parse_dimension(value, key, line_num)?),
        "padding" => rules.padding = Some(parse_dimension(value, key, line_num)?),
        "margin" => rules.margin = Some(parse_dimension(value, key, line_num)?),
        "gap" => rules.gap = Some(parse_dimension(value, key, line_num)?),
        "margin-inline-start" => {
            rules.margin_inline_start = Some(parse_dimension(value, key, line_num)?);
        }
        "margin-inline-end" => {
            rules.margin_inline_end = Some(parse_dimension(value, key, line_num)?);
        }
        "padding-inline-start" => {
            rules.padding_inline_start = Some(parse_dimension(value, key, line_num)?);
        }
        "padding-inline-end" => {
            rules.padding_inline_end = Some(parse_dimension(value, key, line_num)?);
        }

        // ── Taffy flex properties ─────────────────────────────────────────────
        "flex-direction" => {
            rules.flex_direction = Some(match value {
                "row" => FlexDirection::Row,
                "column" => FlexDirection::Column,
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: invalid value `{value}` for `flex-direction`; \
                         must be `row` or `column`"
                    )));
                }
            });
        }
        "direction" => {
            return Err(MizuError::ParseError(format!(
                "line {line_num}: `direction` was renamed to `flex-direction` \
                 (it collided with CSS's unrelated `direction: ltr|rtl`); \
                 use `flex-direction row` or `flex-direction column`"
            )));
        }
        "justify" => rules.justify = Some(parse_justify_content(value, line_num)?),
        "align" => rules.align = Some(parse_align_items(value, line_num)?),

        // ── Visual properties ─────────────────────────────────────────────────
        "background" => rules.background = Some(parse_background(value, line_num)?),
        "background-image" => {
            let path = value.trim_matches('"');
            // Same rule as `image src`: a literal absolute network URL bypasses
            // the `urls` registry and is a covert network channel. Only a local
            // relative path is accepted here (the style renderer does not
            // resolve media aliases for background-image).
            if path.starts_with("mizu://")
                || path.starts_with("http://")
                || path.starts_with("https://")
            {
                return Err(MizuError::ParseError(format!(
                    "line {line_num}: absolute URLs are not allowed in background-image; \
                     use a local relative path"
                )));
            }
            rules.background_image = Some(path.to_string());
        }
        "background-size" => {
            rules.background_size = Some(match value {
                "stretch" => MizuBackgroundSize::Stretch,
                "cover" => MizuBackgroundSize::Cover,
                "tile" => MizuBackgroundSize::Tile,
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: invalid value `{value}` for `background-size`"
                    )));
                }
            });
        }
        "color" => rules.color = Some(parse_color(value, line_num)?),
        "font-size" => {
            rules.font_size = Some(parse_f32(value, key, line_num)?);
        }
        "border-radius" => {
            rules.border_radius = Some(parse_f32(value, key, line_num)?);
        }
        "border-width" => {
            rules.border_width = Some(parse_f32(value, key, line_num)?);
        }
        "border-color" => {
            rules.border_color = Some(parse_color(value, line_num)?);
        }

        // ── Typography (ux-3) ──────────────────────────────────────────────────
        "font-family" => {
            rules.font_family = Some(parse_font_family(value, line_num)?);
        }
        "font-weight" => {
            rules.font_weight = Some(parse_font_weight(value, line_num)?);
        }
        "font-style" => {
            rules.font_style = Some(match value {
                "normal" => MizuFontStyle::Normal,
                "italic" => MizuFontStyle::Italic,
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: invalid value `{value}` for `font-style`; \
                         valid values: normal, italic"
                    )));
                }
            });
        }
        "text-align" => {
            rules.text_align = Some(match value {
                "left" => MizuTextAlign::Left,
                "center" => MizuTextAlign::Center,
                "right" => MizuTextAlign::Right,
                "justify" => MizuTextAlign::Justify,
                "start" => MizuTextAlign::Start,
                "end" => MizuTextAlign::End,
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: invalid value `{value}` for `text-align`; \
                         valid values: left, center, right, justify, start, end"
                    )));
                }
            });
        }
        "line-height" => {
            rules.line_height = Some(parse_f32(value, key, line_num)?);
        }
        "text-decoration" => {
            rules.underline = Some(match value {
                "none" => false,
                "underline" => true,
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: invalid value `{value}` for `text-decoration`; \
                         valid values: none, underline"
                    )));
                }
            });
        }

        // ── Phase-11: overflow & z-index ──────────────────────────────────────
        "overflow" => {
            rules.overflow = match value {
                "visible" => MizuOverflow::Visible,
                "hidden" => MizuOverflow::Hidden,
                "scroll" => MizuOverflow::Scroll,
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: invalid value `{value}` for `overflow`; \
                         valid values: visible, hidden, scroll"
                    )));
                }
            };
        }
        "z-index" => {
            rules.z_index = value.parse::<i32>().map_err(|_| {
                MizuError::ParseError(format!(
                    "line {line_num}: invalid integer `{value}` for `z-index`; \
                     expected a whole number, e.g. `0`, `-1`, `10`"
                ))
            })?;
        }
        "display" => {
            rules.display = Some(match value {
                "none" => Display::None,
                "flex" => Display::Flex,
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: invalid value `{value}` for `display`; \
                         valid values: none, flex"
                    )));
                }
            });
        }

        // ── Unknown property ──────────────────────────────────────────────────
        unknown => {
            return Err(MizuError::ParseError(format!(
                "line {line_num}: unknown style property `{unknown}`; \
                 valid properties: width, height, padding, margin, gap, \
                 margin-inline-start, margin-inline-end, padding-inline-start, padding-inline-end, \
                 flex-direction, justify, align, background, background-image, background-size, color, \
                 font-size, border-radius, border-width, border-color, overflow, z-index, display, \
                 font-family, font-weight, font-style, text-align, line-height, text-decoration"
            )));
        }
    }
    Ok(())
}
