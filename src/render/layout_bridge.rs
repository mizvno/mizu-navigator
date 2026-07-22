#![forbid(unsafe_code)]

use crate::core::errors::MizuError;
use crate::core::types::{Value, VariableStore};
use crate::parser::style::StyleVariant;
use crate::parser::{MizuNode, MizuOverflow, Primitive, StyleRules};
use crate::render::image_codec::AssetSlot;
use crate::render::responsive::{RenderEnvironment, ResolvedDimension, ViewportSize, resolve_dimension, resolve_matching_variants};
use ego_tree::{NodeId as EgoNodeId, NodeRef, Tree};
use std::collections::HashMap;
use taffy::{
    TaffyTree,
    geometry::Size,
    style::{Overflow, Style},
};


/// Mapping from template DOM node IDs to their per-iteration synthetic Taffy
/// node IDs.  `paint_each` installs this as a temporary override so that
/// `paint_node` reads Taffy-computed coordinates from the expanded tree
/// rather than from the stale single-template node.
pub type EachIterationOverrides = HashMap<EgoNodeId, taffy::prelude::NodeId>;

/// Global budget for synthetic layout nodes (L1 invariant).
///
/// L1 — No unmetered work proportional to remote data. Any subsystem that
/// performs O(data) allocation or CPU work must draw from an explicit,
/// named budget. This constant sits at the same order as MAX_INSTRUCTIONS
/// so the expression cliff and the layout cliff coincide.
///
/// An unmeasured starting value, overridable for a single run via
/// `MIZU_MAX_SYNTHETIC_LAYOUT_NODES` (see the module doc on
/// [`crate::core::config`]).
pub static MAX_SYNTHETIC_LAYOUT_NODES: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
    crate::core::config::env_override("MIZU_MAX_SYNTHETIC_LAYOUT_NODES", 20_000)
});

/// One entry per list element: `(row_container_taffy_id, override_map)`.
pub type EachGroupEntries = Vec<(taffy::prelude::NodeId, EachIterationOverrides)>;

/// All synthetic Taffy nodes produced during one [`expand_each_nodes`] call.
///
/// Stored in `MizuWindowManager` and rebuilt every time `resize_viewport` runs
/// so that changes to a list variable are reflected in layout on the next frame.
#[derive(Default)]
pub struct EachExpansion {
    /// `each_dom_id → [(row_taffy_id, {template_dom_id → synth_taffy_id})]`
    pub groups: HashMap<EgoNodeId, EachGroupEntries>,
    /// Snapshot of the original Taffy children of each `Each` node, taken
    /// before expansion.  Used to restore the tree before re-expanding.
    pub original_children: HashMap<EgoNodeId, Vec<taffy::prelude::NodeId>>,
    /// Every synthetic Taffy node created, collected for bulk removal on the
    /// next call (prevents arena growth on each frame).
    pub all_synthetic_ids: Vec<taffy::prelude::NodeId>,
    /// Number of hidden list items per `Each` node due to budget truncation.
    pub truncated: HashMap<EgoNodeId, usize>,
}


/// Expands every `Each` node in the DOM into N synthetic Taffy subtrees
/// (one per list element) so that `taffy.compute_layout` sees the full
/// N-row tree and produces correct per-item positions.
///
/// **Must** be called before `compute_layout` / `compute_layout_with_measure`
/// so that Taffy computes the expanded positions.
///
/// `prev` is the expansion from the previous frame; its synthetic nodes are
/// restored / removed before the new expansion is built.
pub fn expand_each_nodes(
    dom: &Tree<MizuNode>,
    store: &VariableStore,
    taffy: &mut TaffyTree<EgoNodeId>,
    node_to_taffy_id: &HashMap<EgoNodeId, taffy::prelude::NodeId>,
    prev: &EachExpansion,
) -> Result<EachExpansion, MizuError> {
    // ── Step 1: restore the previous expansion ────────────────────────────
    // Put the original template nodes back as Each's Taffy children, then
    // free every synthetic node from the arena.
    for (&each_dom_id, orig_children) in &prev.original_children {
        if let Some(&each_taffy_id) = node_to_taffy_id.get(&each_dom_id) {
            let _ = taffy.set_children(each_taffy_id, orig_children);
        }
    }
    for &synth_id in &prev.all_synthetic_ids {
        let _ = taffy.remove(synth_id);
    }

    // ── Step 2: build the new expansion ───────────────────────────────────
    let mut expansion = EachExpansion::default();
    let mut remaining_budget = *MAX_SYNTHETIC_LAYOUT_NODES;

    // Collect Each-node metadata without holding tree borrows.
    let each_nodes: Vec<(EgoNodeId, String)> = dom
        .nodes()
        .filter_map(|node_ref| {
            let v = node_ref.value();
            if v.primitive != Primitive::Each {
                return None;
            }
            let (_, list_name) = v.iterator_context.as_ref()?;
            Some((node_ref.id(), list_name.clone()))
        })
        .collect();

    for (each_dom_id, list_name) in each_nodes {
        let n = match store.get(&list_name).ok() {
            Some(Value::List(arc)) => arc.len(),
            _ => continue, // list not yet in store — leave this Each as-is
        };
        if n == 0 {
            continue;
        }

        let each_taffy_id = match node_to_taffy_id.get(&each_dom_id) {
            Some(&id) => id,
            None => continue,
        };

        // DOM children of this Each are the template nodes.
        let template_dom_children: Vec<EgoNodeId> = dom
            .get(each_dom_id)
            .map(|n| n.children().map(|c| c.id()).collect())
            .unwrap_or_default();

        if template_dom_children.is_empty() {
            continue;
        }

        let mut template_size = 0;
        for &tmpl_dom_id in &template_dom_children {
            template_size += count_dom_subtree_size(dom, tmpl_dom_id);
        }
        
        let budget_per_row = template_size + 1; // +1 for the row container
        let max_rows = remaining_budget / budget_per_row;
        let clamped_n = n.min(max_rows);

        if clamped_n < n {
            expansion.truncated.insert(each_dom_id, n - clamped_n);
        }
        remaining_budget -= clamped_n * budget_per_row;

        // Save the original Taffy children for restoration next frame.
        let orig_taffy_children: Vec<taffy::prelude::NodeId> = template_dom_children
            .iter()
            .filter_map(|dom_id| node_to_taffy_id.get(dom_id).copied())
            .collect();
        expansion
            .original_children
            .insert(each_dom_id, orig_taffy_children);

        // Build N iteration groups, each containing a clone of the template subtree.
        let mut groups: EachGroupEntries = Vec::with_capacity(clamped_n);

        for _ in 0..clamped_n {
            let mut overrides: EachIterationOverrides = HashMap::new();
            let mut row_children: Vec<taffy::prelude::NodeId> = Vec::new();

            for &tmpl_dom_id in &template_dom_children {
                if let Some(tmpl_node) = dom.get(tmpl_dom_id) {
                    let synth_id = clone_taffy_subtree(
                        tmpl_node,
                        taffy,
                        node_to_taffy_id,
                        &mut overrides,
                        &mut expansion.all_synthetic_ids,
                    )?;
                    row_children.push(synth_id);
                }
            }

            // Row container: a transparent, non-shrinking flex column that
            // wraps the synthetic template copies for this iteration.
            let row_style = taffy::style::Style {
                flex_shrink: 0.0,
                ..taffy::style::Style::default()
            };
            let row_id = taffy
                .new_with_children(row_style, &row_children)
                .map_err(|e| MizuError::ParseError(format!("Each row container: {e}")))?;

            expansion.all_synthetic_ids.push(row_id);
            groups.push((row_id, overrides));
        }

        // Replace the Each's Taffy children with the N row containers.
        let row_ids: Vec<taffy::prelude::NodeId> = groups.iter().map(|(id, _)| *id).collect();
        taffy
            .set_children(each_taffy_id, &row_ids)
            .map_err(|e| MizuError::ParseError(format!("Each set_children: {e}")))?;

        // Ensure the Each container is a Flex column so rows stack vertically
        // regardless of the display mode the single-template style specified.
        if let Ok(mut style) = taffy.style(each_taffy_id).cloned() {
            style.display = taffy::style::Display::Flex;
            style.flex_direction = taffy::style::FlexDirection::Column;
            let _ = taffy.set_style(each_taffy_id, style);
        }

        expansion.groups.insert(each_dom_id, groups);
    }

    Ok(expansion)
}

/// Helper to count the total nodes in a DOM subtree.
fn count_dom_subtree_size(dom: &Tree<MizuNode>, root: EgoNodeId) -> usize {
    let mut count = 1;
    if let Some(node) = dom.get(root) {
        for child in node.children() {
            count += count_dom_subtree_size(dom, child.id());
        }
    }
    count
}

/// Recursively clones the Taffy style-tree rooted at `dom_node` into fresh
/// synthetic Taffy nodes, preserving every node's style.
///
/// Leaf nodes (no DOM children) are created with `new_leaf_with_context` so
/// that `compute_layout_with_measure`'s measure closure receives the original
/// DOM node ID and can compute intrinsic text dimensions correctly.
///
/// `out_overrides` is extended with `(template_dom_id → synthetic_taffy_id)`
/// for every node in the cloned subtree.
fn clone_taffy_subtree(
    dom_node: NodeRef<MizuNode>,
    taffy: &mut TaffyTree<EgoNodeId>,
    node_to_taffy_id: &HashMap<EgoNodeId, taffy::prelude::NodeId>,
    out_overrides: &mut EachIterationOverrides,
    all_synthetic_ids: &mut Vec<taffy::prelude::NodeId>,
) -> Result<taffy::prelude::NodeId, MizuError> {
    let dom_id = dom_node.id();

    // Clone the style of the original Taffy node (if mapped).
    // `.cloned()` copies the Style before taffy is borrowed mutably below.
    let style: taffy::style::Style = node_to_taffy_id
        .get(&dom_id)
        .and_then(|&t_id| taffy.style(t_id).ok())
        .cloned()
        .unwrap_or_default();

    // Recurse children first (bottom-up) so parent containers can reference
    // already-created child IDs.
    let mut child_taffy_ids: Vec<taffy::prelude::NodeId> = Vec::new();
    for child in dom_node.children() {
        let child_synth_id = clone_taffy_subtree(
            child,
            taffy,
            node_to_taffy_id,
            out_overrides,
            all_synthetic_ids,
        )?;
        child_taffy_ids.push(child_synth_id);
    }

    let synth_id = if child_taffy_ids.is_empty() {
        // Leaf: carry the DOM node's context for text measurement.
        taffy
            .new_leaf_with_context(style, dom_id)
            .map_err(|e| MizuError::ParseError(format!("clone leaf: {e}")))?
    } else {
        taffy
            .new_with_children(style, &child_taffy_ids)
            .map_err(|e| MizuError::ParseError(format!("clone container: {e}")))?
    };

    all_synthetic_ids.push(synth_id);
    out_overrides.insert(dom_id, synth_id);
    Ok(synth_id)
}

/// Converts a resolved dimension into a Taffy `Dimension`.
fn to_taffy_dimension(resolved: ResolvedDimension) -> taffy::style::Dimension {
    match resolved {
        ResolvedDimension::Pixels(px) => taffy::style::Dimension::Length(px),
        ResolvedDimension::Percent(pct) => taffy::style::Dimension::Percent(pct / 100.0),
    }
}

/// Converts a resolved dimension into a Taffy `LengthPercentage` (padding/gap).
fn to_taffy_length_percentage(resolved: ResolvedDimension) -> taffy::style::LengthPercentage {
    match resolved {
        ResolvedDimension::Pixels(px) => taffy::style::LengthPercentage::Length(px),
        ResolvedDimension::Percent(pct) => taffy::style::LengthPercentage::Percent(pct / 100.0),
    }
}

/// Converts a resolved dimension into a Taffy `LengthPercentageAuto` (margin).
fn to_taffy_length_percentage_auto(
    resolved: ResolvedDimension,
) -> taffy::style::LengthPercentageAuto {
    match resolved {
        ResolvedDimension::Pixels(px) => taffy::style::LengthPercentageAuto::Length(px),
        ResolvedDimension::Percent(pct) => taffy::style::LengthPercentageAuto::Percent(pct / 100.0),
    }
}

/// Translates Mizu custom StyleRules into Native Taffy styles.
/// Converts percentage values (0.0 to 100.0) into fractions (0.0 to 1.0).
/// `viewport` resolves any `vw`/`vh`/`vmin`/`vmax` dimensions (ux-6) against
/// the current content viewport before handing off to Taffy, which only
/// ever sees pixels or (parent-relative) percent.
pub fn translate_style(rules: &StyleRules, viewport: ViewportSize) -> Style {
    let mut style = Style::default();

    // 1. width / height
    if let Some(dim) = &rules.width {
        style.size.width = to_taffy_dimension(resolve_dimension(dim, viewport));
    }
    if let Some(dim) = &rules.height {
        style.size.height = to_taffy_dimension(resolve_dimension(dim, viewport));
    }

    // 2. padding
    if let Some(dim) = &rules.padding {
        let taffy_val = to_taffy_length_percentage(resolve_dimension(dim, viewport));
        style.padding = taffy::geometry::Rect {
            left: taffy_val,
            right: taffy_val,
            top: taffy_val,
            bottom: taffy_val,
        };
    }

    // 3. margin
    if let Some(dim) = &rules.margin {
        let taffy_val = to_taffy_length_percentage_auto(resolve_dimension(dim, viewport));
        style.margin = taffy::geometry::Rect {
            left: taffy_val,
            right: taffy_val,
            top: taffy_val,
            bottom: taffy_val,
        };
    }

    // 4. gap
    if let Some(dim) = &rules.gap {
        let taffy_val = to_taffy_length_percentage(resolve_dimension(dim, viewport));
        style.gap = taffy::geometry::Size {
            width: taffy_val,
            height: taffy_val,
        };
    }

    // 5. flex properties
    if let Some(dir) = rules.direction {
        style.flex_direction = dir;
    }
    if let Some(justify) = rules.justify {
        style.justify_content = Some(justify);
    }
    if let Some(align) = rules.align {
        style.align_items = Some(align);
    }

    // 7. border
    if let Some(border_width) = rules.border_width {
        let taffy_val = taffy::style::LengthPercentage::Length(border_width);
        style.border = taffy::geometry::Rect {
            left: taffy_val,
            right: taffy_val,
            top: taffy_val,
            bottom: taffy_val,
        };
    }

    // 8. overflow — maps MizuOverflow to Taffy's Point<Overflow> (x and y axis).
    let taffy_overflow = match rules.overflow {
        MizuOverflow::Visible => Overflow::Visible,
        MizuOverflow::Hidden => Overflow::Hidden,
        MizuOverflow::Scroll => Overflow::Scroll,
    };
    style.overflow = taffy::geometry::Point {
        x: taffy_overflow,
        y: taffy_overflow,
    };

    // 9. display — overrides Taffy display mode when explicitly set.
    if let Some(display) = rules.display {
        style.display = display;
    }

    style
}

/// Recursively traverses the DOM tree bottom-up to build the Taffy tree layout.
///
/// `variants`/`env` resolve ux-6 breakpoint/color-scheme style variants —
/// pass `&[]` and a default `RenderEnvironment` for callers that don't need
/// responsive behavior (e.g. tests).
#[allow(clippy::too_many_arguments)]
pub fn build_taffy_tree(
    node: NodeRef<MizuNode>,
    style_rules_map: &HashMap<String, StyleRules>,
    taffy: &mut TaffyTree<EgoNodeId>,
    node_to_taffy_id: &mut HashMap<EgoNodeId, taffy::prelude::NodeId>,
    image_cache: &HashMap<String, AssetSlot>,
    chrome_url: &str,
    variants: &[StyleVariant],
    env: &RenderEnvironment,
) -> Result<taffy::prelude::NodeId, MizuError> {
    let mut children_ids = Vec::new();
    for child in node.children() {
        let child_id = build_taffy_tree(
            child,
            style_rules_map,
            taffy,
            node_to_taffy_id,
            image_cache,
            chrome_url,
            variants,
            env,
        )?;
        children_ids.push(child_id);
    }

    let mizu_node = node.value();
    let mut merged_rules = StyleRules::default();

    // 1. Tag styles
    let tag_name = mizu_node.primitive.as_str();
    if let Some(tag_rules) = style_rules_map.get(tag_name) {
        merged_rules = merged_rules.merge(tag_rules.clone());
    }

    // 2. Class styles
    let class_attr = mizu_node.attributes.get("class").map(String::as_str);
    if let Some(class_attr) = class_attr
        && let Some(class_rules) = style_rules_map.get(class_attr)
    {
        merged_rules = merged_rules.merge(class_rules.clone());
    }

    // 3. Breakpoint / color-scheme variants (ux-6) — applied last, after both
    // bases, in source declaration order (see docs/design/responsive.md).
    let selectors: &[&str] = match class_attr {
        Some(c) => &[tag_name, c],
        None => &[tag_name],
    };
    merged_rules = merged_rules.merge(resolve_matching_variants(variants, selectors, env));

    let mut style = translate_style(&merged_rules, env.viewport);

    if mizu_node.primitive == Primitive::Window {
        style.size = Size {
            width: taffy::style::Dimension::Percent(1.0),
            height: taffy::style::Dimension::Percent(1.0),
        };
    } else if mizu_node.primitive == Primitive::Button {
        style.flex_shrink = 0.0;
        style.overflow = taffy::geometry::Point {
            x: taffy::style::Overflow::Hidden,
            y: taffy::style::Overflow::Hidden,
        };
    } else if mizu_node.primitive == Primitive::Box {
        style.flex_shrink = 1.0;
        style.overflow = taffy::geometry::Point {
            x: taffy::style::Overflow::Hidden,
            y: taffy::style::Overflow::Hidden,
        };
    } else if mizu_node.primitive == Primitive::Image
        && let Some(src) = mizu_node.attributes.get("src")
    {
        let abs_url = if src.starts_with("mizu://") {
            src.clone()
        } else if let Ok(base_uri) = crate::network::uri::MizuUri::parse(chrome_url) {
            let path = if src.starts_with('/') {
                src.clone()
            } else {
                format!("/{}", src)
            };
            format!("mizu://{}{}", base_uri.domain, path)
        } else {
            src.clone()
        };

        let mut intr_width = None;
        let mut intr_height = None;

        if let Some(AssetSlot::Ready(cached)) = image_cache.get(&abs_url) {
            intr_width = Some(cached.width() as f32);
            intr_height = Some(cached.height() as f32);
        }

        if let (Some(w), Some(h)) = (intr_width, intr_height) {
            style.aspect_ratio = Some(w / h);
            // Only apply intrinsic pixel dimensions when *neither* axis has
            // been set by the stylesheet.  If the user specified one axis
            // (e.g. `width 400`) the aspect_ratio alone is sufficient for
            // Taffy to derive the other — overwriting it here would break
            // proportional scaling.
            if style.size.width == taffy::style::Dimension::Auto
                && style.size.height == taffy::style::Dimension::Auto
            {
                style.size.width = taffy::style::Dimension::Length(w);
                style.size.height = taffy::style::Dimension::Length(h);
            }
        }
    }

    let taffy_id = if children_ids.is_empty() {
        taffy
            .new_leaf_with_context(style, node.id())
            .map_err(|e| MizuError::ParseError(format!("Failed to create Taffy node: {e}")))?
    } else {
        taffy
            .new_with_children(style, &children_ids)
            .map_err(|e| MizuError::ParseError(format!("Failed to create Taffy node: {e}")))?
    };

    node_to_taffy_id.insert(node.id(), taffy_id);
    Ok(taffy_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{Value, VariableStore, StringInterner};
    use crate::parser::layout::parse_layout;
    use crate::parser::style::parse_style_with_variants;
    use crate::render::preferences::ColorScheme;
    use std::sync::Arc;

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
        direction column
";
        let (style_rules, variants) = parse_style_with_variants(style).unwrap();
        assert_eq!(variants.len(), 1, "fixture must define exactly one variant");

        let mut interner = StringInterner::new();
        let dom = parse_layout(
            "window\n    box class box\n        box class box\n        box class box\n",
            &mut interner,
        )
        .unwrap();
        let image_cache = HashMap::new();

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
            &style_rules,
            &mut narrow_taffy,
            &mut narrow_map,
            &image_cache,
            "mizu://test/index.mizu",
            &variants,
            &narrow_env,
        )
        .unwrap();

        let mut wide_taffy = TaffyTree::new();
        let mut wide_map = HashMap::new();
        build_taffy_tree(
            dom.root(),
            &style_rules,
            &mut wide_taffy,
            &mut wide_map,
            &image_cache,
            "mizu://test/index.mizu",
            &variants,
            &wide_env,
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
        let mut store = VariableStore::with_interner(interner);
        store.set("items", Value::List(Arc::new(items)));
        store
    }

    #[test]
    fn each_small_list_unaffected_by_budget() {
        let mut interner = StringInterner::new();
        let dom = parse_layout("window\n    each x in items\n        box\n", &mut interner).unwrap();
        let store = setup_test_store(vec![Value::Bool(true); 5]);
        let mut taffy = TaffyTree::new();
        let mut node_to_taffy = HashMap::new();
        for node in dom.nodes() {
            node_to_taffy.insert(node.id(), taffy.new_leaf(taffy::style::Style::default()).unwrap());
        }
        
        let prev = EachExpansion::default();
        let expansion = expand_each_nodes(&dom, &store, &mut taffy, &node_to_taffy, &prev).unwrap();
        
        assert!(expansion.truncated.is_empty(), "Small list should not be truncated");
        let each_node = dom.root().children().next().unwrap().id();
        assert_eq!(expansion.groups.get(&each_node).unwrap().len(), 5);
    }
    
    #[test]
    fn each_huge_list_clamped_to_budget() {
        let mut interner = StringInterner::new();
        let dom = parse_layout("window\n    each x in items\n        box\n", &mut interner).unwrap();
        let store = setup_test_store(vec![Value::Bool(true); *MAX_SYNTHETIC_LAYOUT_NODES + 100]);
        let mut taffy = TaffyTree::new();
        let mut node_to_taffy = HashMap::new();
        for node in dom.nodes() {
            node_to_taffy.insert(node.id(), taffy.new_leaf(taffy::style::Style::default()).unwrap());
        }
        
        let prev = EachExpansion::default();
        let expansion = expand_each_nodes(&dom, &store, &mut taffy, &node_to_taffy, &prev).unwrap();
        
        let each_node = dom.root().children().next().unwrap().id();
        let truncated = expansion.truncated.get(&each_node).copied().unwrap_or(0);
        assert!(truncated > 0, "Huge list must be truncated");
        assert_eq!(expansion.groups.get(&each_node).unwrap().len() + truncated, *MAX_SYNTHETIC_LAYOUT_NODES + 100);
    }

    #[test]
    fn repeated_expansion_no_arena_growth() {
        let mut interner = StringInterner::new();
        let dom = parse_layout("window\n    each x in items\n        box\n", &mut interner).unwrap();
        let store = setup_test_store(vec![Value::Bool(true); 10]);
        let mut taffy = TaffyTree::new();
        let mut node_to_taffy = HashMap::new();
        for node in dom.nodes() {
            node_to_taffy.insert(node.id(), taffy.new_leaf(taffy::style::Style::default()).unwrap());
        }
        
        let mut expansion = EachExpansion::default();
        let mut base_node_count = 0;
        
        for i in 0..5 {
            expansion = expand_each_nodes(&dom, &store, &mut taffy, &node_to_taffy, &expansion).unwrap();
            let total_nodes = taffy.total_node_count();
            if i == 0 {
                base_node_count = total_nodes;
            } else {
                assert_eq!(total_nodes, base_node_count, "Taffy arena should not grow across repeated expansions");
            }
        }
    }
}
