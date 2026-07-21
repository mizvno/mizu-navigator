//! Keyboard focus order (Tab/Shift-Tab) and click/submit event resolution
//! shared between the mouse click handler and keyboard activation.

use ego_tree::NodeId as EgoNodeId;

use crate::parser::{MizuNode, Primitive};

use super::manager::MizuWindowManager;

/// A node is keyboard-focusable iff it is an `input`, a `button`, or any node
/// carrying a `click`/`submit` event handler. Document order is the tab
/// order — Mizu has no `tabindex`.
fn is_focusable(node: &MizuNode) -> bool {
    matches!(node.primitive, Primitive::Input | Primitive::Button)
        || node.events.contains_key("click")
        || node.events.contains_key("submit")
}

/// Walks from `start` up through its ancestors (inclusive) looking for the
/// nearest `click`/`submit` event handlers — the same ancestor walk the
/// mouse click handler performs against a hit-test result. Used to resolve
/// keyboard activation (Enter/Space) to exactly the node a mouse click at
/// the same screen position would have found.
pub(super) fn find_click_and_submit(
    dom: &ego_tree::Tree<MizuNode>,
    start: EgoNodeId,
) -> (Option<EgoNodeId>, Option<EgoNodeId>) {
    let mut action_node_id = None;
    let mut submit_node_id = None;
    let mut current = Some(start);

    while let Some(id) = current {
        let Some(node_ref) = dom.get(id) else {
            break;
        };
        if node_ref.value().events.contains_key("click") {
            action_node_id = Some(id);
        }
        if node_ref.value().events.contains_key("submit") {
            submit_node_id = Some(id);
        }
        if action_node_id.is_some() || submit_node_id.is_some() {
            break;
        }
        current = node_ref.parent().map(|p| p.id());
    }

    (action_node_id, submit_node_id)
}

impl MizuWindowManager {
    /// Collects every keyboard-focusable node in document order (pre-order
    /// DOM traversal). This *is* the tab order.
    pub fn focusable_nodes_in_order(&self) -> Vec<EgoNodeId> {
        self.dom
            .root()
            .descendants()
            .filter(|n| is_focusable(n.value()))
            .map(|n| n.id())
            .collect()
    }

    /// Computes the next (or, with `backward` set, previous) node to focus
    /// given the current `focused_node`, wrapping at the ends. Returns
    /// `None` only when the document has no focusable nodes at all.
    pub fn next_focus_target(&self, backward: bool) -> Option<EgoNodeId> {
        let order = self.focusable_nodes_in_order();
        if order.is_empty() {
            return None;
        }
        let current_idx = self
            .focused_node
            .and_then(|id| order.iter().position(|&n| n == id));
        let next_idx = match (current_idx, backward) {
            (None, false) => 0,
            (None, true) => order.len() - 1,
            (Some(i), false) => (i + 1) % order.len(),
            (Some(i), true) => (i + order.len() - 1) % order.len(),
        };
        Some(order[next_idx])
    }
}
