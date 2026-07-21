//! User preference detection and the theme-aware chrome palette (ux-5).
//!
//! ## Security posture
//!
//! User preferences (color scheme, high-contrast, reduced-motion) are an
//! input to **rendering only**. There is no logic-callable primitive that
//! returns any of them — `tests::no_logic_primitive_exposes_preferences`
//! below proves this the same way `tests/storage_rehydration_taint.rs`
//! proves `read_local` doesn't exist: by constructing the call as an `Expr`
//! directly (bypassing the parser) and confirming the evaluator has no such
//! function. This preserves **S1** (nothing the device knows about the user
//! flows back into the document) and **F1** (no new taint source) — a
//! preference influences paint, never logic.
//!
//! ## What's implemented vs. deferred
//!
//! * **Light/dark** — fully wired: `winit::window::Theme` is read at
//!   startup and on `WindowEvent::ThemeChanged`
//!   (`render::window::event_loop`), and the chrome palette follows it live.
//! * **High-contrast** — plumbing only. Safely detecting Windows' system
//!   high-contrast setting needs a raw `SystemParametersInfo` FFI call,
//!   which is `unsafe` in every binding (including the `windows` crate),
//!   and this crate is `#![forbid(unsafe_code)]` crate-wide. Detection
//!   therefore fails open to "no preference" on every platform today —
//!   [`UserPreferences::high_contrast`] is always `false` until a caller
//!   sets it. The palette-selection logic
//!   ([`ChromePalette::for_preferences`]) and the AA-contrast enforcement
//!   ([`enforce_min_contrast`]) are implemented and tested against a
//!   manually-constructed `UserPreferences`, ready to wire up if/when a safe
//!   detection path exists (e.g. a narrow, separately-audited `unsafe`
//!   sub-crate — the same boundary `accesskit_windows` already draws
//!   outside this crate).
//! * **Reduced-motion** — plumbing only, for the same reason `high_contrast`
//!   is. Mizu has no transitions/animations today (verified: no
//!   `transition` style property exists in `parser::style`), so this is a
//!   forward-looking no-op — the field exists so a future animation feature
//!   has somewhere to read it from without a second round of plumbing.
//! * **Document-side `@dark`/`@light` style-block variants** and **wiring
//!   [`enforce_min_contrast`] into the document text-paint path** are
//!   explicit follow-ups, not implemented here. Both are separable pieces of
//!   work bigger than this commit (a new selector-prefix grammar, and a
//!   render-hot-path change respectively) — chrome theming ships first, per
//!   the ux-5 prompt's own scope allowance.

#![forbid(unsafe_code)]

use vello::peniko::Color;

/// The two color schemes the chrome (and, in future, a document via
/// `@dark`/`@light` variants) can render in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    /// Light background, dark text.
    Light,
    /// Dark background, light text (Mizu's original, and still the
    /// default when no OS preference can be detected).
    Dark,
}

impl From<winit::window::Theme> for ColorScheme {
    fn from(theme: winit::window::Theme) -> Self {
        match theme {
            winit::window::Theme::Light => Self::Light,
            winit::window::Theme::Dark => Self::Dark,
        }
    }
}

/// Detected (or, where noted, assumed-absent) OS-level appearance and
/// accessibility preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserPreferences {
    /// Light or dark, from `winit::window::Theme` — real detection.
    pub color_scheme: ColorScheme,
    /// OS high-contrast mode. Always `false` today — see the module doc's
    /// "What's implemented vs. deferred" note.
    pub high_contrast: bool,
    /// OS reduced-motion preference. Always `false` today, and a no-op even
    /// if set, since Mizu has no animations yet.
    pub reduced_motion: bool,
}

impl Default for UserPreferences {
    /// Dark scheme, no accessibility preferences detected — matches Mizu's
    /// behavior before ux-5 when nothing else is known yet (e.g. before the
    /// first `Window::theme()` read completes).
    fn default() -> Self {
        Self {
            color_scheme: ColorScheme::Dark,
            high_contrast: false,
            reduced_motion: false,
        }
    }
}

/// The full set of chrome colors, resolved once per [`UserPreferences`]
/// change (startup, `ThemeChanged`) rather than branching at every paint
/// call site.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromePalette {
    /// Chrome bar background.
    pub bar_bg: Color,
    /// Back/Reload/Forward button background.
    pub btn_bg: Color,
    /// Button background when disabled (empty back/forward stack).
    pub btn_bg_disabled: Color,
    /// Button glyph color.
    pub btn_text: Color,
    /// Button glyph color when disabled.
    pub btn_text_disabled: Color,
    /// URL bar background.
    pub url_bg: Color,
    /// URL bar border, unfocused.
    pub url_border_idle: Color,
    /// URL bar border, focused.
    pub url_border_focused: Color,
    /// URL bar text.
    pub url_text: Color,
    /// URL bar text-entry cursor.
    pub cursor: Color,
    /// URL bar text selection highlight.
    pub select: Color,
    /// The "loaded OK" status dot.
    pub ok_dot: Color,
}

/// Mizu's original palette — unchanged from before ux-5.
const DARK: ChromePalette = ChromePalette {
    bar_bg: Color::rgba8(43, 43, 43, 255),
    btn_bg: Color::rgba8(60, 60, 60, 255),
    btn_bg_disabled: Color::rgba8(60, 60, 60, 120),
    btn_text: Color::rgba8(220, 220, 220, 255),
    btn_text_disabled: Color::rgba8(220, 220, 220, 100),
    url_bg: Color::rgba8(30, 30, 30, 255),
    url_border_idle: Color::rgba8(80, 80, 80, 255),
    url_border_focused: crate::render::FOCUS_RING_COLOR,
    url_text: Color::rgba8(204, 204, 204, 255),
    cursor: Color::rgba8(255, 255, 255, 255),
    select: Color::rgba8(74, 144, 217, 120),
    ok_dot: Color::rgba8(76, 175, 80, 255),
};

/// Light counterpart. Contrast ratios are computed and asserted (≥ 4.5:1,
/// WCAG AA for normal text) in `tests::chrome_palette_meets_wcag_aa_contrast`
/// — not hand-typed here, so this can never silently drift out of
/// compliance without a failing test.
const LIGHT: ChromePalette = ChromePalette {
    bar_bg: Color::rgba8(238, 238, 238, 255),
    btn_bg: Color::rgba8(222, 222, 222, 255),
    btn_bg_disabled: Color::rgba8(222, 222, 222, 130),
    btn_text: Color::rgba8(32, 32, 32, 255),
    btn_text_disabled: Color::rgba8(32, 32, 32, 110),
    url_bg: Color::rgba8(255, 255, 255, 255),
    url_border_idle: Color::rgba8(170, 170, 170, 255),
    url_border_focused: crate::render::FOCUS_RING_COLOR,
    url_text: Color::rgba8(25, 25, 25, 255),
    cursor: Color::rgba8(20, 20, 20, 255),
    select: Color::rgba8(74, 144, 217, 90),
    ok_dot: Color::rgba8(46, 125, 50, 255),
};

/// A maximum-contrast palette forced when [`UserPreferences::high_contrast`]
/// is set — pure black/white, no mid-tones, so it wins over either base
/// palette regardless of scheme.
const HIGH_CONTRAST: ChromePalette = ChromePalette {
    bar_bg: Color::rgba8(0, 0, 0, 255),
    btn_bg: Color::rgba8(0, 0, 0, 255),
    btn_bg_disabled: Color::rgba8(0, 0, 0, 255),
    btn_text: Color::rgba8(255, 255, 255, 255),
    btn_text_disabled: Color::rgba8(140, 140, 140, 255),
    url_bg: Color::rgba8(0, 0, 0, 255),
    url_border_idle: Color::rgba8(255, 255, 255, 255),
    url_border_focused: Color::rgba8(255, 255, 0, 255),
    url_text: Color::rgba8(255, 255, 255, 255),
    cursor: Color::rgba8(255, 255, 0, 255),
    select: Color::rgba8(255, 255, 0, 140),
    ok_dot: Color::rgba8(0, 255, 0, 255),
};

impl ChromePalette {
    /// Selects the chrome palette for the given preferences: high-contrast
    /// wins outright (it must win over either base scheme — that's the
    /// entire point of the setting), otherwise light/dark per
    /// [`UserPreferences::color_scheme`].
    pub fn for_preferences(prefs: &UserPreferences) -> Self {
        if prefs.high_contrast {
            return HIGH_CONTRAST;
        }
        match prefs.color_scheme {
            ColorScheme::Dark => DARK,
            ColorScheme::Light => LIGHT,
        }
    }
}

/// WCAG relative luminance of an sRGB color (ignores alpha).
///
/// <https://www.w3.org/TR/WCAG21/#dfn-relative-luminance>
fn relative_luminance(c: Color) -> f64 {
    fn channel(v: u8) -> f64 {
        let v = f64::from(v) / 255.0;
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
}

/// WCAG contrast ratio between two colors, in `[1.0, 21.0]`.
///
/// <https://www.w3.org/TR/WCAG21/#dfn-contrast-ratio>
pub fn contrast_ratio(a: Color, b: Color) -> f64 {
    let (l1, l2) = (relative_luminance(a), relative_luminance(b));
    let (lighter, darker) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

/// If `fg` already contrasts against `bg` at or above `min_ratio`, returns it
/// unchanged. Otherwise returns pure black or pure white — whichever
/// contrasts more against `bg` — as a fail-safe replacement.
///
/// This is the "won't let a document drop below AA" enforcement the
/// high-contrast mode requires; it is implemented and tested here but **not
/// yet wired into the document text-paint path** (see the module doc's
/// deferred-scope note) — `high_contrast` never becomes `true` from live
/// detection today, so there is nothing yet that would call this outside a
/// test.
pub fn enforce_min_contrast(fg: Color, bg: Color, min_ratio: f64) -> Color {
    if contrast_ratio(fg, bg) >= min_ratio {
        return fg;
    }
    let black = Color::rgba8(0, 0, 0, fg.a);
    let white = Color::rgba8(255, 255, 255, fg.a);
    if contrast_ratio(black, bg) >= contrast_ratio(white, bg) {
        black
    } else {
        white
    }
}

#[cfg(test)]
mod tests {
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
        let palettes = [("dark", DARK), ("light", LIGHT), ("high-contrast", HIGH_CONTRAST)];
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
        assert!((ratio - 1.0).abs() < 0.01, "identical colors must have ratio 1.0, got {ratio}");
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
        assert!(contrast_ratio(fg, bg) < 4.5, "test setup must start non-compliant");
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
        use crate::core::types::{StateMachine, StringInterner};
        use crate::parser::logic::Expr;

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
            let call = Expr::FunctionCall {
                name: sym,
                args: vec![],
            };
            let mut machine = StateMachine::new();
            let no_functions = Default::default();
            let result = machine.evaluate(&call, 0, &no_functions, &interner);
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
}
