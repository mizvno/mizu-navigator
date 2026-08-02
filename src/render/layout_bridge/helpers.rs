//! Small Taffy-tree helpers shared by expansion and the main build pass:
//! `new_spacer_leaf`, `count_dom_subtree_size`, `clone_taffy_subtree`, and
//! the `ResolvedDimension` → Taffy dimension/length-percentage converters.

use std::collections::HashMap;

use ego_tree::{NodeId as EgoNodeId, NodeRef, Tree};
use taffy::{TaffyTree, geometry::Size};

use crate::core::errors::MizuError;
use crate::parser::MizuNode;
use crate::render::responsive::ResolvedDimension;

use super::expansion::EachIterationOverrides;

/// Creates a fixed-height, full-width leaf node representing the combined
/// height of a run of virtualized-out (not actually expanded) `Each` rows,
/// so the container's total height stays approximately correct.
pub(super) fn new_spacer_leaf(
    taffy: &mut TaffyTree<EgoNodeId>,
    height: f32,
) -> Result<taffy::prelude::NodeId, MizuError> {
    let style = taffy::style::Style {
        flex_shrink: 0.0,
        size: Size {
            width: taffy::style::Dimension::Auto,
            height: taffy::style::Dimension::Length(height),
        },
        ..taffy::style::Style::default()
    };
    taffy
        .new_leaf(style)
        .map_err(|e| MizuError::ParseError(format!("Each spacer: {e}")))
}

/// Helper to count the total nodes in a DOM subtree.
pub(super) fn count_dom_subtree_size(dom: &Tree<MizuNode>, root: EgoNodeId) -> usize {
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
pub(super) fn clone_taffy_subtree(
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
pub(super) fn to_taffy_dimension(resolved: ResolvedDimension) -> taffy::style::Dimension {
    match resolved {
        ResolvedDimension::Pixels(px) => taffy::style::Dimension::Length(px),
        ResolvedDimension::Percent(pct) => taffy::style::Dimension::Percent(pct / 100.0),
    }
}

/// Converts a resolved dimension into a Taffy `LengthPercentage` (padding/gap).
pub(super) fn to_taffy_length_percentage(
    resolved: ResolvedDimension,
) -> taffy::style::LengthPercentage {
    match resolved {
        ResolvedDimension::Pixels(px) => taffy::style::LengthPercentage::Length(px),
        ResolvedDimension::Percent(pct) => taffy::style::LengthPercentage::Percent(pct / 100.0),
    }
}

/// Converts a resolved dimension into a Taffy `LengthPercentageAuto` (margin).
pub(super) fn to_taffy_length_percentage_auto(
    resolved: ResolvedDimension,
) -> taffy::style::LengthPercentageAuto {
    match resolved {
        ResolvedDimension::Pixels(px) => taffy::style::LengthPercentageAuto::Length(px),
        ResolvedDimension::Percent(pct) => taffy::style::LengthPercentageAuto::Percent(pct / 100.0),
    }
}
