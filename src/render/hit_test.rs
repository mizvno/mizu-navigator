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
mod tests {
    use super::*;
    use crate::parser::Primitive;

    #[test]
    fn test_hit_test_inside() {
        let mut tree = Tree::new(MizuNode {
            primitive: Primitive::Window,
            attributes: HashMap::new(),
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });

        let root_id = tree.root().id();

        let child_id = tree
            .root_mut()
            .append(MizuNode {
                primitive: Primitive::Button,
                attributes: HashMap::new(),
                events: HashMap::new(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            })
            .id();

        let mut taffy = TaffyTree::<EgoNodeId>::new();
        let mut node_to_taffy_id = HashMap::new();

        let child_style = taffy::style::Style {
            size: taffy::geometry::Size {
                width: taffy::style::Dimension::Length(100.0),
                height: taffy::style::Dimension::Length(50.0),
            },
            ..Default::default()
        };
        let t_child = taffy.new_leaf_with_context(child_style, child_id).unwrap();
        node_to_taffy_id.insert(child_id, t_child);

        let root_style = taffy::style::Style {
            size: taffy::geometry::Size {
                width: taffy::style::Dimension::Length(800.0),
                height: taffy::style::Dimension::Length(600.0),
            },
            ..Default::default()
        };
        let t_root = taffy.new_with_children(root_style, &[t_child]).unwrap();
        node_to_taffy_id.insert(root_id, t_root);

        use taffy::prelude::TaffyMaxContent;
        taffy
            .compute_layout(t_root, taffy::geometry::Size::MAX_CONTENT)
            .unwrap();

        let scroll_offsets = HashMap::new();

        // Hit the root but not the child
        let hit1 = hit_test(
            &tree,
            &taffy,
            &node_to_taffy_id,
            &scroll_offsets,
            200.0,
            200.0,
        );
        assert_eq!(hit1, Some(root_id));

        // Hit the child (assuming child is placed at 0,0 since no margin/padding)
        let hit2 = hit_test(
            &tree,
            &taffy,
            &node_to_taffy_id,
            &scroll_offsets,
            50.0,
            25.0,
        );
        assert_eq!(hit2, Some(child_id));

        // Outside everything (if root was smaller, but root is 800x600, so outside is >800 or <0)
        let hit3 = hit_test(
            &tree,
            &taffy,
            &node_to_taffy_id,
            &scroll_offsets,
            -10.0,
            200.0,
        );
        assert_eq!(hit3, None);
    }
}
