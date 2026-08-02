//! Tests for the preferences module.

use super::*;

#[test]
fn for_preferences_selects_dark_for_dark_scheme() {
    let prefs = UserPreferences {
        color_scheme: ColorScheme::Dark,
        high_contrast: false,
        reduced_motion: false,
    };
    assert_eq!(ChromePalette::for_preferences(&prefs), DARK);
}

#[test]
fn for_preferences_selects_light_for_light_scheme() {
    let prefs = UserPreferences {
        color_scheme: ColorScheme::Light,
        high_contrast: false,
        reduced_motion: false,
    };
    assert_eq!(ChromePalette::for_preferences(&prefs), LIGHT);
}

#[test]
fn high_contrast_overrides_either_scheme() {
    for color_scheme in [ColorScheme::Dark, ColorScheme::Light] {
        let prefs = UserPreferences {
            color_scheme,
            high_contrast: true,
            reduced_motion: false,
        };
        assert_eq!(
            ChromePalette::for_preferences(&prefs),
            HIGH_CONTRAST,
            "high_contrast must win over {color_scheme:?}"
        );
    }
}

#[test]
fn theme_conversion_matches_winit() {
    assert_eq!(
        ColorScheme::from(winit::window::Theme::Dark),
        ColorScheme::Dark
    );
    assert_eq!(
        ColorScheme::from(winit::window::Theme::Light),
        ColorScheme::Light
    );
}

/// The objective accessibility check: real computed ratios, not a vibe.
/// Every (text, background) pair used for actual chrome text must meet
/// WCAG AA for normal text (4.5:1), in the dark palette, the light
/// palette, and the forced high-contrast palette.
#[test]
fn chrome_palette_meets_wcag_aa_contrast() {
    const MIN_AA: f64 = 4.5;
    let palettes = [
        ("dark", DARK),
        ("light", LIGHT),
        ("high-contrast", HIGH_CONTRAST),
    ];
    for (name, p) in palettes {
        let btn_ratio = contrast_ratio(p.btn_text, p.btn_bg);
        assert!(
            btn_ratio >= MIN_AA,
            "{name}: button text/background contrast {btn_ratio:.2} is below AA ({MIN_AA})"
        );
        let url_ratio = contrast_ratio(p.url_text, p.url_bg);
        assert!(
            url_ratio >= MIN_AA,
            "{name}: URL bar text/background contrast {url_ratio:.2} is below AA ({MIN_AA})"
        );
        for (label, fg, bg) in [
            ("active tab title", p.tab_text, p.tab_active_bg),
            ("inactive tab title", p.tab_text_inactive, p.tab_inactive_bg),
            (
                "close glyph on active tab",
                p.tab_close_glyph,
                p.tab_active_bg,
            ),
            (
                "close glyph on inactive tab",
                p.tab_close_glyph,
                p.tab_inactive_bg,
            ),
            // The inspector paints its rows straight onto the bar
            // background, so its semantic colors are held to the same bar.
            ("ok text on panel", p.ok_dot, p.bar_bg),
            ("error text on panel", p.err_text, p.bar_bg),
            ("dim text on panel", p.tab_text_inactive, p.bar_bg),
            ("normal text on panel", p.tab_text, p.bar_bg),
        ] {
            let ratio = contrast_ratio(fg, bg);
            assert!(
                ratio >= MIN_AA,
                "{name}: {label} contrast {ratio:.2} is below AA ({MIN_AA})"
            );
        }
    }
}

#[test]
fn contrast_ratio_black_on_white_is_maximal() {
    let ratio = contrast_ratio(Color::rgba8(0, 0, 0, 255), Color::rgba8(255, 255, 255, 255));
    assert!(
        (ratio - 21.0).abs() < 0.01,
        "black-on-white must be the maximal WCAG ratio (21:1), got {ratio}"
    );
}

#[test]
fn contrast_ratio_identical_colors_is_one() {
    let c = Color::rgba8(128, 128, 128, 255);
    let ratio = contrast_ratio(c, c);
    assert!(
        (ratio - 1.0).abs() < 0.01,
        "identical colors must have ratio 1.0, got {ratio}"
    );
}

#[test]
fn enforce_min_contrast_leaves_compliant_colors_untouched() {
    let fg = Color::rgba8(255, 255, 255, 255);
    let bg = Color::rgba8(0, 0, 0, 255);
    assert_eq!(enforce_min_contrast(fg, bg, 4.5), fg);
}

#[test]
fn enforce_min_contrast_fixes_low_contrast_pair() {
    // Mid-gray on mid-gray: contrast ~1.0, must be replaced.
    let fg = Color::rgba8(140, 140, 140, 255);
    let bg = Color::rgba8(120, 120, 120, 255);
    assert!(
        contrast_ratio(fg, bg) < 4.5,
        "test setup must start non-compliant"
    );
    let fixed = enforce_min_contrast(fg, bg, 4.5);
    assert!(
        contrast_ratio(fixed, bg) >= 4.5,
        "enforce_min_contrast must produce a compliant color, got ratio {}",
        contrast_ratio(fixed, bg)
    );
}

// ── Security: no logic primitive ever exposes the preference ──────────

#[test]
fn no_logic_primitive_exposes_preferences() {
    // Mirrors `tests/storage_rehydration_taint.rs`'s proof that
    // `read_local` doesn't exist: construct the call as an `Expr`
    // directly (bypassing the parser, so this isn't merely "no syntax
    // for it" but "the evaluator has no such capability"), for every
    // plausible name a document-readable color-scheme primitive might
    // have used, and confirm each fails as an undefined function.
    use crate::core::types::{Evaluator, StringInterner};
    use crate::parser::logic::{Expr, ExprArena};

    for candidate in [
        "get_color_scheme",
        "prefers_dark",
        "prefers_color_scheme",
        "color_scheme",
        "is_dark_mode",
        "high_contrast",
        "prefers_reduced_motion",
    ] {
        let mut interner = StringInterner::new();
        let sym = interner.get_or_intern(candidate);
        let mut arena = ExprArena::new();
        let (args_start, args_len) = arena.push_args(&[]).unwrap();
        let call = Expr::FunctionCall {
            name: sym,
            args_start,
            args_len,
        };
        let mut machine = Evaluator::new(crate::core::config::CONFIG.max_instructions);
        let no_functions = Default::default();
        let interner = interner.freeze();
        let result = machine.evaluate(&call, 0, &no_functions, &interner, &arena);
        assert!(
            result.is_err(),
            "`{candidate}` must not resolve to any evaluator function"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("undefined function"),
            "expected an undefined-function error for `{candidate}`, got: {msg}"
        );
    }
}
