//! Tests for the chrome bar: geometry/hit-zone consistency, cursor/selection
//! editing, and palette contrast.

use super::*;

fn make_state(url: &str) -> ChromeState {
    ChromeState {
        url: url.to_string(),
        cursor: url.len(),
        ..Default::default()
    }
}

#[test]
fn handle_key_enter_adds_schema() {
    let mut s = make_state("example.com");
    let action = s.handle_key(&Key::Named(NamedKey::Enter), None, ModifiersState::empty());
    match action {
        ChromeKeyAction::Navigate(url) => assert_eq!(url, "mizu://example.com"),
        other => panic!("expected Navigate, got {:?}", other),
    }
}

#[test]
fn handle_key_enter_preserves_existing_schema() {
    let mut s = make_state("https://example.com");
    let action = s.handle_key(&Key::Named(NamedKey::Enter), None, ModifiersState::empty());
    match action {
        ChromeKeyAction::Navigate(url) => assert_eq!(url, "https://example.com"),
        other => panic!("expected Navigate, got {:?}", other),
    }
}

#[test]
fn insert_text_advances_cursor() {
    let mut s = make_state("ab");
    s.cursor = 1; // between 'a' and 'b'
    s.insert_text("X");
    assert_eq!(s.url, "aXb");
    assert_eq!(s.cursor, 2);
}

// â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
// Bidi anti-spoofing (ux-7): insert_text is the single choke point both
// typed characters and paste go through â€” see docs/design/bidi.md Â§4.
// â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[test]
fn insert_text_strips_rlo_override_character() {
    // Security regression: U+202E (Right-to-Left Override) must never
    // enter the URL bar's buffer â€” typing or pasting it must not be able
    // to visually disguise a domain.
    let mut s = make_state("");
    s.insert_text("evil\u{202E}gnp.exe");
    assert!(
        !s.url.contains('\u{202E}'),
        "RLO must be stripped, got: {:?}",
        s.url
    );
    assert_eq!(s.url, "evilgnp.exe");
}

#[test]
fn insert_text_strips_bidi_isolates_too() {
    let mut s = make_state("");
    s.insert_text("a\u{2066}b\u{2069}c");
    assert_eq!(s.url, "abc");
}

#[test]
fn insert_text_leaves_clean_urls_untouched() {
    let mut s = make_state("");
    s.insert_text("mizu://example.com/page");
    assert_eq!(s.url, "mizu://example.com/page");
}

#[test]
fn paste_text_also_strips_bidi_overrides() {
    // paste_text -> insert_text, so it inherits the same choke point;
    // pinned separately since paste is a distinct entry point a user
    // (or a malicious clipboard source) could exploit independently of
    // typing.
    let mut s = make_state("");
    s.paste_text("safe\u{202E}evil.com");
    assert!(!s.url.contains('\u{202E}'));
}

#[test]
fn delete_backward_removes_selection() {
    let mut s = make_state("hello");
    s.selection = Some((1, 4)); // select "ell"
    s.cursor = 4;
    s.delete_backward();
    assert_eq!(s.url, "ho");
    assert_eq!(s.cursor, 1);
    assert!(s.selection.is_none());
}

#[test]
fn delete_backward_no_selection_removes_char() {
    let mut s = make_state("abc");
    s.cursor = 2;
    s.delete_backward();
    assert_eq!(s.url, "ac");
    assert_eq!(s.cursor, 1);
}

#[test]
fn select_all_covers_entire_string() {
    let mut s = make_state("hello world");
    s.select_all();
    assert_eq!(s.selection, Some((0, 11)));
    assert_eq!(s.cursor, 11);
}

#[test]
fn selection_range_normalises_inverted_selection() {
    let mut s = make_state("hello");
    s.selection = Some((4, 1)); // inverted (user dragged left)
    assert_eq!(s.selection_range(), Some((1, 4)));
}

#[test]
fn cursor_clamp_on_move_left_at_start() {
    let mut s = make_state("abc");
    s.cursor = 0;
    s.move_left(false);
    assert_eq!(s.cursor, 0);
}

#[test]
fn cursor_clamp_on_move_right_at_end() {
    let mut s = make_state("abc");
    s.cursor = 3;
    s.move_right(false);
    assert_eq!(s.cursor, 3);
}

/// A single-tab strip over an 800px window.
fn test_layout(tab_count: usize) -> ChromeLayout {
    ChromeLayout {
        window_width: 800.0,
        tab_count,
        dropdown_count: 0,
    }
}

/// Hit-tests a point given in *navigation-bar* coordinates: `y` is
/// measured from the top of the bar, below the tab strip.
fn bar_zone(x: f32, y: f32) -> ChromeHitZone {
    chrome_hit_zone(x, y + TAB_STRIP_HEIGHT, &test_layout(1))
}

#[test]
fn tab_strip_hit_zones_prefer_close_over_body() {
    let layout = test_layout(3);
    let (_, first) = tab_rects(&layout).next().expect("at least one tab fits");
    assert_eq!(
        chrome_hit_zone(first.x0 as f32 + 4.0, 8.0, &layout),
        ChromeHitZone::TabItem(0)
    );
    assert_eq!(
        chrome_hit_zone(first.x1 as f32 - 4.0, 8.0, &layout),
        ChromeHitZone::TabCloseButton(0),
        "the close glyph sits inside the tab rect, so it must be tested first"
    );
}

#[test]
fn new_tab_button_follows_the_last_tab() {
    let layout = test_layout(2);
    let last = tab_rects(&layout).last().expect("tabs fit").1;
    assert_eq!(
        chrome_hit_zone(last.x1 as f32 + 4.0, 8.0, &layout),
        ChromeHitZone::NewTabButton
    );
}

#[test]
fn tab_strip_clips_rather_than_overflowing() {
    // 32 tabs at the 80px minimum need 2560px; only what fits in 800px
    // (minus the new-tab button) is produced.
    let layout = test_layout(32);
    let visible = tab_rects(&layout).count();
    assert!(visible < 32 && visible > 0, "got {visible} visible tabs");
    assert!(
        tab_rects(&layout).all(|(_, r)| r.x1 <= (layout.window_width - 24.0) as f64),
        "no tab may extend past the new-tab button"
    );
}

#[test]
fn chrome_hit_zone_history_button() {
    assert_eq!(
        bar_zone(HISTORY_X + 5.0, 10.0),
        ChromeHitZone::HistoryButton,
        "the sidebar toggle leads the bar, on the side the panel opens"
    );
}

#[test]
fn chrome_hit_zone_back_button() {
    assert_eq!(bar_zone(BACK_X + 5.0, 10.0), ChromeHitZone::BackButton);
}

#[test]
fn chrome_hit_zone_reload_button() {
    assert_eq!(bar_zone(RELOAD_X + 8.0, 10.0), ChromeHitZone::ReloadButton);
}

#[test]
fn chrome_hit_zone_forward_button() {
    assert_eq!(
        bar_zone(FORWARD_X + 5.0, 10.0),
        ChromeHitZone::ForwardButton
    );
}

#[test]
fn chrome_hit_zone_url_bar() {
    assert_eq!(bar_zone(200.0, 10.0), ChromeHitZone::UrlBar);
}

#[test]
fn toolbar_buttons_do_not_overlap() {
    // Each button owns BTN_W pixels, and every pair must leave a gap:
    // that gap is what the "background between buttons" cases below hit.
    // Left to right as they are laid out, which is not the order the
    // constants happen to be declared in.
    let xs = [HISTORY_X, BACK_X, FORWARD_X, RELOAD_X, URL_BAR_X];
    for pair in xs.windows(2) {
        assert!(
            pair[0] + BTN_W <= pair[1],
            "the button at {} overruns the one at {}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn chrome_hit_zone_background_below_chrome() {
    assert_eq!(
        chrome_hit_zone(200.0, CHROME_HEIGHT + 1.0, &test_layout(1)),
        ChromeHitZone::Background
    );
}

#[test]
fn chrome_hit_zone_background_between_reload_and_forward() {
    assert_eq!(
        bar_zone(RELOAD_X + BTN_W + 1.0, 10.0),
        ChromeHitZone::Background
    );
}

#[test]
fn chrome_hit_zone_background_between_forward_and_url_bar() {
    assert_eq!(
        bar_zone(FORWARD_X + BTN_W + 1.0, 10.0),
        ChromeHitZone::Background
    );
}

#[test]
fn paste_text_replaces_selection() {
    let mut s = make_state("hello");
    s.selection = Some((0, 5));
    s.cursor = 5;
    s.paste_text("world");
    assert_eq!(s.url, "world");
    assert_eq!(s.cursor, 5);
    assert!(s.selection.is_none());
}

#[test]
fn cut_text_returns_selection() {
    let mut s = make_state("hello");
    s.selection = Some((1, 4));
    s.cursor = 4;
    let cut = s.cut_text();
    assert_eq!(cut, Some("ell".to_string()));
    assert_eq!(s.url, "ho");
}

#[test]
fn ctrl_a_action() {
    let mut s = make_state("hello");
    let mods = ModifiersState::CONTROL;
    let action = s.handle_key(&Key::Character("a".into()), None, mods);
    matches!(action, ChromeKeyAction::Handled);
    assert_eq!(s.selection, Some((0, 5)));
}
