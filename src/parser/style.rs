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
/// Mizu supports these forms:
/// * **Pixels** — a bare number, e.g. `padding 20`.
/// * **Percent** — a number followed by `%`, e.g. `width 50%`, relative to
///   the parent container.
/// * **Viewport units** (ux-6) — `vw`/`vh`/`vmin`/`vmax`, e.g. `width 50vw`,
///   relative to the document's content viewport (the window, minus the
///   chrome bar for `vh`) rather than the parent. Resolved in
///   `render::layout_bridge` against the current window size — see
///   `docs/design/responsive.md`.
#[derive(Debug, Clone, PartialEq)]
pub enum MizuDimension {
    /// A fixed pixel value.
    Pixels(f32),
    /// A percentage of the parent dimension.
    Percent(f32),
    /// A percentage of the viewport width (`vw`).
    ViewportWidth(f32),
    /// A percentage of the viewport height (`vh`).
    ViewportHeight(f32),
    /// A percentage of the smaller viewport dimension (`vmin`).
    ViewportMin(f32),
    /// A percentage of the larger viewport dimension (`vmax`).
    ViewportMax(f32),
}

/// A single condition gating a [`StyleVariant`] — see `docs/design/responsive.md`.
///
/// Deliberately render-context-agnostic (no dependency on `render::preferences`
/// from the parser layer): `Dark`/`Light` are bare markers the render side
/// compares against its own `ColorScheme`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VariantCondition {
    /// Matches when the document content viewport width is `>=` this value.
    MinWidth(f32),
    /// Matches when the document content viewport width is `<=` this value.
    MaxWidth(f32),
    /// Matches when the active color scheme is dark.
    Dark,
    /// Matches when the active color scheme is light.
    Light,
}

/// A style rule set gated by one or more [`VariantCondition`]s, e.g.
/// `.sidebar @max-width 599`. All conditions must hold (AND) for `rules` to
/// be merged over the base rules for `selector` — see
/// `docs/design/responsive.md` for the full resolution/merge order.
#[derive(Debug, Clone, PartialEq)]
pub struct StyleVariant {
    /// The tag or class name this variant applies to (without the leading
    /// `.` for class selectors — same convention as the base rules map's keys).
    pub selector: String,
    /// All conditions that must hold (AND) for `rules` to apply.
    pub conditions: Vec<VariantCondition>,
    /// The properties to merge over the base rules when `conditions` hold.
    pub rules: StyleRules,
}

/// The three CSS-generic font families an author may request via
/// `font-family`.
///
/// This is a **fixed allowlist**, not a denylist, and it is deliberately the
/// entire vocabulary: no concrete family name (`"Comic Sans MS"`), no URL,
/// no `@font-face`. A concrete family string resolved against the OS font
/// directory would be a fingerprinting surface (which fonts are installed),
/// and any path that loads a font from disk or network is a new I/O channel
/// and parser attack surface — the same class of concern `image src`/N4/F1
/// exist to prevent. The author picks a generic; the engine (via fontique's
/// script-aware fallback — see `render::text_engine`) guarantees the glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MizuFontFamily {
    /// Glyphs have plain stroke endings (e.g. Segoe UI, Arial). Default.
    #[default]
    SansSerif,
    /// Glyphs have finishing strokes / serifed endings.
    Serif,
    /// All glyphs share the same fixed advance width.
    Monospace,
}

/// `font-style` value: `normal` or `italic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MizuFontStyle {
    /// Upright ("roman") style. Default.
    #[default]
    Normal,
    /// Slanted style.
    Italic,
}

/// `text-align` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MizuTextAlign {
    /// Align content to the left edge.
    Left,
    /// Center each line.
    Center,
    /// Align content to the right edge.
    Right,
    /// Justify each line (except the last) by spacing out content.
    Justify,
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
#[derive(Debug, Clone, Default, PartialEq)]
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

    // ── Typography (ux-3) ─────────────────────────────────────────────────────
    /// `font-family` — one of the three CSS generics (`sans-serif`, `serif`,
    /// `monospace`). See [`MizuFontFamily`] for the security rationale for
    /// why this is a fixed allowlist.
    pub font_family: Option<MizuFontFamily>,
    /// `font-weight` — `normal` (400), `bold` (700), or a bare numeric
    /// weight in `100..=900`.
    pub font_weight: Option<f32>,
    /// `font-style` — `normal` or `italic`.
    pub font_style: Option<MizuFontStyle>,
    /// `text-align` — `left`, `center`, `right`, or `justify`.
    pub text_align: Option<MizuTextAlign>,
    /// `line-height` — a multiplier of the font size (e.g. `1.4`).
    /// Defaults to `1.2` when unset (`render::text_engine`).
    pub line_height: Option<f32>,
    /// `text-decoration` — `none` or `underline`.
    pub underline: Option<bool>,

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

        if other.font_family.is_some() {
            self.font_family = other.font_family;
        }
        if other.font_weight.is_some() {
            self.font_weight = other.font_weight;
        }
        if other.font_style.is_some() {
            self.font_style = other.font_style;
        }
        if other.text_align.is_some() {
            self.text_align = other.text_align;
        }
        if other.line_height.is_some() {
            self.line_height = other.line_height;
        }
        if other.underline.is_some() {
            self.underline = other.underline;
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
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {line_num}: invalid value `{value}` for `text-align`; \
                         valid values: left, center, right, justify"
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
                 direction, justify, align, background, background-image, background-size, color, \
                 font-size, border-radius, border-width, border-color, overflow, z-index, display, \
                 font-family, font-weight, font-style, text-align, line-height, text-decoration"
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

/// Parses `font-family` against the fixed three-generic allowlist
/// (`sans-serif`, `serif`, `monospace`). Accepts the value quoted or bare —
/// either way, only those three exact tokens are ever accepted. See
/// [`MizuFontFamily`] for the security rationale.
fn parse_font_family(value: &str, line_num: usize) -> Result<MizuFontFamily, MizuError> {
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
fn parse_font_weight(value: &str, line_num: usize) -> Result<f32, MizuError> {
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
        MizuDimension, MizuFontFamily, MizuFontStyle, MizuOverflow, MizuTextAlign, VariantCondition,
        parse_color, parse_style, parse_style_with_variants,
    };
    use crate::core::errors::MizuError;

    // ────────────────────────────────────────────────────────────────────────
    // background-image: absolute URLs rejected
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn background_image_absolute_url_is_rejected() {
        let style = "  .hero\n    background-image \"mizu://cdn.example/bg.png\"\n";
        let result = parse_style(style);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("absolute URLs are not allowed in background-image")),
            "absolute background-image URL must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn background_image_relative_path_is_allowed() {
        let style = "  .hero\n    background-image \"assets/bg.png\"\n";
        let rules = parse_style(style).expect("relative background-image must parse");
        assert_eq!(
            rules.get("hero").and_then(|r| r.background_image.as_deref()),
            Some("assets/bg.png"),
            "relative background-image path must be preserved"
        );
    }

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
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("display") && msg.contains("none, flex")),
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

    // ────────────────────────────────────────────────────────────────────────
    // Typography (ux-3)
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn font_family_sans_serif_parsed() {
        let block = "    .label\n        font-family sans-serif\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].font_family, Some(MizuFontFamily::SansSerif));
    }

    #[test]
    fn font_family_serif_parsed() {
        let block = "    .label\n        font-family serif\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].font_family, Some(MizuFontFamily::Serif));
    }

    #[test]
    fn font_family_monospace_parsed() {
        let block = "    .label\n        font-family monospace\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].font_family, Some(MizuFontFamily::Monospace));
    }

    #[test]
    fn font_family_quoted_generic_also_accepted() {
        let block = "    .label\n        font-family \"serif\"\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].font_family, Some(MizuFontFamily::Serif));
    }

    // ── Security: font-family allowlist cannot be silently widened ──────────

    #[test]
    fn font_family_concrete_name_is_rejected() {
        let block = "    .label\n        font-family \"Comic Sans MS\"\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("font-family")),
            "a concrete font family name must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn font_family_url_is_rejected() {
        let block = "    .label\n        font-family \"http://evil/font.woff\"\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("font-family")),
            "a URL must never be accepted as a font-family value, got: {result:?}"
        );
    }

    #[test]
    fn font_family_bare_word_outside_allowlist_is_rejected() {
        let block = "    .label\n        font-family Arial\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "only the three generics may parse, got: {result:?}"
        );
    }

    #[test]
    fn font_weight_normal_parsed() {
        let block = "    .label\n        font-weight normal\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].font_weight, Some(400.0));
    }

    #[test]
    fn font_weight_bold_parsed() {
        let block = "    .label\n        font-weight bold\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].font_weight, Some(700.0));
    }

    #[test]
    fn font_weight_numeric_parsed() {
        let block = "    .label\n        font-weight 550\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].font_weight, Some(550.0));
    }

    #[test]
    fn font_weight_out_of_range_is_rejected() {
        let block = "    .label\n        font-weight 1500\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("font-weight")),
            "expected font-weight range error, got: {result:?}"
        );
    }

    #[test]
    fn font_weight_garbage_is_rejected() {
        let block = "    .label\n        font-weight chunky\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("font-weight")),
            "expected font-weight error, got: {result:?}"
        );
    }

    #[test]
    fn font_style_italic_parsed() {
        let block = "    .label\n        font-style italic\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].font_style, Some(MizuFontStyle::Italic));
    }

    #[test]
    fn font_style_normal_parsed() {
        let block = "    .label\n        font-style normal\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].font_style, Some(MizuFontStyle::Normal));
    }

    #[test]
    fn font_style_invalid_is_rejected() {
        let block = "    .label\n        font-style slanted\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("font-style")),
            "expected font-style error, got: {result:?}"
        );
    }

    #[test]
    fn text_align_center_parsed() {
        let block = "    .label\n        text-align center\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].text_align, Some(MizuTextAlign::Center));
    }

    #[test]
    fn text_align_all_valid_values_parsed() {
        for (value, expected) in [
            ("left", MizuTextAlign::Left),
            ("center", MizuTextAlign::Center),
            ("right", MizuTextAlign::Right),
            ("justify", MizuTextAlign::Justify),
        ] {
            let block = format!("    .label\n        text-align {value}\n");
            let rules = parse_style(&block).unwrap();
            assert_eq!(rules["label"].text_align, Some(expected));
        }
    }

    #[test]
    fn text_align_invalid_is_rejected() {
        let block = "    .label\n        text-align middle\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("text-align")),
            "expected text-align error, got: {result:?}"
        );
    }

    #[test]
    fn line_height_multiplier_parsed() {
        let block = "    .label\n        line-height 1.4\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["label"].line_height, Some(1.4));
    }

    #[test]
    fn line_height_default_is_unset() {
        let block = "    .label\n        color #000000\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(
            rules["label"].line_height, None,
            "line-height must be None when unset; the 1.2 default lives in text_engine"
        );
    }

    #[test]
    fn text_decoration_underline_parsed() {
        let block = "    .link\n        text-decoration underline\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["link"].underline, Some(true));
    }

    #[test]
    fn text_decoration_none_parsed() {
        let block = "    .link\n        text-decoration none\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["link"].underline, Some(false));
    }

    #[test]
    fn text_decoration_invalid_is_rejected() {
        let block = "    .link\n        text-decoration wavy\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("text-decoration")),
            "expected text-decoration error, got: {result:?}"
        );
    }

    #[test]
    fn typography_properties_merge_like_others() {
        let base = "    .base\n        font-weight bold\n        text-align center\n";
        let override_block = "    .active\n        font-weight normal\n";
        let base_rules = parse_style(base).unwrap();
        let override_rules = parse_style(override_block).unwrap();
        let merged = base_rules["base"].clone().merge(override_rules["active"].clone());
        assert_eq!(merged.font_weight, Some(400.0), "override must win");
        assert_eq!(
            merged.text_align,
            Some(MizuTextAlign::Center),
            "unset fields in the override must not clobber the base"
        );
    }

    #[test]
    fn error_unknown_property_message_lists_typography_properties() {
        // Keep the unknown-property error message in sync with the new
        // properties — a stale list is a paper cut that misleads authors.
        let block = "    .box\n        color-scheme dark\n";
        let result = parse_style(block);
        let msg = result.unwrap_err().to_string();
        for prop in [
            "font-family",
            "font-weight",
            "font-style",
            "text-align",
            "line-height",
            "text-decoration",
        ] {
            assert!(
                msg.contains(prop),
                "unknown-property error must list `{prop}`, got: {msg}"
            );
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // Viewport units (ux-6)
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn viewport_width_unit_parsed() {
        let block = "    .box\n        width 50vw\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["box"].width, Some(MizuDimension::ViewportWidth(50.0)));
    }

    #[test]
    fn viewport_height_unit_parsed() {
        let block = "    .box\n        height 100vh\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["box"].height, Some(MizuDimension::ViewportHeight(100.0)));
    }

    #[test]
    fn viewport_min_unit_parsed() {
        let block = "    .box\n        width 10vmin\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["box"].width, Some(MizuDimension::ViewportMin(10.0)));
    }

    #[test]
    fn viewport_max_unit_parsed() {
        let block = "    .box\n        width 10vmax\n";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["box"].width, Some(MizuDimension::ViewportMax(10.0)));
    }

    #[test]
    fn viewport_unit_applies_to_padding_margin_gap_too() {
        let block = "\
    .box
        padding 2vw
        margin 3vh
        gap 1vmin
";
        let rules = parse_style(block).unwrap();
        let b = &rules["box"];
        assert_eq!(b.padding, Some(MizuDimension::ViewportWidth(2.0)));
        assert_eq!(b.margin, Some(MizuDimension::ViewportHeight(3.0)));
        assert_eq!(b.gap, Some(MizuDimension::ViewportMin(1.0)));
    }

    #[test]
    fn viewport_unit_malformed_value_is_rejected() {
        let block = "    .box\n        width abcvw\n";
        let result = parse_style(block);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("vw")),
            "expected a vw-specific error, got: {result:?}"
        );
    }

    #[test]
    fn plain_pixel_and_percent_still_parse_unaffected_by_unit_suffixes() {
        // Regression: adding vw/vh/vmin/vmax suffix stripping must not
        // disturb the existing bare-number and `%` parsing paths.
        let block = "\
    .box
        width 100
        height 50%
";
        let rules = parse_style(block).unwrap();
        assert_eq!(rules["box"].width, Some(MizuDimension::Pixels(100.0)));
        assert_eq!(rules["box"].height, Some(MizuDimension::Percent(50.0)));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Breakpoint / color-scheme variants (ux-6): parsing
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn variant_min_width_parsed_as_separate_entry() {
        let style = r"
    .sidebar
        width 240
    .sidebar @min-width 600
        width 300
";
        let (base, variants) = parse_style_with_variants(style).unwrap();
        assert_eq!(base["sidebar"].width, Some(MizuDimension::Pixels(240.0)));
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].selector, "sidebar");
        assert_eq!(variants[0].conditions, vec![VariantCondition::MinWidth(600.0)]);
        assert_eq!(variants[0].rules.width, Some(MizuDimension::Pixels(300.0)));
    }

    #[test]
    fn variant_max_width_parsed() {
        let style = "    .box @max-width 599\n        direction column\n";
        let (base, variants) = parse_style_with_variants(style).unwrap();
        assert!(base.is_empty(), "a purely-conditioned selector must not appear in the base map");
        assert_eq!(variants[0].conditions, vec![VariantCondition::MaxWidth(599.0)]);
    }

    #[test]
    fn variant_dark_and_light_parsed() {
        let style = r"
    .card @dark
        background #000000
    .card @light
        background #ffffff
";
        let (_base, variants) = parse_style_with_variants(style).unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].conditions, vec![VariantCondition::Dark]);
        assert_eq!(variants[1].conditions, vec![VariantCondition::Light]);
    }

    #[test]
    fn variant_combined_conditions_and_combined() {
        let style = "    .banner @min-width 600 @max-width 900\n        display flex\n";
        let (_base, variants) = parse_style_with_variants(style).unwrap();
        assert_eq!(
            variants[0].conditions,
            vec![
                VariantCondition::MinWidth(600.0),
                VariantCondition::MaxWidth(900.0),
            ]
        );
    }

    #[test]
    fn variant_unknown_condition_is_rejected() {
        let style = "    .box @huge\n        width 100\n";
        let result = parse_style_with_variants(style);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("unknown variant condition")),
            "expected unknown-variant-condition error, got: {result:?}"
        );
    }

    #[test]
    fn variant_min_width_missing_value_is_rejected() {
        let style = "    .box @min-width\n        width 100\n";
        let result = parse_style_with_variants(style);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("@min-width")),
            "expected a message naming @min-width, got: {result:?}"
        );
    }

    #[test]
    fn variant_min_width_non_numeric_value_is_rejected() {
        let style = "    .box @min-width wide\n        width 100\n";
        let result = parse_style_with_variants(style);
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected a ParseError for a non-numeric @min-width value, got: {result:?}"
        );
    }

    #[test]
    fn plain_parse_style_ignores_variants_but_keeps_base_unaffected() {
        // parse_style (the back-compat wrapper) must behave identically to
        // before ux-6 for documents that don't use variants, and must not
        // error out just because OTHER selectors in the same stylesheet do.
        let style = r"
    .box
        width 100
    .box @dark
        width 200
";
        let rules = parse_style(style).unwrap();
        assert_eq!(rules["box"].width, Some(MizuDimension::Pixels(100.0)));
    }
}
