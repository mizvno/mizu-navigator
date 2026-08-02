//! Tests for the hit_test module.

use super::*;
use crate::parser::Primitive;
use rustc_hash::FxHashMap;

#[test]
fn test_hit_test_inside() {
    let mut tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: FxHashMap::default(),
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });

    let root_id = tree.root().id();

    let child_id = tree
        .root_mut()
        .append(MizuNode {
            primitive: Primitive::Button,
            attributes: FxHashMap::default(),
            events: FxHashMap::default(),
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
