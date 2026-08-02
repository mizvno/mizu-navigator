//! Tests for the mod module.

use super::model::{InspectValue, Row, Seg, Tone};
use super::*;

/// A generous stand-in for the window height in tests that do not care
/// about the panel's overall extent, only about rows near its top.
const PANEL_H: f32 = 600.0;

/// A one-node tree, plus its node id, for exercising row routing.
fn tree_with_root() -> (Tree<MizuNode>, EgoNodeId) {
    let node = MizuNode {
        primitive: crate::parser::Primitive::Box,
        attributes: Default::default(),
        events: Default::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    };
    let tree = Tree::new(node);
    let id = tree.root().id();
    (tree, id)
}

fn expandable_row(node: EgoNodeId, indent: u8) -> Row {
    Row {
        kind: model::RowKind::Item,
        indent,
        segs: vec![Seg::mono("box", Tone::Accent)],
        node: Some(node),
        expandable: true,
        expanded: true,
        inspect: None,
    }
}

/// A non-tree row carrying a full-value payload, as Logic/Network/Events
/// rows do.
fn inspectable_row(title: &str, text: &str) -> Row {
    Row {
        kind: model::RowKind::Item,
        indent: 1,
        segs: vec![Seg::mono(text, Tone::Value)],
        node: None,
        expandable: false,
        expanded: false,
        inspect: Some(InspectValue {
            title: title.to_string(),
            text: text.to_string(),
        }),
    }
}

#[test]
fn toggle_closes_picker() {
    let mut s = InspectorState::new();
    s.open = true;
    s.picker = true;
    s.toggle();
    assert!(!s.open);
    assert!(!s.picker, "closing the panel must cancel picker mode");
}

#[test]
fn toggle_closes_the_value_drawer() {
    let mut s = InspectorState::new();
    s.open = true;
    s.value_view = Some(ValueView::new("Value".into(), "x".repeat(100)));
    s.toggle();
    assert!(
        s.value_view.is_none(),
        "closing the panel must not leave a stale drawer open"
    );
}

#[test]
fn scroll_is_clamped_to_content() {
    let mut s = InspectorState::new();
    s.max_scroll = 100.0;
    s.scroll_by(250.0);
    assert_eq!(s.scroll_offset(), 100.0);
    s.scroll_by(-500.0);
    assert_eq!(s.scroll_offset(), 0.0);
}

#[test]
fn tab_bar_click_switches_tab() {
    let mut s = InspectorState::new();
    let rows: Vec<Row> = Vec::new();
    // Click in the last tab slot (Network).
    let tab_strip = PANEL_WIDTH - PICKER_BTN_WIDTH;
    let outcome = handle_panel_click(&mut s, &rows, PANEL_H, tab_strip - 1.0, 10.0);
    assert!(outcome.changed);
    assert_eq!(s.tab, InspectorTab::Network);
}

#[test]
fn every_tab_is_reachable_from_its_own_slot() {
    let strip = PANEL_WIDTH - PICKER_BTN_WIDTH;
    let width = strip / InspectorTab::ALL.len() as f32;
    for (i, tab) in InspectorTab::ALL.iter().enumerate() {
        let centre = i as f32 * width + width / 2.0;
        assert_eq!(
            panel_hit(&[], 0.0, PANEL_H, false, centre, 5.0),
            PanelHit::Tab(*tab),
            "the centre of slot {i} must select {tab:?}"
        );
    }
}

#[test]
fn picker_button_toggles_picker() {
    let mut s = InspectorState::new();
    let rows: Vec<Row> = Vec::new();
    let outcome = handle_panel_click(&mut s, &rows, PANEL_H, PANEL_WIDTH - 5.0, 10.0);
    assert!(outcome.changed);
    assert!(s.picker);
}

#[test]
fn row_tops_accumulate_per_row_heights() {
    let (_tree, id) = tree_with_root();
    let rows = vec![
        Row {
            kind: model::RowKind::Header,
            indent: 0,
            segs: vec![],
            node: None,
            expandable: false,
            expanded: false,
            inspect: None,
        },
        expandable_row(id, 0),
    ];
    let tops = row_tops(&rows);
    assert_eq!(tops.len(), 3);
    assert_eq!(tops[0], CONTENT_VPAD);
    assert_eq!(tops[1], CONTENT_VPAD + HEADER_ROW_HEIGHT);
    assert_eq!(
        tops[2],
        CONTENT_VPAD + HEADER_ROW_HEIGHT + ROW_HEIGHT + CONTENT_VPAD
    );
}

#[test]
fn row_at_resolves_variable_height_rows() {
    let (_tree, id) = tree_with_root();
    let rows = vec![
        Row {
            kind: model::RowKind::Header,
            indent: 0,
            segs: vec![],
            node: None,
            expandable: false,
            expanded: false,
            inspect: None,
        },
        expandable_row(id, 0),
    ];
    let tops = row_tops(&rows);
    assert_eq!(row_at(&tops, CONTENT_VPAD + 1.0), Some(0));
    assert_eq!(
        row_at(&tops, CONTENT_VPAD + HEADER_ROW_HEIGHT + 1.0),
        Some(1)
    );
    assert_eq!(row_at(&tops, 0.0), None, "the top padding is not a row");
    assert_eq!(row_at(&tops, 10_000.0), None, "past the end is not a row");
}

#[test]
fn selecting_a_parent_does_not_collapse_it() {
    let (_tree, id) = tree_with_root();
    let rows = vec![expandable_row(id, 0)];
    let mut s = InspectorState::new();
    // Click well to the right of the disclosure triangle.
    let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
    let x = row_content_left(0) + TWISTY_WIDTH + 20.0;
    assert!(handle_panel_click(&mut s, &rows, PANEL_H, x, y).changed);
    assert_eq!(s.selected, Some(id));
    assert!(
        s.collapsed.is_empty(),
        "selecting a container must not hide its children"
    );
}

#[test]
fn the_twisty_toggles_without_selecting() {
    let (_tree, id) = tree_with_root();
    let rows = vec![expandable_row(id, 0)];
    let mut s = InspectorState::new();
    let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
    let x = row_content_left(0) + TWISTY_WIDTH / 2.0;
    assert!(handle_panel_click(&mut s, &rows, PANEL_H, x, y).changed);
    assert!(s.collapsed.contains(&id));
    assert_eq!(
        s.selected, None,
        "the triangle is a disclosure, not a target"
    );
    // And it toggles back.
    assert!(handle_panel_click(&mut s, &rows, PANEL_H, x, y).changed);
    assert!(s.collapsed.is_empty());
}

#[test]
fn the_twisty_strip_follows_indentation() {
    let (_tree, id) = tree_with_root();
    let rows = vec![expandable_row(id, 3)];
    let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
    assert_eq!(
        panel_hit(&rows, 0.0, PANEL_H, false, row_content_left(0) + 2.0, y),
        PanelHit::Row(0),
        "a deep row's triangle is not at the panel's left margin"
    );
    assert_eq!(
        panel_hit(&rows, 0.0, PANEL_H, false, row_content_left(3) + 2.0, y),
        PanelHit::Twisty(0)
    );
}

#[test]
fn clicks_past_the_last_row_hit_nothing() {
    let (_tree, id) = tree_with_root();
    let rows = vec![expandable_row(id, 0)];
    let mut s = InspectorState::new();
    assert!(!handle_panel_click(&mut s, &rows, PANEL_H, 100.0, content_top() + 900.0).changed);
    assert_eq!(s.selected, None);
}

#[test]
fn scrolled_rows_hit_test_at_their_scrolled_position() {
    let (_tree, id) = tree_with_root();
    let rows = vec![expandable_row(id, 0), expandable_row(id, 0)];
    // Scroll the first row off the top; the second now sits where the
    // first was.
    assert_eq!(
        panel_hit(
            &rows,
            ROW_HEIGHT,
            PANEL_H,
            false,
            200.0,
            content_top() + CONTENT_VPAD + 2.0
        ),
        PanelHit::Row(1)
    );
}

// ── Value drawer ─────────────────────────────────────────────────────

#[test]
fn clicking_a_long_value_row_opens_the_drawer() {
    let long = "x".repeat(200);
    let rows = vec![inspectable_row("Value", &long)];
    let mut s = InspectorState::new();
    let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
    let outcome = handle_panel_click(&mut s, &rows, PANEL_H, 50.0, y);
    assert!(outcome.changed);
    let view = s.value_view.expect("the drawer must open");
    assert_eq!(view.title, "Value");
    assert_eq!(view.text, long, "the drawer must show the untruncated text");
}

#[test]
fn clicking_the_same_value_again_closes_the_drawer() {
    let long = "y".repeat(200);
    let rows = vec![inspectable_row("Value", &long)];
    let mut s = InspectorState::new();
    let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
    handle_panel_click(&mut s, &rows, PANEL_H, 50.0, y);
    assert!(s.value_view.is_some());
    handle_panel_click(&mut s, &rows, PANEL_H, 50.0, y);
    assert!(
        s.value_view.is_none(),
        "clicking the same value must toggle the drawer closed, like the twisty"
    );
}

#[test]
fn clicking_a_different_value_replaces_the_drawer() {
    let rows = vec![inspectable_row("Value", &"a".repeat(200)), {
        let mut row = inspectable_row("Target", &"b".repeat(200));
        row.kind = model::RowKind::Detail;
        row
    }];
    let mut s = InspectorState::new();
    let y1 = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
    handle_panel_click(&mut s, &rows, PANEL_H, 50.0, y1);
    let y2 = content_top() + CONTENT_VPAD + ROW_HEIGHT + DETAIL_ROW_HEIGHT / 2.0;
    handle_panel_click(&mut s, &rows, PANEL_H, 50.0, y2);
    let view = s
        .value_view
        .expect("a different value must open, not close");
    assert_eq!(view.title, "Target");
}

#[test]
fn the_drawer_occupies_the_panel_bottom_and_its_buttons_are_reachable() {
    let drawer_top = PANEL_H - DRAWER_HEIGHT;
    assert_eq!(
        panel_hit(&[], 0.0, PANEL_H, true, PANEL_WIDTH - 5.0, drawer_top + 5.0),
        PanelHit::DrawerClose
    );
    assert_eq!(
        panel_hit(
            &[],
            0.0,
            PANEL_H,
            true,
            PANEL_WIDTH - DRAWER_CLOSE_WIDTH - 5.0,
            drawer_top + 5.0
        ),
        PanelHit::DrawerCopy
    );
    assert_eq!(
        panel_hit(
            &[],
            0.0,
            PANEL_H,
            true,
            20.0,
            drawer_top + DRAWER_HEADER_HEIGHT + 10.0
        ),
        PanelHit::DrawerBody
    );
}

#[test]
fn closing_the_drawer_frees_the_rows_it_was_covering() {
    let (_tree, id) = tree_with_root();
    let rows = vec![expandable_row(id, 0)];
    // With the drawer open, a click at the row's old position (now under
    // the drawer) must not reach the row.
    let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
    // Only true if the row sits below the drawer's top; pick a tall panel
    // so the row is unambiguously in the drawer's shadow instead.
    let short_panel = content_top() + DRAWER_HEIGHT - 1.0;
    assert_eq!(
        panel_hit(
            &rows,
            0.0,
            short_panel,
            true,
            100.0,
            y.min(short_panel - 1.0)
        ),
        PanelHit::DrawerBody,
        "a row underneath an open drawer must not be clickable"
    );
}

#[test]
fn copy_reports_the_full_text_without_touching_the_clipboard() {
    let long = "z".repeat(200);
    let mut s = InspectorState::new();
    s.value_view = Some(ValueView::new("Value".into(), long.clone()));
    let drawer_top = PANEL_H - DRAWER_HEIGHT;
    let outcome = handle_panel_click(
        &mut s,
        &[],
        PANEL_H,
        PANEL_WIDTH - DRAWER_CLOSE_WIDTH - 5.0,
        drawer_top + 5.0,
    );
    assert_eq!(
        outcome.copy,
        Some(long),
        "the click must hand back the exact text to copy"
    );
    assert!(
        s.value_view.is_some(),
        "copying must not itself close the drawer"
    );
    assert!(
        s.value_view.unwrap().copied_at.is_some(),
        "the flash timestamp must be recorded for the paint pass"
    );
}

#[test]
fn drawer_close_button_closes_it() {
    let mut s = InspectorState::new();
    s.value_view = Some(ValueView::new("Value".into(), "hello".into()));
    let drawer_top = PANEL_H - DRAWER_HEIGHT;
    let outcome = handle_panel_click(&mut s, &[], PANEL_H, PANEL_WIDTH - 5.0, drawer_top + 5.0);
    assert!(outcome.changed);
    assert!(s.value_view.is_none());
}
