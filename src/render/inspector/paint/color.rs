//! Panel tone resolution: [`Tones`] (the per-paint six-colour set derived
//! from the chrome palette), plus the `readable`/`faded` colour helpers.

use vello::peniko::Color;

use crate::render::inspector::model::Tone;
use crate::render::preferences::ChromePalette;

pub(super) struct Tones {
    pub(super) key: Color,
    pub(super) value: Color,
    pub(super) muted: Color,
    pub(super) accent: Color,
    pub(super) good: Color,
    pub(super) bad: Color,
}

/// Minimum contrast a tone must reach against the panel background.
const MIN_TEXT_CONTRAST: f64 = 4.5;

impl Tones {
    pub(super) fn new(palette: &ChromePalette) -> Self {
        Tones {
            key: palette.url_text,
            value: palette.tab_text,
            muted: palette.tab_text_inactive,
            // `url_border_focused` is a *ring* colour, chosen to stand out
            // against a border, and in the light palette it lands at 2.45:1
            // as text — well under AA. Darkening it toward the background's
            // opposite pole keeps the accent recognisably itself while making
            // it readable, which pure black (what `enforce_min_contrast`
            // would return) does not.
            accent: readable(palette.url_border_focused, palette.bar_bg),
            good: readable(palette.ok_dot, palette.bar_bg),
            bad: readable(palette.err_text, palette.bar_bg),
        }
    }

    pub(super) fn get(&self, tone: Tone) -> Color {
        match tone {
            Tone::Key => self.key,
            Tone::Value => self.value,
            Tone::Muted => self.muted,
            Tone::Accent => self.accent,
            Tone::Good => self.good,
            Tone::Bad => self.bad,
        }
    }
}

/// Mixes `fg` toward black or white — whichever contrasts more with `bg` —
/// by the smallest amount that reaches [`MIN_TEXT_CONTRAST`].
///
/// Contrast is monotonic in that mix factor (the mix runs from `fg` to the
/// pole that is furthest from `bg` in luminance), so a bisection finds the
/// least-altered colour that passes.  Returns `fg` untouched when it already
/// does, which is the case for every tone in the dark and high-contrast
/// palettes.
pub(super) fn readable(fg: Color, bg: Color) -> Color {
    use crate::render::preferences::contrast_ratio;
    if contrast_ratio(fg, bg) >= MIN_TEXT_CONTRAST {
        return fg;
    }
    let pole = if contrast_ratio(Color::rgba8(0, 0, 0, 255), bg)
        >= contrast_ratio(Color::rgba8(255, 255, 255, 255), bg)
    {
        0u8
    } else {
        255u8
    };
    let mix = |t: f32| {
        let blend = |c: u8| {
            (c as f32 + (pole as f32 - c as f32) * t)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Color {
            r: blend(fg.r),
            g: blend(fg.g),
            b: blend(fg.b),
            a: fg.a,
        }
    };
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..12 {
        let mid = (lo + hi) / 2.0;
        if contrast_ratio(mix(mid), bg) >= MIN_TEXT_CONTRAST {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    mix(hi)
}

/// `color` at a reduced alpha, for hairlines and guides.
pub(super) fn faded(color: Color, alpha: u8) -> Color {
    Color { a: alpha, ..color }
}
