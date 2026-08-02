//! Tests for the responsive module.

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
    assert_eq!(
        base["sidebar"].width,
        Some(crate::parser::MizuDimension::Pixels(240.0))
    );

    let narrow = env(500.0, ColorScheme::Dark);
    let resolved_narrow =
        base["sidebar"]
            .clone()
            .merge(resolve_matching_variants(&variants, &["sidebar"], &narrow));
    assert_eq!(
        resolved_narrow.width,
        Some(crate::parser::MizuDimension::Percent(100.0)),
        "below the max-width threshold, the variant must override width"
    );

    let wide = env(800.0, ColorScheme::Dark);
    let resolved_wide =
        base["sidebar"]
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
    flex-direction column
.panel @min-width 600
    flex-direction row
";
    let (base, variants) = parse_style_with_variants(style).unwrap();

    for (width, expected) in [
        (400.0, taffy::style::FlexDirection::Column),
        (600.0, taffy::style::FlexDirection::Row),
        (900.0, taffy::style::FlexDirection::Row),
        (300.0, taffy::style::FlexDirection::Column),
    ] {
        let e = env(width, ColorScheme::Dark);
        let resolved =
            base["panel"]
                .clone()
                .merge(resolve_matching_variants(&variants, &["panel"], &e));
        assert_eq!(
            resolved.flex_direction,
            Some(expected),
            "at width {width}, expected flex-direction {expected:?}"
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
    let resolved_dark =
        base["card"]
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
    let resolved_light =
        base["card"]
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
        let resolved =
            base["banner"]
                .clone()
                .merge(resolve_matching_variants(&variants, &["banner"], &e));
        let is_flex = resolved.display == Some(taffy::style::Display::Flex);
        assert_eq!(
            is_flex, expect_flex,
            "at width {width}, expected display:flex = {expect_flex}"
        );
    }
}
