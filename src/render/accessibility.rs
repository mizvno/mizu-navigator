//! Accessibility tree: a read-only view of the DOM derived for assistive
//! technology (AT), via [`accesskit`].
//!
//! ## Security posture
//!
//! This module only *reads* DOM/store state to build an
//! [`accesskit::TreeUpdate`] — the same posture as the Inspector (F12 panel)
//! and storage invariant S1 (write-only from the document's side). Accessible
//! names come from the same interpolated content the renderer paints
//! ([`crate::core::types::VariableStore::interpolate`]), so AT never learns a
//! value the document couldn't already display. The one action channel this
//! module does wire (`Action::Default` / `Action::Focus`, handled in
//! `render::window::event_loop`) routes through the exact same gesture-gated
//! dispatch keyboard activation uses (ux-1's `dispatch_click_gesture` /
//! `dispatch_form_submit`) — an AT-initiated activation is a real user
//! gesture, not a second, ungated path into the evaluator.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use accesskit::{Node as AccessNode, NodeBuilder, NodeClassSet, NodeId as AccessNodeId, Role, Tree as AccessTree, TreeUpdate};
use ego_tree::{NodeId as EgoNodeId, Tree};

use crate::core::types::VariableStore;
use crate::parser::{MizuNode, Primitive};

/// Wraps an `accesskit_winit::Event` so it can travel through winit's
/// `Event::UserEvent` channel alongside Mizu's own window events.
#[derive(Debug)]
pub enum MizuUserEvent {
    /// An accessibility event (initial-tree request, AT-initiated action, or
    /// deactivation) delivered by `accesskit_winit::Adapter`.
    Accesskit(accesskit_winit::Event),
}

impl From<accesskit_winit::Event> for MizuUserEvent {
    fn from(event: accesskit_winit::Event) -> Self {
        MizuUserEvent::Accesskit(event)
    }
}

/// Converts a stable DOM node u32 id (see
/// `crate::render::window::MizuWindowManager::node_id_to_u32`) into an
/// `accesskit::NodeId`. Offset by 1 so id 0 — reserved by some platform
/// accessibility APIs to mean "no node" — is never assigned to a real node.
fn access_id(u32_id: u32) -> AccessNodeId {
    AccessNodeId(u32_id as u64 + 1)
}

/// Inverse of [`access_id`]: resolves an `accesskit::NodeId` received in an
/// AT action request back to the DOM node it names, via the manager's
/// `u32_to_node_id` reverse map. `None` if the id is stale (e.g. the
/// document reloaded since the AT last queried the tree).
pub(crate) fn resolve_ego_id(
    u32_to_node_id: &HashMap<u32, EgoNodeId>,
    id: AccessNodeId,
) -> Option<EgoNodeId> {
    let u32_id = u32::try_from(id.0.checked_sub(1)?).ok()?;
    u32_to_node_id.get(&u32_id).copied()
}

/// Maps a Mizu layout primitive to its accesskit role. `Each` (a list
/// template, not a visible primitive of its own) is exposed as a plain
/// container, matching `Box`.
fn role_for(primitive: Primitive) -> Role {
    match primitive {
        Primitive::Window => Role::Window,
        Primitive::Box | Primitive::Each => Role::GenericContainer,
        Primitive::Text | Primitive::Markdown => Role::StaticText,
        Primitive::Button => Role::Button,
        Primitive::Input => Role::TextInput,
        Primitive::Image => Role::Image,
        Primitive::Form => Role::Form,
    }
}

/// Builds a full `accesskit::TreeUpdate` from the current DOM.
///
/// This is a full rebuild (not an incremental patch) on every call — kept
/// small and pure so it can be called from the `RedrawRequested` handler,
/// piggybacking on the renderer's own frame coalescing rather than needing a
/// separate debounce mechanism.
///
/// Accessible names ("what AT hears") are drawn from the same attributes and
/// interpolated content the renderer paints ("what the eye sees"): `alt` for
/// `Image` (absent → no name, so AT announces an unlabeled image rather than
/// silence), interpolated `content` for `Text`/`Markdown`/`Button`, and the
/// literal `placeholder` for `Input`.
pub fn build_a11y_tree(
    dom: &Tree<MizuNode>,
    node_id_to_u32: &HashMap<EgoNodeId, u32>,
    focused_node: Option<EgoNodeId>,
    store: &VariableStore,
) -> TreeUpdate {
    let mut classes = NodeClassSet::new();
    let mut nodes: Vec<(AccessNodeId, AccessNode)> = Vec::new();

    let root_ego_id = dom.root().id();
    let root_id = node_id_to_u32
        .get(&root_ego_id)
        .copied()
        .map(access_id)
        .unwrap_or(AccessNodeId(1));

    build_node(root_ego_id, dom, node_id_to_u32, store, &mut classes, &mut nodes);

    let focus = focused_node
        .and_then(|id| node_id_to_u32.get(&id))
        .copied()
        .map(access_id)
        .unwrap_or(root_id);

    TreeUpdate {
        nodes,
        tree: Some(AccessTree::new(root_id)),
        focus,
    }
}

/// Recursively builds one `accesskit::Node` per DOM node (pre-order) and
/// appends it to `out`; children are linked by id, mirroring the DOM's own
/// parent/child structure exactly.
fn build_node(
    ego_id: EgoNodeId,
    dom: &Tree<MizuNode>,
    node_id_to_u32: &HashMap<EgoNodeId, u32>,
    store: &VariableStore,
    classes: &mut NodeClassSet,
    out: &mut Vec<(AccessNodeId, AccessNode)>,
) {
    let Some(node_ref) = dom.get(ego_id) else {
        return;
    };
    let Some(&u32_id) = node_id_to_u32.get(&ego_id) else {
        return;
    };
    let mizu_node = node_ref.value();
    let this_id = access_id(u32_id);

    let mut builder = NodeBuilder::new(role_for(mizu_node.primitive));

    match mizu_node.primitive {
        Primitive::Image => {
            // Where the dead `alt` attribute finally gets consumed. No
            // `alt` → no name, deliberately: an unlabeled image is exposed
            // (role still present), not hidden or silently skipped.
            if let Some(alt) = mizu_node.attributes.get("alt")
                && !alt.is_empty()
            {
                builder.set_name(alt.clone());
            }
        }
        Primitive::Text | Primitive::Markdown | Primitive::Button => {
            if let Some(content) = mizu_node.attributes.get("content") {
                let name = store.interpolate(content).unwrap_or_default();
                if !name.is_empty() {
                    builder.set_name(name);
                }
            }
        }
        Primitive::Input => {
            if let Some(placeholder) = mizu_node.attributes.get("placeholder")
                && !placeholder.is_empty()
            {
                builder.set_name(placeholder.clone());
            }
        }
        _ => {}
    }

    // Mirrors ux-1's `is_focusable` predicate exactly, so anything Tab can
    // reach is also anything AT can act on — one definition, not a second
    // notion of "interactive" that could drift from the keyboard model.
    if crate::render::window::is_focusable(mizu_node) {
        builder.add_action(accesskit::Action::Focus);
        if mizu_node.primitive == Primitive::Button
            || mizu_node.events.contains_key("click")
            || mizu_node.events.contains_key("submit")
        {
            builder.add_action(accesskit::Action::Default);
        }
    }

    let mut child_ids = Vec::new();
    for child in node_ref.children() {
        build_node(child.id(), dom, node_id_to_u32, store, classes, out);
        if let Some(&child_u32) = node_id_to_u32.get(&child.id()) {
            child_ids.push(access_id(child_u32));
        }
    }
    builder.set_children(child_ids);

    out.push((this_id, builder.build(classes)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    fn node(primitive: Primitive, attrs: &[(&str, &str)]) -> MizuNode {
        let mut attributes = StdHashMap::new();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v.to_string());
        }
        MizuNode {
            primitive,
            attributes,
            events: StdHashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        }
    }

    /// window -> [button(content="Save"), input(placeholder="email"),
    ///            image(alt="logo"), image(no alt)]
    fn build_fixture() -> (
        Tree<MizuNode>,
        HashMap<EgoNodeId, u32>,
        EgoNodeId, // button
        EgoNodeId, // input
        EgoNodeId, // labeled image
        EgoNodeId, // bare image
    ) {
        let mut tree = Tree::new(node(Primitive::Window, &[]));
        let button_id = tree
            .root_mut()
            .append(node(Primitive::Button, &[("content", "Save")]))
            .id();
        let input_id = tree
            .root_mut()
            .append(node(Primitive::Input, &[("placeholder", "email")]))
            .id();
        let labeled_image_id = tree
            .root_mut()
            .append(node(Primitive::Image, &[("alt", "logo")]))
            .id();
        let bare_image_id = tree.root_mut().append(node(Primitive::Image, &[])).id();

        let mut node_id_to_u32 = HashMap::new();
        let mut next = 0u32;
        for n in tree.nodes() {
            node_id_to_u32.insert(n.id(), next);
            next += 1;
        }

        (
            tree,
            node_id_to_u32,
            button_id,
            input_id,
            labeled_image_id,
            bare_image_id,
        )
    }

    #[test]
    fn roles_and_names_match_the_fixture() {
        let (tree, node_id_to_u32, button_id, input_id, labeled_image_id, bare_image_id) =
            build_fixture();
        let store = VariableStore::new();

        let update = build_a11y_tree(&tree, &node_id_to_u32, None, &store);
        let by_id: HashMap<AccessNodeId, &AccessNode> = update.nodes.iter().map(|(id, n)| (*id, n)).collect();

        let button_node = by_id[&access_id(node_id_to_u32[&button_id])];
        assert_eq!(button_node.role(), Role::Button);
        assert_eq!(button_node.name().as_deref(), Some("Save"));

        let input_node = by_id[&access_id(node_id_to_u32[&input_id])];
        assert_eq!(input_node.role(), Role::TextInput);
        assert_eq!(input_node.name().as_deref(), Some("email"));

        let labeled_image_node = by_id[&access_id(node_id_to_u32[&labeled_image_id])];
        assert_eq!(labeled_image_node.role(), Role::Image);
        assert_eq!(
            labeled_image_node.name().as_deref(),
            Some("logo"),
            "alt-bearing image must expose its alt text as the accessible name"
        );

        let bare_image_node = by_id[&access_id(node_id_to_u32[&bare_image_id])];
        assert_eq!(
            bare_image_node.role(),
            Role::Image,
            "an image with no alt is still exposed (flagged as unlabeled), not silently omitted"
        );
        assert_eq!(
            bare_image_node.name().as_deref(),
            None,
            "an image with no alt must expose no accessible name"
        );
    }

    #[test]
    fn removing_alt_regresses_the_name_to_empty() {
        // Regression pin: `alt` must never become dead code again. If this
        // starts failing, something stopped reading the `alt` attribute.
        let mut tree = Tree::new(node(Primitive::Window, &[]));
        let with_alt = tree
            .root_mut()
            .append(node(Primitive::Image, &[("alt", "a cat")]))
            .id();
        let mut node_id_to_u32 = HashMap::new();
        for (i, n) in tree.nodes().enumerate() {
            node_id_to_u32.insert(n.id(), i as u32);
        }
        let store = VariableStore::new();
        let update = build_a11y_tree(&tree, &node_id_to_u32, None, &store);
        let named = update
            .nodes
            .iter()
            .find(|(id, _)| *id == access_id(node_id_to_u32[&with_alt]))
            .map(|(_, n)| n.name())
            .flatten();
        assert_eq!(named.as_deref(), Some("a cat"));

        // Now the same image, minus `alt`.
        let mut tree2 = Tree::new(node(Primitive::Window, &[]));
        let without_alt = tree2.root_mut().append(node(Primitive::Image, &[])).id();
        let mut node_id_to_u32_2 = HashMap::new();
        for (i, n) in tree2.nodes().enumerate() {
            node_id_to_u32_2.insert(n.id(), i as u32);
        }
        let update2 = build_a11y_tree(&tree2, &node_id_to_u32_2, None, &store);
        let unnamed = update2
            .nodes
            .iter()
            .find(|(id, _)| *id == access_id(node_id_to_u32_2[&without_alt]))
            .map(|(_, n)| n.name())
            .flatten();
        assert_eq!(unnamed, None, "removing alt must clear the accessible name");
    }

    #[test]
    fn focus_in_tree_update_tracks_focused_node() {
        let (tree, node_id_to_u32, button_id, input_id, ..) = build_fixture();
        let store = VariableStore::new();

        let update = build_a11y_tree(&tree, &node_id_to_u32, Some(input_id), &store);
        assert_eq!(update.focus, access_id(node_id_to_u32[&input_id]));

        let update = build_a11y_tree(&tree, &node_id_to_u32, Some(button_id), &store);
        assert_eq!(update.focus, access_id(node_id_to_u32[&button_id]));

        // Nothing focused: falls back to the root, never an unset/dangling id.
        let update = build_a11y_tree(&tree, &node_id_to_u32, None, &store);
        assert_eq!(
            update.focus,
            access_id(node_id_to_u32[&tree.root().id()])
        );
    }

    #[test]
    fn resolve_ego_id_round_trips_through_access_id() {
        let (_tree, node_id_to_u32, button_id, ..) = build_fixture();
        let mut u32_to_node_id = HashMap::new();
        for (&ego, &u32_id) in &node_id_to_u32 {
            u32_to_node_id.insert(u32_id, ego);
        }
        let ak_id = access_id(node_id_to_u32[&button_id]);
        assert_eq!(resolve_ego_id(&u32_to_node_id, ak_id), Some(button_id));

        // A stale/unknown id must resolve to None, not panic or alias another node.
        assert_eq!(resolve_ego_id(&u32_to_node_id, AccessNodeId(999_999)), None);
    }
}
