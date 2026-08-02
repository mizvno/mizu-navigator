//! Tests for the history_sidebar module.

use super::*;
use crate::render::window::history::VisitRecord;

const CHROME: f32 = 54.0;

fn log_with(n: usize) -> HistoryLog {
    let mut log = HistoryLog::default();
    for i in 0..n {
        log.push(VisitRecord::new(
            format!("mizu://test/{i}"),
            format!("Page {i}"),
        ));
    }
    log
}

/// Y coordinate of the vertical centre of row `index`, unscrolled.
fn row_centre_y(index: usize) -> f32 {
    CHROME + HEADER_HEIGHT + GROUP_ROW_H + index as f32 * ROW_H + ROW_H / 2.0
}

#[test]
fn panel_is_docked_to_the_left_edge() {
    assert!(
        contains_x(0.0),
        "the panel starts at the window's left edge"
    );
    assert!(contains_x(SIDEBAR_WIDTH - 1.0));
    assert!(!contains_x(SIDEBAR_WIDTH), "and ends at its own width");
}

#[test]
fn points_outside_the_panel_are_not_hits() {
    let log = log_with(3);
    assert_eq!(
        history_sidebar_hit(SIDEBAR_WIDTH + 1.0, row_centre_y(0), &log, 0.0, CHROME),
        HistorySidebarHit::None,
        "content to the right of the panel must stay clickable"
    );
    assert_eq!(
        history_sidebar_hit(10.0, CHROME - 1.0, &log, 0.0, CHROME),
        HistorySidebarHit::None,
        "the chrome bar owns everything above the panel"
    );
}

#[test]
fn rows_hit_test_newest_first() {
    let log = log_with(3);
    // Newest visit is "mizu://test/2", so it is row 0.
    assert_eq!(
        history_sidebar_hit(40.0, row_centre_y(0), &log, 0.0, CHROME),
        HistorySidebarHit::Entry(0)
    );
    assert_eq!(
        history_sidebar_hit(40.0, row_centre_y(2), &log, 0.0, CHROME),
        HistorySidebarHit::Entry(2)
    );
    assert_eq!(
        log.get(0).unwrap().url,
        "mizu://test/2",
        "index 0 must be the most recent visit"
    );
}

#[test]
fn scrolling_moves_which_row_is_under_the_cursor() {
    let log = log_with(10);
    let y = row_centre_y(0);
    assert_eq!(
        history_sidebar_hit(40.0, y, &log, ROW_H * 3.0, CHROME),
        HistorySidebarHit::Entry(3),
        "a three-row scroll must bring row 3 under the first row's position"
    );
}

#[test]
fn group_headers_are_not_clickable_as_entries() {
    let log = log_with(3);
    let group_y = CHROME + HEADER_HEIGHT + GROUP_ROW_H / 2.0;
    assert_eq!(
        history_sidebar_hit(40.0, group_y, &log, 0.0, CHROME),
        HistorySidebarHit::Background
    );
}

#[test]
fn header_zone_separates_the_clear_button_from_the_title() {
    let log = log_with(1);
    let header_y = CHROME + HEADER_HEIGHT / 2.0;
    assert_eq!(
        history_sidebar_hit(HPAD, header_y, &log, 0.0, CHROME),
        HistorySidebarHit::Background,
        "the title is not a button"
    );
    assert_eq!(
        history_sidebar_hit(SIDEBAR_WIDTH - HPAD - 2.0, header_y, &log, 0.0, CHROME),
        HistorySidebarHit::Clear
    );
}

#[test]
fn empty_log_has_no_content_and_no_scroll() {
    let log = HistoryLog::default();
    assert_eq!(total_content_height(&log), 0.0);
    assert_eq!(clamp_scroll(500.0, &log, 800.0, CHROME), 0.0);
    assert_eq!(
        history_sidebar_hit(40.0, 400.0, &log, 0.0, CHROME),
        HistorySidebarHit::Background,
        "an empty panel still swallows clicks rather than leaking them to the page"
    );
}

#[test]
fn content_height_covers_every_row_and_its_group_header() {
    let log = log_with(4);
    // Four same-day visits under a single "Today" header.
    assert_eq!(total_content_height(&log), GROUP_ROW_H + 4.0 * ROW_H);
}

#[test]
fn scroll_is_clamped_to_the_scrollable_range() {
    let log = log_with(40);
    let window_height = 400.0;
    let max = total_content_height(&log) - visible_height(window_height, CHROME);
    assert_eq!(clamp_scroll(-50.0, &log, window_height, CHROME), 0.0);
    assert_eq!(clamp_scroll(1e6, &log, window_height, CHROME), max);
    assert_eq!(
        scroll_by(max, 10.0, &log, window_height, CHROME),
        max,
        "scrolling past the end must not run off"
    );
}

#[test]
fn short_lists_do_not_scroll_at_all() {
    let log = log_with(2);
    assert_eq!(
        scroll_by(0.0, 50.0, &log, 900.0, CHROME),
        0.0,
        "content that already fits has nothing to scroll"
    );
}
