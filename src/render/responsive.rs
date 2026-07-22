//! Responsive layout support (ux-6): viewport units and window-width /
//! color-scheme variant conditions.
//!
//! See `docs/design/responsive.md` for the full design memo (approved
//! Phase 1) this module implements. Summary:
//!
//! * **Viewport units** (`vw`/`vh`/`vmin`/`vmax`, parsed into
//!   [`crate::parser::style::MizuDimension`]) resolve against the document's
//!   content viewport — the window size, with `vh` excluding the chrome
//!   bar's height (`CHROME_HEIGHT`) since that space is never available to
//!   the document.
//! * **Breakpoints** (`@min-width N` / `@max-width N`) and the ux-5
//!   document-side color-scheme variant (`@dark` / `@light`) share one
//!   [`VariantCondition`] grammar and one resolution function
//!   ([`resolve_matching_variants`]), per the memo's explicit goal of not
//!   forking three parallel mechanisms across ux-5/ux-6/ux-7.
//!
//! ## Security / L1 posture
//!
//! Both are pure render-time layout/style math: no capability, no I/O, no
//! taint, and no logic-callable primitive exposing window size or the
//! resolved variant (mirrors ux-5's posture for OS preferences). Variant
//! resolution only changes *which* [`StyleRules`] a selector resolves to —
//! it never creates DOM nodes, so `MAX_SYNTHETIC_LAYOUT_NODES` (invariant
//! L1, `render::layout_bridge`) is structurally unaffected: see
//! `layout_bridge::tests::breakpoint_toggle_does_not_change_node_count` for
//! the regression pin. Resolution is `O(number of variants in the
//! stylesheet)`, independent of document/node count.
//!
//! Window width is read once per layout pass as a plain input, never as a
//! layout *output* fed back into the same pass — see the memo's rejection
//! of a layout-level conditional (option 2c) for why that distinction
//! matters (no re-entrancy).

#![forbid(unsafe_code)]

use crate::parser::style::{MizuDimension, StyleRules, StyleVariant, VariantCondition};
use crate::render::preferences::ColorScheme;

/// The document's content viewport in logical pixels — the window size,
/// minus whatever space the chrome bar owns (`height` already excludes
/// `CHROME_HEIGHT`; see the module doc).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportSize {
    /// Content viewport width (`vw` basis).
    pub width: f32,
    /// Content viewport height, excluding the chrome bar (`vh` basis).
    pub height: f32,
}

/// Snapshot of the environment values a [`VariantCondition`] can be gated
/// on, resolved once per layout pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderEnvironment {
    /// Current content viewport size.
    pub viewport: ViewportSize,
    /// Current OS/detected color scheme (ux-5).
    pub color_scheme: ColorScheme,
}

/// A [`MizuDimension`] with viewport units already resolved to an absolute
/// pixel value — ready for Taffy, which only needs to further resolve
/// [`Self::Percent`] against the parent container at its own layout time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResolvedDimension {
    /// An absolute pixel value (already resolved, if it started as a
    /// viewport unit).
    Pixels(f32),
    /// A percentage of the parent container — resolved later, by Taffy.
    Percent(f32),
}

/// Resolves a [`MizuDimension`] against `viewport`. Pixels and percent pass
/// through unchanged; `vw`/`vh`/`vmin`/`vmax` become absolute pixels.
pub fn resolve_dimension(dim: &MizuDimension, viewport: ViewportSize) -> ResolvedDimension {
    match dim {
        MizuDimension::Pixels(px) => ResolvedDimension::Pixels(*px),
        MizuDimension::Percent(pct) => ResolvedDimension::Percent(*pct),
        MizuDimension::ViewportWidth(pct) => {
            ResolvedDimension::Pixels(pct / 100.0 * viewport.width)
        }
        MizuDimension::ViewportHeight(pct) => {
            ResolvedDimension::Pixels(pct / 100.0 * viewport.height)
        }
        MizuDimension::ViewportMin(pct) => {
            ResolvedDimension::Pixels(pct / 100.0 * viewport.width.min(viewport.height))
        }
        MizuDimension::ViewportMax(pct) => {
            ResolvedDimension::Pixels(pct / 100.0 * viewport.width.max(viewport.height))
        }
    }
}

/// Whether a single [`VariantCondition`] currently holds against `env`.
fn condition_matches(cond: &VariantCondition, env: &RenderEnvironment) -> bool {
    match cond {
        VariantCondition::MinWidth(w) => env.viewport.width >= *w,
        VariantCondition::MaxWidth(w) => env.viewport.width <= *w,
        VariantCondition::Dark => env.color_scheme == ColorScheme::Dark,
        VariantCondition::Light => env.color_scheme == ColorScheme::Light,
    }
}

/// Merges every variant in `variants` whose selector is one of `selectors`
/// and whose conditions **all** currently hold (AND), in source
/// (declaration) order — later-declared variants win ties, matching every
/// other "declaration order" rule already in the grammar.
///
/// `O(variants.len())`: independent of document/node size, so this cannot
/// become a budget concern regardless of how large the DOM is (see the
/// module doc's L1 note).
pub fn resolve_matching_variants(
    variants: &[StyleVariant],
    selectors: &[&str],
    env: &RenderEnvironment,
) -> StyleRules {
    let mut result = StyleRules::default();
    for variant in variants {
        if selectors.contains(&variant.selector.as_str())
            && variant.conditions.iter().all(|c| condition_matches(c, env))
        {
            result = result.merge(variant.rules.clone());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::style::parse_style_with_variants;

    fn env(width: f32, scheme: ColorScheme) -> RenderEnvironment {
        RenderEnvironment {
            viewport: ViewportSize {
                width,
                height: 800.0,
            },
            color_scheme: scheme,
        }
    }

    #[test]
    fn resolve_dimension_passes_through_pixels_and_percent() {
        let vp = ViewportSize {
            width: 1000.0,
            height: 500.0,
        };
        assert_eq!(
            resolve_dimension(&MizuDimension::Pixels(42.0), vp),
            ResolvedDimension::Pixels(42.0)
        );
        assert_eq!(
            resolve_dimension(&MizuDimension::Percent(50.0), vp),
            ResolvedDimension::Percent(50.0)
        );
    }

    #[test]
    fn resolve_dimension_computes_viewport_units() {
        let vp = ViewportSize {
            width: 1000.0,
            height: 500.0,
        };
        assert_eq!(
            resolve_dimension(&MizuDimension::ViewportWidth(50.0), vp),
            ResolvedDimension::Pixels(500.0)
        );
        assert_eq!(
            resolve_dimension(&MizuDimension::ViewportHeight(100.0), vp),
            ResolvedDimension::Pixels(500.0)
        );
        assert_eq!(
            resolve_dimension(&MizuDimension::ViewportMin(10.0), vp),
            ResolvedDimension::Pixels(50.0),
            "vmin must use the smaller of width/height (500)"
        );
        assert_eq!(
            resolve_dimension(&MizuDimension::ViewportMax(10.0), vp),
            ResolvedDimension::Pixels(100.0),
            "vmax must use the larger of width/height (1000)"
        );
    }

    #[test]
    fn breakpoint_variant_applies_below_threshold_only() {
        let style = r"
    .sidebar
        width 240
    .sidebar @max-width 599
        width 100%
";
        let (base, variants) = parse_style_with_variants(style).unwrap();
        assert_eq!(base["sidebar"].width, Some(crate::parser::MizuDimension::Pixels(240.0)));

        let narrow = env(500.0, ColorScheme::Dark);
        let resolved_narrow = base["sidebar"]
            .clone()
            .merge(resolve_matching_variants(&variants, &["sidebar"], &narrow));
        assert_eq!(
            resolved_narrow.width,
            Some(crate::parser::MizuDimension::Percent(100.0)),
            "below the max-width threshold, the variant must override width"
        );

        let wide = env(800.0, ColorScheme::Dark);
        let resolved_wide = base["sidebar"]
            .clone()
            .merge(resolve_matching_variants(&variants, &["sidebar"], &wide));
        assert_eq!(
            resolved_wide.width,
            Some(crate::parser::MizuDimension::Pixels(240.0)),
            "above the max-width threshold, the base rules must apply, untouched"
        );
    }

    #[test]
    fn resizing_across_the_threshold_flips_the_variant() {
        let style = r"
    .panel
        direction column
    .panel @min-width 600
        direction row
";
        let (base, variants) = parse_style_with_variants(style).unwrap();

        for (width, expected) in [
            (400.0, taffy::style::FlexDirection::Column),
            (600.0, taffy::style::FlexDirection::Row),
            (900.0, taffy::style::FlexDirection::Row),
            (300.0, taffy::style::FlexDirection::Column),
        ] {
            let e = env(width, ColorScheme::Dark);
            let resolved = base["panel"]
                .clone()
                .merge(resolve_matching_variants(&variants, &["panel"], &e));
            assert_eq!(
                resolved.direction,
                Some(expected),
                "at width {width}, expected direction {expected:?}"
            );
        }
    }

    #[test]
    fn dark_and_light_variants_do_not_leak_into_each_other() {
        let style = r"
    .card
        background #ffffff
    .card @dark
        background #000000
    .card @light
        background #eeeeee
";
        let (base, variants) = parse_style_with_variants(style).unwrap();

        let dark = env(1000.0, ColorScheme::Dark);
        let resolved_dark = base["card"]
            .clone()
            .merge(resolve_matching_variants(&variants, &["card"], &dark));
        assert_eq!(
            resolved_dark.background,
            Some(crate::parser::style::MizuBackground::Solid(
                crate::parser::MizuColor::rgb(0, 0, 0)
            )),
            "@dark must apply in dark scheme"
        );

        let light = env(1000.0, ColorScheme::Light);
        let resolved_light = base["card"]
            .clone()
            .merge(resolve_matching_variants(&variants, &["card"], &light));
        assert_eq!(
            resolved_light.background,
            Some(crate::parser::style::MizuBackground::Solid(
                crate::parser::MizuColor::rgb(0xEE, 0xEE, 0xEE)
            )),
            "@light must apply in light scheme, not @dark's value"
        );
    }

    #[test]
    fn combined_conditions_require_all_to_hold() {
        let style = r"
    .banner
        display none
    .banner @min-width 600 @max-width 900
        display flex
";
        let (base, variants) = parse_style_with_variants(style).unwrap();

        for (width, expect_flex) in [(500.0, false), (700.0, true), (901.0, false)] {
            let e = env(width, ColorScheme::Dark);
            let resolved = base["banner"]
                .clone()
                .merge(resolve_matching_variants(&variants, &["banner"], &e));
            let is_flex = resolved.display == Some(taffy::style::Display::Flex);
            assert_eq!(
                is_flex, expect_flex,
                "at width {width}, expected display:flex = {expect_flex}"
            );
        }
    }
}
