//! Tests for the inspector model.

use rustc_hash::FxHashMap;

use super::format::{format_action, format_expr};
use super::rows::{
    CONTENT_PREVIEW_CHARS, Decl, fmt_bytes, node_label, node_label_segs, push_decl_rows,
};
use super::types::{Flex, Row, Tone};
use crate::core::types::StringInterner;
use crate::parser::{EventBlock, MizuNode};

#[test]
fn format_expr_roundtrips_simple_source() {
    let mut interner = StringInterner::new();
    let expr =
        crate::parser::logic::parse_expr_standalone("count > 4 && !busy", &mut interner).unwrap();
    let interner = interner.freeze();
    assert_eq!(
        format_expr(expr.root(), &expr.arena, &interner),
        "count > 4 && !busy"
    );
}

#[test]
fn format_action_assign() {
    let mut interner = StringInterner::new();
    let action = crate::parser::logic::parse_action("count = count + 1", &mut interner).unwrap();
    let interner = interner.freeze();
    assert_eq!(format_action(&action, &interner), "count = count + 1");
}

fn node_with(attrs: &[(&str, &str)], events: &[&str]) -> MizuNode {
    let mut attributes = FxHashMap::default();
    for (k, v) in attrs {
        attributes.insert(k.to_string(), v.to_string());
    }
    let mut event_map = FxHashMap::default();
    let mut it = StringInterner::new();
    for name in events {
        let action = crate::parser::logic::parse_action("x = 1", &mut it).unwrap();
        event_map.insert((*name).to_string(), EventBlock::Click { action });
    }
    MizuNode {
        primitive: crate::parser::Primitive::Button,
        attributes,
        events: event_map,
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

#[test]
fn node_label_shows_events_and_class() {
    let node = node_with(&[("class", "card")], &["click"]);
    let label = node_label(&node, None);
    assert!(label.contains("button"));
    assert!(label.contains(".card"));
    assert!(label.contains("[click]"));
}

#[test]
fn node_label_segments_are_individually_toned() {
    let node = node_with(&[("id", "save"), ("class", "primary")], &[]);
    let segs = node_label_segs(&node, None);
    assert_eq!(segs[0].text, "button");
    assert_eq!(segs[0].tone, Tone::Accent, "the tag carries the accent");
    assert_eq!(segs[1].text, "#save");
    assert_eq!(segs[2].text, ".primary");
    assert!(
        segs.iter().all(|s| s.flex != Flex::Elide),
        "with no text content there is nothing that should yield first"
    );
}

#[test]
fn long_content_is_the_segment_that_yields() {
    let long = "a".repeat(400);
    let node = node_with(&[("content", long.as_str())], &[]);
    let segs = node_label_segs(&node, None);
    let content = segs
        .iter()
        .find(|s| s.text.starts_with('"'))
        .expect("content segment");
    assert_eq!(
        content.flex,
        Flex::Elide,
        "the text preview must be the segment the painter shrinks"
    );
    assert!(
        content.text.chars().count() <= CONTENT_PREVIEW_CHARS + 3,
        "a whole paragraph must not be pasted onto one tree row"
    );
}

#[test]
fn content_preview_collapses_newlines() {
    let node = node_with(&[("content", "first\n\n  second")], &[]);
    let label = node_label(&node, None);
    assert!(
        label.contains("\"first second\""),
        "embedded whitespace must not break the single-line row: {label}"
    );
}

#[test]
fn row_heights_differ_by_role() {
    assert!(
        Row::header("x").height() > Row::item(0, vec![]).height(),
        "headers need the extra leading that separates sections"
    );
    assert!(Row::detail(0, vec![]).height() < Row::item(0, vec![]).height());
}

#[test]
fn byte_counts_are_human_readable() {
    assert_eq!(fmt_bytes(512), "512 B");
    assert_eq!(fmt_bytes(2048), "2.0 KB");
    assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
}

#[test]
fn declaration_names_are_padded_into_a_column() {
    let mut rows = Vec::new();
    push_decl_rows(
        &mut rows,
        &[Decl::new("gap", "4"), Decl::new("border-radius", "8")],
    );
    let widths: Vec<usize> = rows.iter().map(|r| r.segs[0].text.len()).collect();
    assert_eq!(
        widths[0], widths[1],
        "values must line up in a column, so names pad to a common width"
    );
}
