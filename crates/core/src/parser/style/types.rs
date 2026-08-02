//! Style data types: `MizuColor`, `MizuBackground`, `MizuDimension`,
//! `VariantCondition`/`StyleVariant`, and `StyleRules` (with its merge and
//! variant-application logic).

use taffy::style::{AlignItems, Display, FlexDirection, JustifyContent};

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
#[derive(Debug, Clone, Copy, PartialEq)]
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
    /// Align to the start of the inline direction — the left edge under
    /// LTR, the right edge under RTL (ux-7; see `docs/design/bidi.md`).
    /// Resolved to `Left`/`Right` at paint time by the node's resolved
    /// `dir`; `render::bidi::resolve_direction`.
    Start,
    /// The mirror of [`Self::Start`] — the right edge under LTR, the left
    /// edge under RTL.
    End,
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
    /// `margin-inline-start` (ux-7) — the left edge under LTR, the right
    /// edge under RTL. Overrides just that one side of the uniform
    /// `margin` value, if both are set. See `docs/design/bidi.md`.
    pub margin_inline_start: Option<MizuDimension>,
    /// `margin-inline-end` — the mirror of [`Self::margin_inline_start`].
    pub margin_inline_end: Option<MizuDimension>,
    /// `padding-inline-start` (ux-7) — same resolution rule as
    /// [`Self::margin_inline_start`], for `padding`.
    pub padding_inline_start: Option<MizuDimension>,
    /// `padding-inline-end` — the mirror of [`Self::padding_inline_start`].
    pub padding_inline_end: Option<MizuDimension>,

    // ── Taffy flex properties ─────────────────────────────────────────────────
    /// `flex-direction` (renamed from `direction` in ux-7 — the old name
    /// collided with CSS's unrelated `direction: ltr|rtl` property; see
    /// `docs/design/bidi.md` §3). Maps to [`taffy::style::FlexDirection`].
    /// Valid author-facing values: `row`, `column`. Under a resolved RTL
    /// direction, `row` is internally translated to `FlexDirection::RowReverse`
    /// (`render::layout_bridge::translate_style`) — the author-facing
    /// vocabulary is unaffected.
    pub flex_direction: Option<FlexDirection>,
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
    /// Merges a borrowed set of rules into this one. `other` rules take
    /// precedence (e.g. class styles overriding tag styles).
    ///
    /// Prefer this over [`Self::merge`] when the caller holds a reference to
    /// the incoming rules (e.g. a `HashMap` lookup) — it clones only the
    /// individual fields that actually win, rather than the entire struct.
    pub fn merge_from(&mut self, other: &Self) {
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
        if other.margin_inline_start.is_some() {
            self.margin_inline_start = other.margin_inline_start;
        }
        if other.margin_inline_end.is_some() {
            self.margin_inline_end = other.margin_inline_end;
        }
        if other.padding_inline_start.is_some() {
            self.padding_inline_start = other.padding_inline_start;
        }
        if other.padding_inline_end.is_some() {
            self.padding_inline_end = other.padding_inline_end;
        }
        if other.flex_direction.is_some() {
            self.flex_direction = other.flex_direction;
        }
        if other.justify.is_some() {
            self.justify = other.justify;
        }
        if other.align.is_some() {
            self.align = other.align;
        }
        if other.background.is_some() {
            self.background = other.background.clone();
        }
        if other.background_image.is_some() {
            self.background_image = other.background_image.clone();
        }
        if other.background_size.is_some() {
            self.background_size = other.background_size;
        }
        if other.color.is_some() {
            self.color = other.color.clone();
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
            self.border_color = other.border_color.clone();
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
        if other.overflow != MizuOverflow::Visible {
            self.overflow = other.overflow;
        }
        if other.z_index != 0 {
            self.z_index = other.z_index;
        }
        if other.display.is_some() {
            self.display = other.display;
        }
    }

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
        if other.margin_inline_start.is_some() {
            self.margin_inline_start = other.margin_inline_start;
        }
        if other.margin_inline_end.is_some() {
            self.margin_inline_end = other.margin_inline_end;
        }
        if other.padding_inline_start.is_some() {
            self.padding_inline_start = other.padding_inline_start;
        }
        if other.padding_inline_end.is_some() {
            self.padding_inline_end = other.padding_inline_end;
        }

        if other.flex_direction.is_some() {
            self.flex_direction = other.flex_direction;
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
