//! Tests for the bidi module.

use super::*;
use rustc_hash::FxHashMap;

fn node(dir: Option<&str>) -> MizuNode {
    let mut attributes = FxHashMap::default();
    if let Some(d) = dir {
        attributes.insert("dir".to_string(), d.to_string());
    }
    MizuNode {
        primitive: crate::parser::Primitive::Box,
        attributes,
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

fn lang_node(lang: Option<&str>) -> MizuNode {
    let mut attributes = FxHashMap::default();
    if let Some(l) = lang {
        attributes.insert("lang".to_string(), l.to_string());
    }
    MizuNode {
        primitive: crate::parser::Primitive::Box,
        attributes,
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

#[test]
fn resolve_direction_finds_explicit_dir_on_self() {
    let tree = ego_tree::Tree::new(node(Some("rtl")));
    assert_eq!(resolve_direction(tree.root()), ResolvedDirection::Rtl);
}

#[test]
fn resolve_direction_inherits_from_ancestor() {
    let mut tree = ego_tree::Tree::new(node(Some("rtl")));
    let child_id = tree.root_mut().append(node(None)).id();
    let grandchild_id = tree.get_mut(child_id).unwrap().append(node(None)).id();
    assert_eq!(
        resolve_direction(tree.get(grandchild_id).unwrap()),
        ResolvedDirection::Rtl,
        "an unset dir must inherit from the nearest ancestor that set one"
    );
}

#[test]
fn resolve_direction_explicit_auto_does_not_stop_inheritance() {
    let mut tree = ego_tree::Tree::new(node(Some("rtl")));
    let child_id = tree.root_mut().append(node(Some("auto"))).id();
    assert_eq!(
        resolve_direction(tree.get(child_id).unwrap()),
        ResolvedDirection::Rtl,
        "`dir=\"auto\"` must not shadow an ancestor's explicit direction"
    );
}

#[test]
fn resolve_direction_child_overrides_ancestor() {
    let mut tree = ego_tree::Tree::new(node(Some("rtl")));
    let child_id = tree.root_mut().append(node(Some("ltr"))).id();
    assert_eq!(
        resolve_direction(tree.get(child_id).unwrap()),
        ResolvedDirection::Ltr
    );
}

#[test]
fn resolve_direction_defaults_to_auto_with_no_dir_anywhere() {
    let tree = ego_tree::Tree::new(node(None));
    assert_eq!(resolve_direction(tree.root()), ResolvedDirection::Auto);
}

#[test]
fn is_rtl_for_layout_treats_auto_as_ltr() {
    assert!(!ResolvedDirection::Auto.is_rtl_for_layout());
    assert!(!ResolvedDirection::Ltr.is_rtl_for_layout());
    assert!(ResolvedDirection::Rtl.is_rtl_for_layout());
}

#[test]
fn prepend_mark_matches_explicit_direction_only() {
    assert_eq!(ResolvedDirection::Ltr.prepend_mark(), Some('\u{200E}'));
    assert_eq!(ResolvedDirection::Rtl.prepend_mark(), Some('\u{200F}'));
    assert_eq!(ResolvedDirection::Auto.prepend_mark(), None);
}

#[test]
fn resolve_lang_finds_explicit_lang_on_self() {
    let tree = ego_tree::Tree::new(lang_node(Some("en")));
    assert_eq!(resolve_lang(tree.root()), Some("en".to_string()));
}

#[test]
fn resolve_lang_inherits_from_ancestor() {
    let mut tree = ego_tree::Tree::new(lang_node(Some("it")));
    let child_id = tree.root_mut().append(lang_node(None)).id();
    let grandchild_id = tree.get_mut(child_id).unwrap().append(lang_node(None)).id();
    assert_eq!(
        resolve_lang(tree.get(grandchild_id).unwrap()),
        Some("it".to_string()),
        "an unset lang must inherit from the nearest ancestor that set one"
    );
}

#[test]
fn resolve_lang_child_overrides_ancestor() {
    let mut tree = ego_tree::Tree::new(lang_node(Some("en")));
    let child_id = tree.root_mut().append(lang_node(Some("ja"))).id();
    assert_eq!(
        resolve_lang(tree.get(child_id).unwrap()),
        Some("ja".to_string())
    );
}

#[test]
fn resolve_lang_defaults_to_none_with_no_lang_anywhere() {
    let tree = ego_tree::Tree::new(lang_node(None));
    assert_eq!(resolve_lang(tree.root()), None);
}

#[test]
fn strip_bidi_overrides_removes_rlo_and_isolates() {
    let input = "evil\u{202E}gnp.exe\u{2066}safe\u{2069}";
    let stripped = strip_bidi_overrides(input);
    assert_eq!(stripped, "evilgnp.exesafe");
    assert!(!stripped.chars().any(is_bidi_override_or_isolate));
}

#[test]
fn strip_bidi_overrides_leaves_clean_strings_untouched() {
    let input = "mizu://example.com/page";
    let stripped = strip_bidi_overrides(input);
    assert_eq!(stripped, input);
    assert!(matches!(stripped, std::borrow::Cow::Borrowed(_)));
}

#[test]
fn strip_bidi_overrides_does_not_touch_legitimate_bidi_text() {
    // Sanity: only the specific override/isolate ranges are stripped —
    // ordinary Hebrew/Arabic text (which is NOT in either stripped
    // range; it's just letters with strong bidi *properties*, not
    // format control characters) must pass through untouched.
    let hebrew = "\u{05E9}\u{05DC}\u{05D5}\u{05DD}"; // "שלום"
    let stripped = strip_bidi_overrides(hebrew);
    assert_eq!(stripped, hebrew);
}
