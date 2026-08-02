//! Tests for `focus.rs`: keyboard focus order (ux-1) — which nodes are
//! focusable, and Tab/Shift-Tab traversal order and wraparound.

use super::*;

#[test]
fn focusable_nodes_in_order_excludes_plain_includes_click_box() {
    // window -> [plain box, clickable box, input, button] in document order.
    let tree = Tree::new(window_node());
    let mut manager = MizuWindowManager::new(
        tree,
        HashMap::new(),
        Vec::new(),
        FxHashMap::default(),
        #[cfg(feature = "insecure-dev")]
        false,
    )
    .expect("manager created");

    let plain_id = manager
        .active_mut()
        .dom
        .root_mut()
        .append(plain_box_node())
        .id();
    let click_box_id = manager
        .active_mut()
        .dom
        .root_mut()
        .append(clickable_box_node())
        .id();
    let input_id = manager
        .active_mut()
        .dom
        .root_mut()
        .append(input_node("a"))
        .id();
    let button_id = manager
        .active_mut()
        .dom
        .root_mut()
        .append(button_node())
        .id();
    manager.active_mut().rebuild_node_mappings();

    let order = manager.active_mut().focusable_nodes_in_order();
    assert!(
        !order.contains(&plain_id),
        "a plain box with no click/submit event must not be focusable"
    );
    assert_eq!(
        order,
        vec![click_box_id, input_id, button_id],
        "focusable nodes must appear in document (pre-order) order, \
             including a non-button/input box that carries a click event"
    );
}

#[test]
fn tab_advances_and_wraps_shift_tab_reverses() {
    // window -> [input a, input b, input c]
    let tree = Tree::new(window_node());
    let mut manager = MizuWindowManager::new(
        tree,
        HashMap::new(),
        Vec::new(),
        FxHashMap::default(),
        #[cfg(feature = "insecure-dev")]
        false,
    )
    .expect("manager created");

    let a = manager
        .active_mut()
        .dom
        .root_mut()
        .append(input_node("a"))
        .id();
    let b = manager
        .active_mut()
        .dom
        .root_mut()
        .append(input_node("b"))
        .id();
    let c = manager
        .active_mut()
        .dom
        .root_mut()
        .append(input_node("c"))
        .id();
    manager.active_mut().rebuild_node_mappings();

    // Nothing focused: Tab focuses the first, Shift-Tab focuses the last.
    assert_eq!(manager.active_mut().next_focus_target(false), Some(a));
    assert_eq!(manager.active_mut().next_focus_target(true), Some(c));

    // Forward advance a -> b -> c -> wraps to a.
    manager.active_mut().focused_node = Some(a);
    assert_eq!(manager.active_mut().next_focus_target(false), Some(b));
    manager.active_mut().focused_node = Some(b);
    assert_eq!(manager.active_mut().next_focus_target(false), Some(c));
    manager.active_mut().focused_node = Some(c);
    assert_eq!(
        manager.active_mut().next_focus_target(false),
        Some(a),
        "Tab from the last focusable node must wrap to the first"
    );

    // Shift-Tab reverses: a -> wraps to c.
    manager.active_mut().focused_node = Some(a);
    assert_eq!(
        manager.active_mut().next_focus_target(true),
        Some(c),
        "Shift-Tab from the first focusable node must wrap to the last"
    );
    manager.active_mut().focused_node = Some(c);
    assert_eq!(manager.active_mut().next_focus_target(true), Some(b));
}
