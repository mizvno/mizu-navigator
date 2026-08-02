//! Mouse hit-testing algorithm for layout interaction.

use ego_tree::{NodeId as EgoNodeId, Tree};
use std::collections::HashMap;
use taffy::TaffyTree;

use crate::parser::MizuNode;

struct HitTestContext<'a> {
    tree: &'a Tree<MizuNode>,
    taffy: &'a TaffyTree<EgoNodeId>,
    node_to_taffy_id: &'a HashMap<EgoNodeId, taffy::prelude::NodeId>,
    scroll_offsets: &'a HashMap<EgoNodeId, f32>,
    mouse_x: f32,
    mouse_y: f32,
}

/// Performs a hit-test to find the deepest node intersecting with the given coordinates.
pub fn hit_test(
    tree: &Tree<MizuNode>,
    taffy: &TaffyTree<EgoNodeId>,
    node_to_taffy_id: &HashMap<EgoNodeId, taffy::prelude::NodeId>,
    scroll_offsets: &HashMap<EgoNodeId, f32>,
    mouse_x: f32,
    mouse_y: f32,
) -> Option<EgoNodeId> {
    let ctx = HitTestContext {
        tree,
        taffy,
        node_to_taffy_id,
        scroll_offsets,
        mouse_x,
        mouse_y,
    };
    hit_test_node(tree.root().id(), &ctx, 0.0, 0.0)
}

fn hit_test_node(
    node_id: EgoNodeId,
    ctx: &HitTestContext<'_>,
    offset_x: f32,
    offset_y: f32,
) -> Option<EgoNodeId> {
    let mut current_offset_x = offset_x;
    let mut current_offset_y = offset_y;
    let mut width = 0.0;
    let mut height = 0.0;

    if let Some(&t_id) = ctx.node_to_taffy_id.get(&node_id)
        && let Ok(layout) = ctx.taffy.layout(t_id)
    {
        current_offset_x += layout.location.x;
        current_offset_y += layout.location.y;
        width = layout.size.width;
        height = layout.size.height;
    }

    let inside = ctx.mouse_x >= current_offset_x
        && ctx.mouse_x <= current_offset_x + width
        && ctx.mouse_y >= current_offset_y
        && ctx.mouse_y <= current_offset_y + height;

    if !inside {
        return None;
    }

    let node_ref = ctx.tree.get(node_id)?;

    // If this node is scrolled, its children are shifted UP visually.
    // So we must subtract the scroll offset from the Y coordinate passed to children.
    let scroll_y = ctx.scroll_offsets.get(&node_id).copied().unwrap_or(0.0);

    for child in node_ref.children() {
        if let Some(hit) = hit_test_node(
            child.id(),
            ctx,
            current_offset_x,
            current_offset_y - scroll_y,
        ) {
            return Some(hit);
        }
    }

    Some(node_id)
}

#[cfg(test)]
mod tests;
