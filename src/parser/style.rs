//! # `style` — Mizu Style Sheet Parser (Phase 3 + Phase 11)
//!
//! This module tokenizes the `style_block` produced by [`super::splitter`]
//! into a typed, validated [`HashMap`] of class-name → [`StyleRules`] pairs,
//! ready to be handed to the Taffy layout engine in Phase 4 and the Vello
//! rendering pipeline in Phase 11.
//!
//! ## Grammar (excerpt from MIZU_GUIDELINES.md § 2.2)
//!
//! ```text
//! .class_name
//!     property value
//!     property value
//! .another_class
//!     property value
//! ```
//!
//! Rules:
//! * The `.class_name` selector sits at the **baseline indentation level**
//!   (the minimum indentation found in the block — set dynamically from the
//!   first non-empty line).
//! * Properties are on lines indented *deeper* than baseline.
//! * Syntax is `key value` — **no colons, no semicolons**.
//! * Hex colours start with `#` and are **unquoted**.
//!
//! ## Type Mapping
//!
//! | Mizu keyword | Mizu value form    | Rust representation       |
//! |--------------|--------------------|---------------------------|
//! | `width`, `height`, `padding`, `margin`, `gap` | `100` or `50%` | [`MizuDimension`] |
//! | `direction`  | `row` \| `column`  | [`taffy::style::FlexDirection`] |
//! | `justify`    | `center` etc.      | [`taffy::style::JustifyContent`] |
//! | `align`      | `stretch` etc.     | [`taffy::style::AlignItems`] |
//! | `background`, `color` | `#rrggbb` | [`MizuColor`] |
//! | `font-size`, `border-radius` | `14` | `f32` |
//! | `overflow`   | `visible` \| `hidden` \| `scroll` | [`MizuOverflow`] |
//! | `z-index`    | `-5`, `0`, `10`    | `i32` |
//!
//! ## Pipeline Position
//!
//! ```text
//! style_block: String   (from parser::splitter)
//!        │
//!        ▼
//! ┌─────────────────────────────┐
//! │  parser::style::parse_style │  ← this module
//! │  (Phase 3)                  │
//! │  • indentation detection    │
//! │  • selector / property scan │
//! │  • hex color parsing        │
//! │  • Taffy type mapping       │
//! └─────────────┬───────────────┘
//!               │  HashMap<String, StyleRules>
//!               ▼
//!       (Phase 4) Taffy layout tree construction
//! ```

#![forbid(unsafe_code)]

use std::collections::HashMap;

use taffy::style::{AlignItems, Display, FlexDirection, JustifyContent};

use crate::core::errors::MizuError;


/// An RGBA colour parsed from a Mizu hex literal (`#rgb`, `#rrggbb`, or
/// `#rrggbbaa`).
///
/// Alpha defaults to `0xFF` (fully opaque) when the source uses the 3- or
/// 6-digit form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MizuColor {
    /// Red channel, 0–255.
    pub r: u8,
    /// Green channel, 0–255.
    pub g: u8,
    /// Blue channel, 0–255.
    pub b: u8,
    /// Alpha channel, 0–255.  `0xFF` = fully opaque.
    pub a: u8,
}

impl MizuColor {
    /// Constructs a fully-opaque colour.
    #[must_use]
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xFF }
    }

    /// Constructs a colour with an explicit alpha channel.
    #[must_use]
    pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

/// A background value that can be a solid color or a linear gradient.
#[derive(Debug, Clone, PartialEq)]
pub enum MizuBackground {
    /// A solid flat color.
    Solid(MizuColor),
    /// A linear gradient with an angle and two stop colors.
    LinearGradient {
        /// The angle in degrees.
        angle: f32,
        /// The starting color.
        start: MizuColor,
        /// The ending color.
        end: MizuColor,
    },
}

/// The sizing strategy for a background image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MizuBackgroundSize {
    #[default]
    /// Stretches to fill the node box, ignoring aspect ratio.
    Stretch,
    /// Scales to cover the node box, preserving aspect ratio (cropping if necessary).
    Cover,
    /// Tiles the image at its natural size to fill the node box.
    Tile,
}


/// Controls how a container's children behave when they overflow its bounds.
///
/// Maps to [`taffy::style::Overflow`] for layout and to Vello layer clipping
/// in the GPU paint pass.
///
/// | Mizu value | Layout effect          | Rendering effect                      |
/// |------------|------------------------|---------------------------------------|
/// | `visible`  | Content bleeds out     | No clip                               |
/// | `hidden`   | Minimum size is `0`    | Children clipped to container rect    |
/// | `scroll`   | Minimum size is `0`    | Clip + scrollable via mouse wheel     |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MizuOverflow {
    /// Children paint freely outside the container boundary.
    #[default]
    Visible,
    /// Children are clipped to the container rectangle — not scrollable.
    Hidden,
    /// Children are clipped and the container is scrollable via mouse wheel.
    Scroll,
}

/// A dimension value used for `width`, `height`, `padding`, `margin`, and
/// `gap` properties.
///
/// Mizu supports two forms:
/// * **Pixels** — a bare number, e.g. `padding 20`.
/// * **Percent** — a number followed by `%`, e.g. `width 50%`.
#[derive(Debug, Clone, PartialEq)]
pub enum MizuDimension {
    /// A fixed pixel value.
    Pixels(f32),
    /// A percentage of the parent dimension.
    Percent(f32),
}

/// The parsed, validated style rules for a single Mizu class selector.
///
/// All fields are `Option` — omitted properties remain `None` and will fall
/// back to layout-engine defaults during Phase 4 tree construction.
///
/// ## Taffy Integration
///
/// The three Taffy fields (`direction`, `justify`, `align`) use Taffy's own
/// enums directly so that the values can be moved into a `taffy::style::Style`
/// struct without any conversion layer.
#[derive(Debug, Clone, Default)]
pub struct StyleRules {
    // ── Layout dimensions ────────────────────────────────────────────────────
    /// `width` property.
    pub width: Option<MizuDimension>,
    /// `height` property.
    pub height: Option<MizuDimension>,
    /// Uniform `padding` property (all four sides).
    pub padding: Option<MizuDimension>,
    /// Uniform `margin` property (all four sides).
    pub margin: Option<MizuDimension>,
    /// Flex/grid `gap` property.
    pub gap: Option<MizuDimension>,

    // ── Taffy flex properties ─────────────────────────────────────────────────
    /// `direction` — maps to [`taffy::style::FlexDirection`].
    /// Valid values: `row`, `column`.
    pub direction: Option<FlexDirection>,
    /// `justify` — maps to [`taffy::style::JustifyContent`].
    /// Valid values: `start`, `end`, `center`, `space-between`,
    /// `space-around`, `space-evenly`, `stretch`.
    pub justify: Option<JustifyContent>,
    /// `align` — maps to [`taffy::style::AlignItems`].
    /// Valid values: `start`, `end`, `center`, `stretch`, `baseline`.
    pub align: Option<AlignItems>,

    // ── Visual properties ─────────────────────────────────────────────────────
    /// `background` — unquoted hex colour (e.g. `#1a2b3c`), rgba, or linear-gradient.
    pub background: Option<MizuBackground>,
    /// `background-image` — path to an image file.
    pub background_image: Option<String>,
    /// `background-size` — stretch, cover, or tile.
    pub background_size: Option<MizuBackgroundSize>,
    /// `color` — text foreground color.
    pub color: Option<MizuColor>,
    /// `font-size` — point/pixel size, e.g. `14`.
    pub font_size: Option<f32>,
    /// `border-radius` — corner radius in pixels, e.g. `8`.
    pub border_radius: Option<f32>,
    /// `border-width` — border thickness in pixels.
    pub border_width: Option<f32>,
    /// `border-color` — border color.
    pub border_color: Option<MizuColor>,

    // ── Phase-11 layout mechanics ─────────────────────────────────────────────
    /// `overflow` — controls child clipping and scroll behaviour.
    ///
    /// Defaults to [`MizuOverflow::Visible`] (no clipping, no scrolling).
    pub overflow: MizuOverflow,
    /// `z-index` — painting order depth within a sibling group.
    ///
    /// Higher values are painted last (on top). Negative values are valid.
    /// Defaults to `0`.
    pub z_index: i32,
    /// `display` — overrides the Taffy display mode for this node.
    ///
    /// `None` = use Taffy default (`Flex`). Explicit values: `none` (hide),
    /// `flex` (re-show after a conditional `none`).
    pub display: Option<Display>,
}

impl StyleRules {
    /// Merges another set of rules into this one. `other` rules take precedence
    /// (e.g. class styles overriding tag styles).
    pub fn merge(mut self, other: Self) -> Self {
        if other.width.is_some() {
            self.width = other.width;
        }
        if other.height.is_some() {
            self.height = other.height;
        }
        if other.padding.is_some() {
            self.padding = other.padding;
        }
        if other.margin.is_some() {
            self.margin = other.margin;
        }
        if other.gap.is_some() {
            self.gap = other.gap;
        }

        if other.direction.is_some() {
            self.direction = other.direction;
        }
        if other.justify.is_some() {
            self.justify = other.justify;
        }
        if other.align.is_some() {
            self.align = other.align;
        }

        if other.background.is_some() {
            self.background = other.background;
        }
        if other.background_image.is_some() {
            self.background_image = other.background_image;
        }
        if other.background_size.is_some() {
            self.background_size = other.background_size;
        }
        if other.color.is_some() {
            self.color = other.color;
        }
        if other.font_size.is_some() {
            self.font_size = other.font_size;
        }
        if other.border_radius.is_some() {
            self.border_radius = other.border_radius;
        }
        if other.border_width.is_some() {
            self.border_width = other.border_width;
        }
        if other.border_color.is_some() {
            self.border_color = other.border_color;
        }

        // Primitive overwrites
        if other.overflow != MizuOverflow::Visible {
            self.overflow = other.overflow;
        }
        if other.z_index != 0 {
            self.z_index = other.z_index;
        }
        if other.display.is_some() {
            self.display = other.display;
        }

        self
    }
}


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
/// use mizu::parser::style::parse_style;
///
/// let block = "    .card\n        padding 20\n        background #ffffff\n";
/// let rules = parse_style(block).unwrap();
/// assert!(rules.contains_key("card"));
/// ```
pub fn parse_style(style_content: &str) -> Result<HashMap<String, StyleRules>, MizuError> {
    let mut result: HashMap<String, StyleRules> = HashMap::new();
    let mut baseline: Option<usize> = None;
    let mut current_class: Option<String> = None;
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
            // Flush the previous class into the result map.
            if let Some(name) = current_class.take() {
                result.insert(name, current_rules);
                current_rules = StyleRules::default();
            }

            let mut selector_name = trimmed.to_owned();
            if let Some(stripped) = selector_name.strip_prefix('.') {
                selector_name = stripped.to_owned();
                if selector_name.is_empty() {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: class name cannot be empty"
                    )));
                }
            } else {
                let is_valid_tag = matches!(
                    selector_name.to_lowercase().as_str(),
                    "window" | "box" | "text" | "button" | "input" | "image" | "markdown"
                );
                if !is_valid_tag {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: selector `{selector_name}` must start with `.`"
                    )));
                }
            }

            // Selectors must not contain spaces (multi-token selectors are not
            // supported in Mizu V1).
            if selector_name.contains(' ') {
                return Err(MizuError::ParseError(format!(
                    "line {line_num}: selector `{selector_name}` must not contain spaces"
                )));
            }

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

    // Flush the last class.
    if let Some(name) = current_class {
        result.insert(name, current_rules);
    }

    match prop_errors.len() {
        0 => Ok(result),
        1 => Err(prop_errors.remove(0)),
        _ => Err(MizuError::MultipleErrors(prop_errors)),
    }
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

        // ── Taffy flex properties ─────────────────────────────────────────────
        "direction" => {
            rules.direction = Some(match value {
                "row" => FlexDirection::Row,
                "column" => FlexDirection::Column,
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: invalid value `{value}` for `direction`; \
                         must be `row` or `column`"
                    )));
                }
            });
        }
        "justify" => rules.justify = Some(parse_justify_content(value, line_num)?),
        "align" => rules.align = Some(parse_align_items(value, line_num)?),

        // ── Visual properties ─────────────────────────────────────────────────
        "background" => rules.background = Some(parse_background(value, line_num)?),
        "background-image" => {
            let path = value.trim_matches('"');
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
                        "line {line_num}: display supporta solo: none, flex"
                    )));
                }
            });
        }

        // ── Unknown property ──────────────────────────────────────────────────
        unknown => {
            return Err(MizuError::ParseError(format!(
                "line {line_num}: unknown style property `{unknown}`; \
                 valid properties: width, height, padding, margin, gap, \
                 direction, justify, align, background, background-image, background-size, color, \
                 font-size, border-radius, border-width, border-color, overflow, z-index, display"
            )));
        }
    }
    Ok(())
}


/// Returns the number of leading space characters in `line`.
///
/// Only space (`U+0020`) is counted.  Tabs are deliberately excluded since the
/// Mizu spec mandates space-based indentation.
#[inline]
fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Parses a [`MizuDimension`] from a token that is either a plain `f32`
/// (pixels) or an `f32` followed immediately by `%` (percent).
fn parse_dimension(token: &str, prop: &str, line_num: usize) -> Result<MizuDimension, MizuError> {
    if let Some(pct_str) = token.strip_suffix('%') {
        pct_str
            .parse::<f32>()
            .map(MizuDimension::Percent)
            .map_err(|_| {
                MizuError::ParseError(format!(
                    "line {line_num}: invalid percentage `{token}` for `{prop}`; \
                 expected a number followed by `%`, e.g. `50%`"
                ))
            })
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
fn parse_f32(token: &str, prop: &str, line_num: usize) -> Result<f32, MizuError> {
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
fn parse_color(token: &str, line_num: usize) -> Result<MizuColor, MizuError> {
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

fn parse_background(token: &str, line_num: usize) -> Result<MizuBackground, MizuError> {
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
fn expand_nibble(nibble: u8) -> u8 {
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
fn parse_hex_byte(s: &str, token: &str, line_num: usize) -> Result<u8, MizuError> {
    u8::from_str_radix(s, 16).map_err(|_| {
        MizuError::ParseError(format!(
            "line {line_num}: internal error parsing hex byte `{s}` in `{token}`"
        ))
    })
}

/// Maps a Mizu `justify` value string to [`JustifyContent`].
fn parse_justify_content(value: &str, line_num: usize) -> Result<JustifyContent, MizuError> {
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
fn parse_align_items(value: &str, line_num: usize) -> Result<AlignItems, MizuError> {
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


#[cfg(test)]
mod tests {
    use super::{
        AlignItems, Display, FlexDirection, JustifyContent, MizuBackground, MizuColor,
        MizuDimension, MizuOverflow, parse_color, parse_style,
    };
    use crate::core::errors::MizuError;

    // ────────────────────────────────────────────────────────────────────────
    // Hex colour parser
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn hex_3digit_expands_correctly() {
        // #fff → r=255, g=255, b=255, a=255
        let c = parse_color("#fff", 1).unwrap();
        assert_eq!(c, MizuColor::rgb(0xFF, 0xFF, 0xFF));
    }

    #[test]
    fn hex_3digit_mixed() {
        // #f0a → #ff00aa
        let c = parse_color("#f0a", 1).unwrap();
        assert_eq!(c, MizuColor::rgb(0xFF, 0x00, 0xAA));
    }

    #[test]
    fn hex_6digit_red() {
        let c = parse_color("#ff0000", 1).unwrap();
        assert_eq!(c, MizuColor::rgb(0xFF, 0x00, 0x00));
    }

    #[test]
    fn hex_6digit_lowercase_and_uppercase() {
        let lower = parse_color("#1a2b3c", 1).unwrap();
        let upper = parse_color("#1A2B3C", 1).unwrap();
        assert_eq!(lower, upper);
    }

    #[test]
    fn hex_6digit_black() {
        let c = parse_color("#000000", 1).unwrap();
        assert_eq!(c, MizuColor::rgb(0, 0, 0));
    }

    #[test]
    fn hex_8digit_with_alpha() {
        // #00000080 → semi-transparent black
        let c = parse_color("#00000080", 1).unwrap();
        assert_eq!(c, MizuColor::rgba(0x00, 0x00, 0x00, 0x80));
    }

    #[test]
    fn hex_8digit_fully_transparent() {
        let c = parse_color("#ffffff00", 1).unwrap();
        assert_eq!(c.a, 0x00);
    }

    #[test]
    fn hex_error_no_hash_prefix() {
        let result = parse_color("ff0000", 3);
        assert!(matches!(result, Err(MizuError::ParseError(_))));
    }

    #[test]
    fn hex_error_invalid_characters() {
        let result = parse_color("#gg0000", 5);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("non-hexadecimal")),
            "expected non-hexadecimal error"
        );
    }

    #[test]
    fn hex_error_wrong_length_4_digits() {
        let result = parse_color("#1234", 1);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("length")),
            "expected length error"
        );
    }

    #[test]
    fn hex_error_wrong_length_5_digits() {
        let result = parse_color("#12345", 1);
        assert!(matches!(result, Err(MizuError::ParseError(_))));
    }

    #[test]
    fn hex_error_empty_after_hash() {
        let result = parse_color("#", 1);
        assert!(matches!(result, Err(MizuError::ParseError(_))));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Dimension parsing
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn dimension_pixels_integer() {
        let block = "    .box\n        width 100\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["box"].width, Some(MizuDimension::Pixels(100.0)));
    }

    #[test]
    fn dimension_pixels_fractional() {
        let block = "    .box\n        height 12.5\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["box"].height, Some(MizuDimension::Pixels(12.5)));
    }

    #[test]
    fn dimension_percent() {
        let block = "    .container\n        width 50%\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["container"].width, Some(MizuDimension::Percent(50.0)));
    }

    #[test]
    fn dimension_percent_fractional() {
        let block = "    .col\n        width 33.33%\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["col"].width, Some(MizuDimension::Percent(33.33)));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Flex property parsing
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn direction_row() {
        let block = "    .flex\n        direction row\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["flex"].direction, Some(FlexDirection::Row));
    }

    #[test]
    fn direction_column() {
        let block = "    .flex\n        direction column\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["flex"].direction, Some(FlexDirection::Column));
    }

    #[test]
    fn justify_center() {
        let block = "    .row\n        justify center\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["row"].justify, Some(JustifyContent::Center));
    }

    #[test]
    fn justify_space_between() {
        let block = "    .row\n        justify space-between\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["row"].justify, Some(JustifyContent::SpaceBetween));
    }

    #[test]
    fn justify_space_around() {
        let block = "    .row\n        justify space-around\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["row"].justify, Some(JustifyContent::SpaceAround));
    }

    #[test]
    fn justify_space_evenly() {
        let block = "    .row\n        justify space-evenly\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["row"].justify, Some(JustifyContent::SpaceEvenly));
    }

    #[test]
    fn justify_stretch() {
        let block = "    .row\n        justify stretch\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["row"].justify, Some(JustifyContent::Stretch));
    }

    #[test]
    fn align_stretch() {
        let block = "    .col\n        align stretch\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["col"].align, Some(AlignItems::Stretch));
    }

    #[test]
    fn align_baseline() {
        let block = "    .col\n        align baseline\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["col"].align, Some(AlignItems::Baseline));
    }

    #[test]
    fn align_center() {
        let block = "    .col\n        align center\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["col"].align, Some(AlignItems::Center));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Visual properties
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn background_hex_color() {
        let block = "    .card\n        background #1a2b3c\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(
            rules["card"].background,
            Some(MizuBackground::Solid(MizuColor::rgb(0x1A, 0x2B, 0x3C)))
        );
    }

    #[test]
    fn foreground_color_hex() {
        let block = "    .text\n        color #333333\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["text"].color, Some(MizuColor::rgb(0x33, 0x33, 0x33)));
    }

    #[test]
    fn font_size() {
        let block = "    .label\n        font-size 16\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].font_size, Some(16.0_f32));
    }

    #[test]
    fn border_radius() {
        let block = "    .button\n        border-radius 8\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["button"].border_radius, Some(8.0_f32));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Full stylesheet — integration
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_complex_stylesheet_multiple_classes() {
        // Use a raw string so that `\` on the first line does NOT strip
        // the leading spaces from subsequent lines (Rust string-continuation
        // escape would silently remove them).
        let block = r"
    .card
        width 100%
        padding 20
        background #ffffff
        border-radius 8
    .button
        direction row
        justify center
        align stretch
        background #0077cc
        color #ffffff
        font-size 14
    .header
        height 60
        background #1a1a2e
        color #eee
";
        let rules = parse_style(block).unwrap();

        assert_eq!(rules.len(), 3);

        // .card
        let card = &rules["card"];
        assert_eq!(card.width, Some(MizuDimension::Percent(100.0)));
        assert_eq!(card.padding, Some(MizuDimension::Pixels(20.0)));
        assert_eq!(
            card.background,
            Some(MizuBackground::Solid(MizuColor::rgb(0xFF, 0xFF, 0xFF)))
        );
        assert_eq!(card.border_radius, Some(8.0));

        // .button
        let btn = &rules["button"];
        assert_eq!(btn.direction, Some(FlexDirection::Row));
        assert_eq!(btn.justify, Some(JustifyContent::Center));
        assert_eq!(btn.align, Some(AlignItems::Stretch));
        assert_eq!(
            btn.background,
            Some(MizuBackground::Solid(MizuColor::rgb(0x00, 0x77, 0xCC)))
        );
        assert_eq!(btn.color, Some(MizuColor::rgb(0xFF, 0xFF, 0xFF)));
        assert_eq!(btn.font_size, Some(14.0));

        // .header
        let hdr = &rules["header"];
        assert_eq!(hdr.height, Some(MizuDimension::Pixels(60.0)));
    }

    #[test]
    fn properties_do_not_bleed_between_classes() {
        let block = r"
    .a
        padding 10
    .b
        margin 5
";
        let rules = parse_style(block).unwrap();
        assert!(
            rules["a"].margin.is_none(),
            "`margin` must not bleed from .b into .a"
        );
        assert!(
            rules["b"].padding.is_none(),
            "`padding` must not bleed from .a into .b"
        );
    }

    #[test]
    fn empty_style_block_returns_empty_map() {
        let rules = parse_style("").unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn blank_lines_between_properties_are_skipped() {
        let block = "\
    .box

        width 100

        height 50

";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["box"].width, Some(MizuDimension::Pixels(100.0)));
        assert_eq!(rules["box"].height, Some(MizuDimension::Pixels(50.0)));
    }

    #[test]
    fn parse_style_all_dimension_properties() {
        let block = "\
    .layout
        width 200
        height 100
        padding 10
        margin 5
        gap 8
";
        let rules = parse_style(block).unwrap();
        let l = &rules["layout"];
        assert_eq!(l.width, Some(MizuDimension::Pixels(200.0)));
        assert_eq!(l.height, Some(MizuDimension::Pixels(100.0)));
        assert_eq!(l.padding, Some(MizuDimension::Pixels(10.0)));
        assert_eq!(l.margin, Some(MizuDimension::Pixels(5.0)));
        assert_eq!(l.gap, Some(MizuDimension::Pixels(8.0)));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Failure paths
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn error_unknown_property() {
        let block = "    .box\n        color-scheme dark\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("unknown style property")),
            "expected unknown property error, got: {result:?}"
        );
    }

    #[test]
    fn error_colon_separator_rejected() {
        let block = "    .box\n        padding: 20\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("`:` or `;`")),
            "expected colon/semicolon error, got: {result:?}"
        );
    }

    #[test]
    fn error_semicolon_separator_rejected() {
        let block = "    .box\n        padding 20;\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("`:` or `;`")),
            "expected semicolon error, got: {result:?}"
        );
    }

    #[test]
    fn error_missing_property_value() {
        let block = "    .box\n        width\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("no value")),
            "expected missing value error, got: {result:?}"
        );
    }

    #[test]
    fn error_invalid_hex_characters() {
        let block = "    .box\n        background #gg0000\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("non-hexadecimal")),
            "expected non-hex error, got: {result:?}"
        );
    }

    #[test]
    fn error_invalid_hex_length() {
        let block = "    .box\n        color #1234\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("length")),
            "expected length error, got: {result:?}"
        );
    }

    #[test]
    fn error_color_without_hash() {
        let block = "    .box\n        background ff0000\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError for unquoted, un-hashed color"
        );
    }

    #[test]
    fn error_invalid_direction_value() {
        let block = "    .box\n        direction circle\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("direction")),
            "expected direction error, got: {result:?}"
        );
    }

    #[test]
    fn error_invalid_justify_value() {
        let block = "    .box\n        justify middle\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("justify")),
            "expected justify error, got: {result:?}"
        );
    }

    #[test]
    fn error_invalid_align_value() {
        let block = "    .box\n        align top\n";
        let result = parse_style(block).unwrap_err();
        assert!(
            result.to_string().contains("align"),
            "error should name the property"
        );
    }

    #[test]
    fn error_property_outside_class() {
        // Provide a block where the ONLY content is a property-like line
        // with no class selector before it. The baseline is detected from
        // the first non-empty line; since it does not start with `.`, the
        // parser returns a ParseError ("expected class selector starting
        // with '.'" is the concrete message, but any ParseError qualifies).
        let block = "    missing_class_selector\n        padding 20\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError when content appears outside any class block, got: {result:?}"
        );
    }

    #[test]
    fn error_root_level_line_without_dot() {
        let block = "    card\n        padding 10\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("`.`")),
            "expected selector error, got: {result:?}"
        );
    }

    #[test]
    fn error_empty_class_name() {
        let block = "    .\n        padding 10\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("name")),
            "expected class-name error, got: {result:?}"
        );
    }

    #[test]
    fn error_invalid_pixel_value() {
        let block = "    .box\n        padding twenty\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("padding")),
            "expected numeric error for `padding`, got: {result:?}"
        );
    }

    #[test]
    fn error_invalid_percentage_value() {
        let block = "    .box\n        width half%\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError for non-numeric percentage"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Phase-11 overflow property
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn overflow_visible_is_default() {
        // When no overflow is specified the field must default to Visible.
        let block = "    .card\n        padding 10\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["card"].overflow, MizuOverflow::Visible);
    }

    #[test]
    fn overflow_hidden_parsed() {
        let block = "    .clip\n        overflow hidden\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["clip"].overflow, MizuOverflow::Hidden);
    }

    #[test]
    fn overflow_scroll_parsed() {
        let block = "    .scroller\n        overflow scroll\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["scroller"].overflow, MizuOverflow::Scroll);
    }

    #[test]
    fn overflow_visible_explicit() {
        let block = "    .container\n        overflow visible\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["container"].overflow, MizuOverflow::Visible);
    }

    #[test]
    fn overflow_error_invalid_value() {
        let block = "    .box\n        overflow auto\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("overflow")),
            "expected overflow error, got: {result:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Phase-11 z-index property
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn z_index_default_is_zero() {
        let block = "    .layer\n        padding 5\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["layer"].z_index, 0);
    }

    #[test]
    fn z_index_positive() {
        let block = "    .modal\n        z-index 10\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["modal"].z_index, 10);
    }

    #[test]
    fn z_index_negative() {
        let block = "    .behind\n        z-index -5\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["behind"].z_index, -5);
    }

    #[test]
    fn z_index_zero_explicit() {
        let block = "    .normal\n        z-index 0\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["normal"].z_index, 0);
    }

    #[test]
    fn z_index_error_float() {
        let block = "    .box\n        z-index 1.5\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("z-index")),
            "expected z-index integer error, got: {result:?}"
        );
    }

    #[test]
    fn z_index_error_text_value() {
        let block = "    .box\n        z-index top\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("z-index")),
            "expected z-index error for non-integer, got: {result:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Phase-11 overflow + z-index combined
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn overflow_and_z_index_together() {
        let block = r"
    .panel
        overflow scroll
        z-index 2
        background #1a1a2e
";
        let rules = parse_style(block).unwrap();
        let panel = &rules["panel"];
        assert_eq!(panel.overflow, MizuOverflow::Scroll);
        assert_eq!(panel.z_index, 2);
        assert_eq!(
            panel.background,
            Some(MizuBackground::Solid(MizuColor::rgb(0x1A, 0x1A, 0x2E)))
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // MultipleErrors accumulation (Fase D)
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn three_bad_properties_produce_multiple_errors() {
        // Three independently invalid property values — the parser must collect
        // all three instead of stopping at the first.
        let block = "\
    .box
        font-size abc
        z-index def
        direction xyz
";
        let result = parse_style(block);
        match result {
            Err(MizuError::MultipleErrors(errs)) => {
                assert_eq!(errs.len(), 3, "expected 3 errors, got: {errs:?}");
                // Each sub-error must be a ParseError with context.
                for e in &errs {
                    assert!(
                        matches!(e, MizuError::ParseError(_)),
                        "sub-error should be ParseError, got: {e:?}"
                    );
                }
            }
            other => panic!("expected MultipleErrors, got: {other:?}"),
        }
    }

    #[test]
    fn two_bad_properties_produce_multiple_errors() {
        let block = "\
    .card
        width bad-value
        height also-bad
";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::MultipleErrors(ref v)) if v.len() == 2),
            "expected MultipleErrors with 2 items, got: {result:?}"
        );
    }

    #[test]
    fn one_bad_property_produces_single_parse_error() {
        // Single property error → unwrapped ParseError for backwards compat.
        let block = "    .box\n        z-index bad\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "single error should be ParseError, got: {result:?}"
        );
    }

    #[test]
    fn multiple_errors_display_includes_count() {
        let block = "\
    .card
        font-size bad
        z-index bad
        direction bad
";
        let err = parse_style(block).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("3 parse error"),
            "display should mention 3 errors, got: `{msg}`"
        );
    }

    #[test]
    fn valid_properties_between_bad_ones_are_applied() {
        // A valid `padding` between two bad properties: the valid value must be
        // retained in the output even though the function ultimately returns Err.
        // This behaviour is intentional — the partial result is discarded at the
        // call site (Err path), but we verify the accumulation logic itself.
        let block = "\
    .x
        font-size abc
        padding 20
        direction bad
";
        // Should be MultipleErrors with exactly 2 entries (font-size + direction).
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::MultipleErrors(ref v)) if v.len() == 2),
            "expected MultipleErrors(2), got: {result:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // display property
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_display_none_parsed() {
        let block = "    .hidden\n        display none\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["hidden"].display, Some(Display::None));
    }

    #[test]
    fn test_display_flex_parsed() {
        let block = "    .visible\n        display flex\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["visible"].display, Some(Display::Flex));
    }

    #[test]
    fn test_display_other_value_error() {
        let block = "    .box\n        display block\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("display supporta solo: none, flex")),
            "expected display error, got: {result:?}"
        );
    }

    #[test]
    fn test_display_conditional_class_active_overrides_to_none() {
        // Base class sets display flex; conditional class (active) sets display none.
        // Merging base + conditional must yield Display::None.
        let base_block = "    .base\n        display flex\n";
        let cond_block = "    .nascosto\n        display none\n";
        let base_rules = parse_style(base_block).unwrap();
        let cond_rules = parse_style(cond_block).unwrap();
        let merged = base_rules["base"]
            .clone()
            .merge(cond_rules["nascosto"].clone());
        assert_eq!(merged.display, Some(Display::None));
    }

    #[test]
    fn test_display_conditional_class_not_active_keeps_flex() {
        // Base class sets display flex; conditional class NOT applied.
        // Only the base StyleRules is used — must yield Display::Flex.
        let base_block = "    .base\n        display flex\n";
        let base_rules = parse_style(base_block).unwrap();
        assert_eq!(base_rules["base"].display, Some(Display::Flex));
    }
}
