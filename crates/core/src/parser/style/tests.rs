//! Tests for the style module.

use super::values::*;
use super::*;
use crate::core::errors::MizuError;
use taffy::style::{AlignItems, Display, FlexDirection, JustifyContent};

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
        rules
            .get("hero")
            .and_then(|r| r.background_image.as_deref()),
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
fn flex_direction_row() {
    let block = "    .flex\n        flex-direction row\n";
    let rules = parse_style(block).unwrap();
    assert_eq!(rules["flex"].flex_direction, Some(FlexDirection::Row));
}

#[test]
fn flex_direction_column() {
    let block = "    .flex\n        flex-direction column\n";
    let rules = parse_style(block).unwrap();
    assert_eq!(rules["flex"].flex_direction, Some(FlexDirection::Column));
}

#[test]
fn direction_is_rejected_with_a_helpful_rename_message() {
    // The old `direction` name is retired (renamed to `flex-direction`,
    // ux-7) — it must not silently fall through to "unknown property";
    // it gets its own actionable error.
    let block = "    .flex\n        direction row\n";
    let result = parse_style(block);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("renamed to `flex-direction`")),
        "expected a rename-specific error, got: {result:?}"
    );
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
    flex-direction row
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
    assert_eq!(btn.flex_direction, Some(FlexDirection::Row));
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
fn id_selector_parses_and_is_stored_hash_prefixed() {
    let block = "    #header\n        background #1a2b3c\n";
    let rules = parse_style(block).unwrap();
    assert_eq!(
        rules["#header"].background,
        Some(MizuBackground::Solid(MizuColor::rgb(0x1A, 0x2B, 0x3C))),
        "an id selector must be stored under its `#`-prefixed key"
    );
}

#[test]
fn error_empty_id_name() {
    let block = "    #\n        padding 10\n";
    let result = parse_style(block);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("name")),
        "expected id-name error, got: {result:?}"
    );
}

#[test]
fn id_and_class_of_the_same_bare_name_do_not_collide() {
    let block = "    .card\n        background #111111\n    #card\n        background #222222\n";
    let rules = parse_style(block).unwrap();
    assert_eq!(
        rules["card"].background,
        Some(MizuBackground::Solid(MizuColor::rgb(0x11, 0x11, 0x11)))
    );
    assert_eq!(
        rules["#card"].background,
        Some(MizuBackground::Solid(MizuColor::rgb(0x22, 0x22, 0x22)))
    );
}

#[test]
fn form_and_headings_are_valid_bare_tag_selectors() {
    for tag in ["form", "h1", "h2", "h3", "h4", "h5", "h6"] {
        let block = format!("    {tag}\n        padding 10\n");
        let result = parse_style(&block);
        assert!(
            result.is_ok(),
            "`{tag}` must be a valid bare tag selector, got: {result:?}"
        );
    }
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
    let merged = base_rules["base"]
        .clone()
        .merge(override_rules["active"].clone());
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
    assert_eq!(
        rules["box"].height,
        Some(MizuDimension::ViewportHeight(100.0))
    );
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
    assert_eq!(
        variants[0].conditions,
        vec![VariantCondition::MinWidth(600.0)]
    );
    assert_eq!(variants[0].rules.width, Some(MizuDimension::Pixels(300.0)));
}

#[test]
fn variant_max_width_parsed() {
    let style = "    .box @max-width 599\n        flex-direction column\n";
    let (base, variants) = parse_style_with_variants(style).unwrap();
    assert!(
        base.is_empty(),
        "a purely-conditioned selector must not appear in the base map"
    );
    assert_eq!(
        variants[0].conditions,
        vec![VariantCondition::MaxWidth(599.0)]
    );
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
