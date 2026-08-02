//! Hit testing: resolves a logical `(x, y)` point to a `ChromeHitZone`.

use super::*;

// ── Hit testing ───────────────────────────────────────────────────────────────

/// Returns the [`ChromeHitZone`] for a logical (x, y) coordinate.
/// Returns [`ChromeHitZone::Background`] if the point is outside the chrome area
/// (y ≥ CHROME_HEIGHT) or in an unoccupied region.
pub fn chrome_hit_zone(x: f32, y: f32, layout: &ChromeLayout) -> ChromeHitZone {
    if layout.dropdown_count > 0 {
        let item_h = 24.0;
        let padding = 4.0;
        let dropdown_h = (layout.dropdown_count as f32) * item_h + padding * 2.0;

        let url_bar_right = (layout.window_width - STATUS_W).max(URL_BAR_X + 10.0);

        if x >= URL_BAR_X
            && x < url_bar_right
            && y >= URL_BAR_Y + URL_BAR_H
            && y < URL_BAR_Y + URL_BAR_H + dropdown_h
        {
            let relative_y = y - (URL_BAR_Y + URL_BAR_H + padding);
            if relative_y >= 0.0 {
                let index = (relative_y / item_h) as usize;
                if index < layout.dropdown_count {
                    return ChromeHitZone::AutocompleteSuggestion(index);
                }
            }
            return ChromeHitZone::Background;
        }
    }

    if !(0.0..CHROME_HEIGHT).contains(&y) {
        return ChromeHitZone::Background;
    }
    let window_width = layout.window_width;
    if y < TAB_STRIP_HEIGHT {
        for (i, rect) in tab_rects(layout) {
            if (rect.x0..rect.x1).contains(&(x as f64)) {
                // Close first: its glyph lives inside the tab's own rect, so
                // testing the tab body first would swallow every close click.
                return if (x as f64) >= rect.x1 - TAB_CLOSE_W as f64 {
                    ChromeHitZone::TabCloseButton(i)
                } else {
                    ChromeHitZone::TabItem(i)
                };
            }
        }
        let plus = new_tab_rect(layout);
        if (plus.x0..plus.x1).contains(&(x as f64)) {
            return ChromeHitZone::NewTabButton;
        }
        return ChromeHitZone::Background;
    }
    if (HISTORY_X..HISTORY_X + BTN_W).contains(&x) && (BTN_Y..BTN_Y + BTN_H).contains(&y) {
        return ChromeHitZone::HistoryButton;
    }
    if (BACK_X..BACK_X + BTN_W).contains(&x) && (BTN_Y..BTN_Y + BTN_H).contains(&y) {
        return ChromeHitZone::BackButton;
    }
    if (RELOAD_X..RELOAD_X + BTN_W).contains(&x) && (BTN_Y..BTN_Y + BTN_H).contains(&y) {
        return ChromeHitZone::ReloadButton;
    }
    if (FORWARD_X..FORWARD_X + BTN_W).contains(&x) && (BTN_Y..BTN_Y + BTN_H).contains(&y) {
        return ChromeHitZone::ForwardButton;
    }
    let url_bar_right = (window_width - STATUS_W).max(URL_BAR_X + 10.0);
    if x >= URL_BAR_X && x < url_bar_right && (URL_BAR_Y..URL_BAR_Y + URL_BAR_H).contains(&y) {
        return ChromeHitZone::UrlBar;
    }
    ChromeHitZone::Background
}

/// Returns the logical X left edge of the URL text area (inside the bar padding).
pub fn url_text_left(window_width: f32) -> f32 {
    let _ = window_width;
    URL_BAR_X + URL_TEXT_PAD
}
