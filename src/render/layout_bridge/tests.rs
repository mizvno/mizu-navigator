//! Tests for the layout_bridge module.

use std::collections::HashMap;
use std::sync::Arc;

use ego_tree::NodeId as EgoNodeId;
use taffy::TaffyTree;
use taffy::style::FlexDirection;

use super::*;
use crate::core::types::{StringInterner, Value, VariableStore};
use crate::parser::StyleRules;
use crate::parser::layout::parse_layout;
use crate::parser::style::parse_style_with_variants;
use crate::render::bidi::ResolvedDirection;
use crate::render::preferences::ColorScheme;
use crate::render::responsive::{RenderEnvironment, ViewportSize};

/// L1 regression (ux-6): a breakpoint toggling must never change the
/// synthetic/Taffy node count. A breakpoint selects among rule sets — it
/// does not duplicate subtrees — so `MAX_SYNTHETIC_LAYOUT_NODES`
/// (invariant L1) is structurally unaffected by resizing across a
/// threshold. This drives `build_taffy_tree` directly at two window
/// widths straddling the same document's breakpoint and asserts the
/// resulting Taffy node count (one node per DOM node, by construction)
/// is identical either side.
#[test]
fn breakpoint_toggle_does_not_change_node_count() {
    let style = r"
.box
    width 240
.box @max-width 599
    width 100%
    flex-direction column
";
    let (style_rules, variants) = parse_style_with_variants(style).unwrap();
    assert_eq!(variants.len(), 1, "fixture must define exactly one variant");

    let mut interner = StringInterner::new();
    let dom = parse_layout(
        "doc\n    box class box\n        box class box\n        box class box\n",
        &mut interner,
    )
    .unwrap();
    let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());

    let narrow_env = RenderEnvironment {
        viewport: ViewportSize {
            width: 400.0,
            height: 800.0,
        },
        color_scheme: ColorScheme::Dark,
    };
    let wide_env = RenderEnvironment {
        viewport: ViewportSize {
            width: 1200.0,
            height: 800.0,
        },
        color_scheme: ColorScheme::Dark,
    };

    let mut narrow_taffy = TaffyTree::new();
    let mut narrow_map = HashMap::new();
    build_taffy_tree(
        dom.root(),
        &mut TaffyBuildContext {
            style_rules_map: &style_rules,
            taffy: &mut narrow_taffy,
            node_to_taffy_id: &mut narrow_map,
            image_cache: &mut image_cache,
            chrome_url: "mizu://test/index.mizu",
            variants: &variants,
            env: &narrow_env,
        },
    )
    .unwrap();

    let mut wide_taffy = TaffyTree::new();
    let mut wide_map = HashMap::new();
    build_taffy_tree(
        dom.root(),
        &mut TaffyBuildContext {
            style_rules_map: &style_rules,
            taffy: &mut wide_taffy,
            node_to_taffy_id: &mut wide_map,
            image_cache: &mut image_cache,
            chrome_url: "mizu://test/index.mizu",
            variants: &variants,
            env: &wide_env,
        },
    )
    .unwrap();

    assert_eq!(
        narrow_map.len(),
        wide_map.len(),
        "node count (one Taffy node per DOM node) must be identical on \
         either side of the breakpoint — a breakpoint selects a rule \
         set, it must never duplicate a subtree"
    );
    assert_eq!(
        narrow_map.len(),
        dom.nodes().count(),
        "sanity: build_taffy_tree must create exactly one Taffy node per DOM node"
    );
}

fn setup_test_store(items: Vec<Value>) -> VariableStore {
    let interner = StringInterner::new();
    let mut store = crate::core::types::VariableStore {
        evaluator: Default::default(),
        interner,
    };
    store.set("items", Value::List(Arc::new(items)));
    let store = store.freeze();
    store
}

#[test]
fn each_small_list_unaffected_by_budget() {
    let mut interner = StringInterner::new();
    let dom = parse_layout("doc\n    each x in items\n        box\n", &mut interner).unwrap();
    let store = setup_test_store(vec![Value::Bool(true); 5]);
    let mut taffy = TaffyTree::new();
    let mut node_to_taffy = HashMap::new();
    for node in dom.nodes() {
        node_to_taffy.insert(
            node.id(),
            taffy.new_leaf(taffy::style::Style::default()).unwrap(),
        );
    }

    let prev = EachExpansion::default();
    let expansion = expand_each_nodes(
        &dom,
        &store,
        &mut taffy,
        &node_to_taffy,
        &prev,
        None,
        0.0,
        800.0,
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap();

    assert!(
        expansion.truncated.is_empty(),
        "Small list should not be truncated"
    );
    let each_node = dom.root().children().next().unwrap().id();
    assert_eq!(expansion.groups.get(&each_node).unwrap().len(), 5);
}

#[test]
fn each_huge_list_clamped_to_budget() {
    let mut interner = StringInterner::new();
    let dom = parse_layout("doc\n    each x in items\n        box\n", &mut interner).unwrap();
    let store = setup_test_store(vec![Value::Bool(true); *MAX_SYNTHETIC_LAYOUT_NODES + 100]);
    let mut taffy = TaffyTree::new();
    let mut node_to_taffy = HashMap::new();
    for node in dom.nodes() {
        node_to_taffy.insert(
            node.id(),
            taffy.new_leaf(taffy::style::Style::default()).unwrap(),
        );
    }

    let prev = EachExpansion::default();
    // An oversized viewport makes the *needed* virtualization window span
    // the whole list, isolating the budget-clamp path from windowing —
    // see `each_huge_list_with_normal_viewport_is_virtualized_not_truncated`
    // for the "normal viewport" case.
    let expansion = expand_each_nodes(
        &dom,
        &store,
        &mut taffy,
        &node_to_taffy,
        &prev,
        None,
        0.0,
        10_000_000.0,
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap();

    let each_node = dom.root().children().next().unwrap().id();
    let truncated = expansion.truncated.get(&each_node).copied().unwrap_or(0);
    assert!(truncated > 0, "Huge list must be truncated");
    assert_eq!(
        expansion.groups.get(&each_node).unwrap().len() + truncated,
        *MAX_SYNTHETIC_LAYOUT_NODES + 100
    );
}

#[test]
fn each_huge_list_with_normal_viewport_is_virtualized_not_truncated() {
    let mut interner = StringInterner::new();
    let dom = parse_layout("doc\n    each x in items\n        box\n", &mut interner).unwrap();
    let store = setup_test_store(vec![Value::Bool(true); *MAX_SYNTHETIC_LAYOUT_NODES + 100]);
    let mut taffy = TaffyTree::new();
    let mut node_to_taffy = HashMap::new();
    for node in dom.nodes() {
        node_to_taffy.insert(
            node.id(),
            taffy.new_leaf(taffy::style::Style::default()).unwrap(),
        );
    }

    let prev = EachExpansion::default();
    let expansion = expand_each_nodes(
        &dom,
        &store,
        &mut taffy,
        &node_to_taffy,
        &prev,
        None,
        0.0,
        800.0,
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap();

    let each_node = dom.root().children().next().unwrap().id();
    assert!(
        expansion.truncated.is_empty(),
        "a normal viewport must virtualize a huge list, not hit the budget backstop"
    );
    let window_len = expansion.groups.get(&each_node).unwrap().len();
    assert!(
        window_len < 200,
        "only a small window of rows near the viewport should be expanded, got {window_len}"
    );
    assert_eq!(
        expansion
            .window_start
            .get(&each_node)
            .copied()
            .unwrap_or(999),
        0,
        "scrolled to the top, the window should start at index 0"
    );
}

#[test]
fn scrolling_shifts_the_virtualized_window() {
    let mut interner = StringInterner::new();
    let dom = parse_layout("doc\n    each x in items\n        box\n", &mut interner).unwrap();
    let store = setup_test_store(vec![Value::Bool(true); *MAX_SYNTHETIC_LAYOUT_NODES + 100]);
    let mut taffy = TaffyTree::new();
    let mut node_to_taffy = HashMap::new();
    for node in dom.nodes() {
        node_to_taffy.insert(
            node.id(),
            taffy.new_leaf(taffy::style::Style::default()).unwrap(),
        );
    }
    let each_node = dom.root().children().next().unwrap().id();

    let prev = EachExpansion::default();
    let top = expand_each_nodes(
        &dom,
        &store,
        &mut taffy,
        &node_to_taffy,
        &prev,
        None,
        0.0,
        800.0,
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap();
    let top_start = top.window_start.get(&each_node).copied().unwrap_or(999);
    assert_eq!(top_start, 0, "scrolled to the top, window starts at 0");

    let scrolled = expand_each_nodes(
        &dom,
        &store,
        &mut taffy,
        &node_to_taffy,
        &top,
        None,
        5000.0,
        800.0,
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap();
    let scrolled_start = scrolled.window_start.get(&each_node).copied().unwrap_or(0);
    assert!(
        scrolled_start > top_start,
        "scrolling down must move the window's start index forward, got {scrolled_start}"
    );
}

#[test]
fn repeated_expansion_no_arena_growth() {
    let mut interner = StringInterner::new();
    let dom = parse_layout("doc\n    each x in items\n        box\n", &mut interner).unwrap();
    let store = setup_test_store(vec![Value::Bool(true); 10]);
    let mut taffy = TaffyTree::new();
    let mut node_to_taffy = HashMap::new();
    for node in dom.nodes() {
        node_to_taffy.insert(
            node.id(),
            taffy.new_leaf(taffy::style::Style::default()).unwrap(),
        );
    }

    let mut expansion = EachExpansion::default();
    let mut base_node_count = 0;

    for i in 0..5 {
        expansion = expand_each_nodes(
            &dom,
            &store,
            &mut taffy,
            &node_to_taffy,
            &expansion,
            None,
            0.0,
            800.0,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        let total_nodes = taffy.total_node_count();
        if i == 0 {
            base_node_count = total_nodes;
        } else {
            assert_eq!(
                total_nodes, base_node_count,
                "Taffy arena should not grow across repeated expansions"
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// Bidi/RTL logical properties + flex mirroring (ux-7)
// ────────────────────────────────────────────────────────────────────────

#[test]
fn margin_inline_start_resolves_left_under_ltr_right_under_rtl() {
    let vp = ViewportSize {
        width: 800.0,
        height: 600.0,
    };
    let mut rules = StyleRules::default();
    rules.margin_inline_start = Some(crate::parser::MizuDimension::Pixels(10.0));
    rules.margin_inline_end = Some(crate::parser::MizuDimension::Pixels(20.0));

    let ltr_style = translate_style(&rules, vp, ResolvedDirection::Ltr);
    assert_eq!(
        ltr_style.margin.left,
        taffy::style::LengthPercentageAuto::Length(10.0),
        "margin-inline-start must resolve to the left edge under LTR"
    );
    assert_eq!(
        ltr_style.margin.right,
        taffy::style::LengthPercentageAuto::Length(20.0),
        "margin-inline-end must resolve to the right edge under LTR"
    );

    let rtl_style = translate_style(&rules, vp, ResolvedDirection::Rtl);
    assert_eq!(
        rtl_style.margin.right,
        taffy::style::LengthPercentageAuto::Length(10.0),
        "margin-inline-start must resolve to the right edge under RTL"
    );
    assert_eq!(
        rtl_style.margin.left,
        taffy::style::LengthPercentageAuto::Length(20.0),
        "margin-inline-end must resolve to the left edge under RTL"
    );
}

#[test]
fn padding_inline_start_resolves_left_under_ltr_right_under_rtl() {
    let vp = ViewportSize {
        width: 800.0,
        height: 600.0,
    };
    let mut rules = StyleRules::default();
    rules.padding_inline_start = Some(crate::parser::MizuDimension::Pixels(5.0));

    let ltr_style = translate_style(&rules, vp, ResolvedDirection::Ltr);
    assert_eq!(
        ltr_style.padding.left,
        taffy::style::LengthPercentage::Length(5.0)
    );
    assert_eq!(
        ltr_style.padding.right,
        taffy::style::LengthPercentage::Length(0.0)
    );

    let rtl_style = translate_style(&rules, vp, ResolvedDirection::Rtl);
    assert_eq!(
        rtl_style.padding.right,
        taffy::style::LengthPercentage::Length(5.0)
    );
    assert_eq!(
        rtl_style.padding.left,
        taffy::style::LengthPercentage::Length(0.0)
    );
}

#[test]
fn logical_inline_properties_override_only_their_own_side_of_uniform_margin() {
    // A uniform `margin` sets all four sides; `margin-inline-start`
    // overrides only the resolved-left side, leaving top/bottom (and
    // the untouched inline-end side) at the uniform value.
    let vp = ViewportSize {
        width: 800.0,
        height: 600.0,
    };
    let mut rules = StyleRules::default();
    rules.margin = Some(crate::parser::MizuDimension::Pixels(8.0));
    rules.margin_inline_start = Some(crate::parser::MizuDimension::Pixels(30.0));

    let style = translate_style(&rules, vp, ResolvedDirection::Ltr);
    assert_eq!(
        style.margin.left,
        taffy::style::LengthPercentageAuto::Length(30.0)
    );
    assert_eq!(
        style.margin.right,
        taffy::style::LengthPercentageAuto::Length(8.0)
    );
    assert_eq!(
        style.margin.top,
        taffy::style::LengthPercentageAuto::Length(8.0)
    );
    assert_eq!(
        style.margin.bottom,
        taffy::style::LengthPercentageAuto::Length(8.0)
    );
}

#[test]
fn flex_row_mirrors_to_row_reverse_under_rtl() {
    let vp = ViewportSize {
        width: 800.0,
        height: 600.0,
    };
    let mut rules = StyleRules::default();
    rules.flex_direction = Some(FlexDirection::Row);

    let ltr_style = translate_style(&rules, vp, ResolvedDirection::Ltr);
    assert_eq!(ltr_style.flex_direction, FlexDirection::Row);

    let rtl_style = translate_style(&rules, vp, ResolvedDirection::Rtl);
    assert_eq!(
        rtl_style.flex_direction,
        FlexDirection::RowReverse,
        "a row container must mirror to RowReverse under resolved RTL"
    );
}

#[test]
fn flex_column_is_unaffected_by_rtl() {
    let vp = ViewportSize {
        width: 800.0,
        height: 600.0,
    };
    let mut rules = StyleRules::default();
    rules.flex_direction = Some(FlexDirection::Column);

    let rtl_style = translate_style(&rules, vp, ResolvedDirection::Rtl);
    assert_eq!(
        rtl_style.flex_direction,
        FlexDirection::Column,
        "column is a vertical axis and must not mirror under RTL"
    );
}

#[test]
fn build_taffy_tree_resolves_dir_attribute_inheritance_for_mirroring() {
    // End-to-end: a `dir="rtl"` layout attribute on a node reaches
    // build_taffy_tree's flex-direction mirroring, via resolve_direction's
    // ancestor walk — not just translate_style's unit-level behavior.
    let mut interner = StringInterner::new();
    let dom = parse_layout("doc dir=rtl\n    box class row\n", &mut interner).unwrap();
    let style = "    .row\n        flex-direction row\n";
    let (style_rules, variants) = parse_style_with_variants(style).unwrap();
    let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());
    let env = RenderEnvironment {
        viewport: ViewportSize {
            width: 800.0,
            height: 600.0,
        },
        color_scheme: ColorScheme::Dark,
    };

    let mut taffy = TaffyTree::new();
    let mut node_map = HashMap::new();
    build_taffy_tree(
        dom.root(),
        &mut TaffyBuildContext {
            style_rules_map: &style_rules,
            taffy: &mut taffy,
            node_to_taffy_id: &mut node_map,
            image_cache: &mut image_cache,
            chrome_url: "mizu://test/index.mizu",
            variants: &variants,
            env: &env,
        },
    )
    .unwrap();

    let row_node_id = dom.root().children().next().unwrap().id();
    let row_taffy_id = *node_map.get(&row_node_id).unwrap();
    let resolved_style = taffy.style(row_taffy_id).unwrap();
    assert_eq!(
        resolved_style.flex_direction,
        FlexDirection::RowReverse,
        "the root doc's dir=\"rtl\" must inherit down to the row box and mirror it"
    );
}

#[test]
fn id_selector_wins_over_class_which_wins_over_tag() {
    let style = r"
box
    width 100
.card
    width 200
#hero
    width 300
";
    let (style_rules, variants) = parse_style_with_variants(style).unwrap();
    let mut interner = StringInterner::new();
    let dom = parse_layout(
        "doc\n    box class card id hero\n    box class card\n    box\n",
        &mut interner,
    )
    .unwrap();
    let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());
    let env = RenderEnvironment {
        viewport: ViewportSize {
            width: 800.0,
            height: 600.0,
        },
        color_scheme: ColorScheme::Dark,
    };

    let mut taffy = TaffyTree::new();
    let mut node_map = HashMap::new();
    build_taffy_tree(
        dom.root(),
        &mut TaffyBuildContext {
            style_rules_map: &style_rules,
            taffy: &mut taffy,
            node_to_taffy_id: &mut node_map,
            image_cache: &mut image_cache,
            chrome_url: "mizu://test/index.mizu",
            variants: &variants,
            env: &env,
        },
    )
    .unwrap();

    let mut children = dom.root().children();
    let id_and_class_box = children.next().unwrap().id();
    let class_only_box = children.next().unwrap().id();
    let tag_only_box = children.next().unwrap().id();

    let width_of = |id: EgoNodeId| {
        let taffy_id = *node_map.get(&id).unwrap();
        taffy.style(taffy_id).unwrap().size.width
    };

    assert_eq!(
        width_of(id_and_class_box),
        taffy::style::Dimension::Length(300.0),
        "an id selector must win over a class selector on the same node"
    );
    assert_eq!(
        width_of(class_only_box),
        taffy::style::Dimension::Length(200.0),
        "a class selector must win over a tag selector on the same node"
    );
    assert_eq!(
        width_of(tag_only_box),
        taffy::style::Dimension::Length(100.0),
        "with neither class nor id, the tag selector applies"
    );
}
