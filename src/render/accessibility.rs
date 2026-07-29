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

/// Converts a DOM node u32 id (see
/// `crate::render::window::TabState::node_id_to_u32`) into an
/// `accesskit::NodeId`, within the id space of document generation `epoch`.
///
/// The low 32 bits are the node's per-document id, offset by 1 so that id 0 —
/// reserved by some platform accessibility APIs to mean "no node" — is never
/// assigned to a real node. The high 32 bits are the generation.
///
/// # Why the generation is part of the id
///
/// `node_id_to_u32` numbers nodes from zero on every document load, and the
/// accesskit adapter is one long-lived consumer shared by every tab. Without
/// the generation, loading a page or switching tabs hands the consumer the
/// *same* ids attached to entirely different nodes, and it reads that as the
/// old tree being rearranged: nodes appear to move to a new parent while
/// their old parent disappears. The consumer prunes the vanished parent's old
/// subtree, taking live nodes with it, and then unwraps a `None` looking one
/// of them back up (`accesskit_consumer` 0.18, `tree.rs:350`).
///
/// Making generations disjoint means a reload is what it actually is —
/// every old node gone, every new node new — so nothing is ever mistaken for
/// a node that used to exist.
fn access_id(epoch: u32, u32_id: u32) -> AccessNodeId {
    AccessNodeId(((epoch as u64) << 32) | (u32_id as u64 + 1))
}

/// Inverse of [`access_id`]: resolves an `accesskit::NodeId` received in an
/// AT action request back to the DOM node it names, via the tab's
/// `u32_to_node_id` reverse map.
///
/// Returns `None` when the id belongs to a different document generation than
/// `epoch` — an AT that queried the tree before a navigation must not have
/// its stale request land on whatever node inherited that slot in the new
/// document.
pub(crate) fn resolve_ego_id(
    epoch: u32,
    u32_to_node_id: &HashMap<u32, EgoNodeId>,
    id: AccessNodeId,
) -> Option<EgoNodeId> {
    if (id.0 >> 32) as u32 != epoch {
        return None;
    }
    let u32_id = u32::try_from((id.0 & 0xffff_ffff).checked_sub(1)?).ok()?;
    u32_to_node_id.get(&u32_id).copied()
}

/// Maps a Mizu layout primitive to its accesskit role. `Each` (a list
/// template, not a visible primitive of its own) is exposed as a plain
/// container, matching `Box`.
fn role_for(primitive: Primitive) -> Role {
    match primitive {
        Primitive::Doc => Role::Window,
        Primitive::Box | Primitive::Each => Role::GenericContainer,
        Primitive::Text | Primitive::Markdown => Role::StaticText,
        Primitive::Button => Role::Button,
        Primitive::Input => Role::TextInput,
        Primitive::Image => Role::Image,
        Primitive::Form => Role::Form,
        Primitive::Heading => Role::Heading,
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
    epoch: u32,
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
        .map(|u32_id| access_id(epoch, u32_id))
        .unwrap_or(AccessNodeId(1));

    build_node(epoch, root_ego_id, dom, node_id_to_u32, store, &mut classes, &mut nodes);

    let focus = focused_node
        .and_then(|id| node_id_to_u32.get(&id))
        .copied()
        .map(|u32_id| access_id(epoch, u32_id))
        .unwrap_or(root_id);

    TreeUpdate {
        nodes,
        tree: Some(AccessTree::new(root_id)),
        focus,
    }
}

/// Recursively builds one `accesskit::Node` per DOM node and appends it to
/// `out`, children before their parent; children are linked by id, mirroring
/// the DOM's own parent/child structure exactly.
fn build_node(
    epoch: u32,
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
    let this_id = access_id(epoch, u32_id);

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
        Primitive::Heading => {
            // The parser's `h1`-`h6` match arm only ever writes `"1"`-`"6"`
            // into this attribute (see `parser::layout::parse_primitive_and_attrs`),
            // so the parse always succeeds; there is no invalid-level case
            // to handle.
            if let Some(level) = mizu_node
                .attributes
                .get("level")
                .and_then(|s| s.parse::<usize>().ok())
            {
                builder.set_hierarchical_level(level);
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
        build_node(epoch, child.id(), dom, node_id_to_u32, store, classes, out);
        if let Some(&child_u32) = node_id_to_u32.get(&child.id()) {
            child_ids.push(access_id(epoch, child_u32));
        }
    }
    builder.set_children(child_ids);

    out.push((this_id, builder.build(classes)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustc_hash::FxHashMap;

    /// Arbitrary document generation; the value only has to be consistent
    /// within a test.
    const EPOCH: u32 = 7;

    fn node(primitive: Primitive, attrs: &[(&str, &str)]) -> MizuNode {
        let mut attributes = FxHashMap::default();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v.to_string());
        }
        MizuNode {
            primitive,
            attributes,
            events: FxHashMap::default(),
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
        let mut tree = Tree::new(node(Primitive::Doc, &[]));
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

        let update = build_a11y_tree(EPOCH, &tree, &node_id_to_u32, None, &store);
        let by_id: HashMap<AccessNodeId, &AccessNode> = update.nodes.iter().map(|(id, n)| (*id, n)).collect();

        let button_node = by_id[&access_id(EPOCH, node_id_to_u32[&button_id])];
        assert_eq!(button_node.role(), Role::Button);
        assert_eq!(button_node.name().as_deref(), Some("Save"));

        let input_node = by_id[&access_id(EPOCH, node_id_to_u32[&input_id])];
        assert_eq!(input_node.role(), Role::TextInput);
        assert_eq!(input_node.name().as_deref(), Some("email"));

        let labeled_image_node = by_id[&access_id(EPOCH, node_id_to_u32[&labeled_image_id])];
        assert_eq!(labeled_image_node.role(), Role::Image);
        assert_eq!(
            labeled_image_node.name().as_deref(),
            Some("logo"),
            "alt-bearing image must expose its alt text as the accessible name"
        );

        let bare_image_node = by_id[&access_id(EPOCH, node_id_to_u32[&bare_image_id])];
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
    fn heading_node_gets_heading_role_and_matching_hierarchical_level() {
        let mut tree = Tree::new(node(Primitive::Doc, &[]));
        let h1_id = tree
            .root_mut()
            .append(node(Primitive::Heading, &[("level", "1")]))
            .id();
        let h4_id = tree
            .root_mut()
            .append(node(Primitive::Heading, &[("level", "4")]))
            .id();
        let mut node_id_to_u32 = HashMap::new();
        for (i, n) in tree.nodes().enumerate() {
            node_id_to_u32.insert(n.id(), i as u32);
        }
        let store = VariableStore::new();
        let update = build_a11y_tree(EPOCH, &tree, &node_id_to_u32, None, &store);
        let by_id: HashMap<AccessNodeId, &AccessNode> = update.nodes.iter().map(|(id, n)| (*id, n)).collect();

        let h1_node = by_id[&access_id(EPOCH, node_id_to_u32[&h1_id])];
        assert_eq!(h1_node.role(), Role::Heading);
        assert_eq!(h1_node.hierarchical_level(), Some(1));

        let h4_node = by_id[&access_id(EPOCH, node_id_to_u32[&h4_id])];
        assert_eq!(h4_node.role(), Role::Heading);
        assert_eq!(h4_node.hierarchical_level(), Some(4));
    }

    #[test]
    fn removing_alt_regresses_the_name_to_empty() {
        // Regression pin: `alt` must never become dead code again. If this
        // starts failing, something stopped reading the `alt` attribute.
        let mut tree = Tree::new(node(Primitive::Doc, &[]));
        let with_alt = tree
            .root_mut()
            .append(node(Primitive::Image, &[("alt", "a cat")]))
            .id();
        let mut node_id_to_u32 = HashMap::new();
        for (i, n) in tree.nodes().enumerate() {
            node_id_to_u32.insert(n.id(), i as u32);
        }
        let store = VariableStore::new();
        let update = build_a11y_tree(EPOCH, &tree, &node_id_to_u32, None, &store);
        let named = update
            .nodes
            .iter()
            .find(|(id, _)| *id == access_id(EPOCH, node_id_to_u32[&with_alt]))
            .map(|(_, n)| n.name())
            .flatten();
        assert_eq!(named.as_deref(), Some("a cat"));

        // Now the same image, minus `alt`.
        let mut tree2 = Tree::new(node(Primitive::Doc, &[]));
        let without_alt = tree2.root_mut().append(node(Primitive::Image, &[])).id();
        let mut node_id_to_u32_2 = HashMap::new();
        for (i, n) in tree2.nodes().enumerate() {
            node_id_to_u32_2.insert(n.id(), i as u32);
        }
        let update2 = build_a11y_tree(EPOCH, &tree2, &node_id_to_u32_2, None, &store);
        let unnamed = update2
            .nodes
            .iter()
            .find(|(id, _)| *id == access_id(EPOCH, node_id_to_u32_2[&without_alt]))
            .map(|(_, n)| n.name())
            .flatten();
        assert_eq!(unnamed, None, "removing alt must clear the accessible name");
    }

    #[test]
    fn focus_in_tree_update_tracks_focused_node() {
        let (tree, node_id_to_u32, button_id, input_id, ..) = build_fixture();
        let store = VariableStore::new();

        let update = build_a11y_tree(EPOCH, &tree, &node_id_to_u32, Some(input_id), &store);
        assert_eq!(update.focus, access_id(EPOCH, node_id_to_u32[&input_id]));

        let update = build_a11y_tree(EPOCH, &tree, &node_id_to_u32, Some(button_id), &store);
        assert_eq!(update.focus, access_id(EPOCH, node_id_to_u32[&button_id]));

        // Nothing focused: falls back to the root, never an unset/dangling id.
        let update = build_a11y_tree(EPOCH, &tree, &node_id_to_u32, None, &store);
        assert_eq!(
            update.focus,
            access_id(EPOCH, node_id_to_u32[&tree.root().id()])
        );
    }

    #[test]
    fn resolve_ego_id_round_trips_through_access_id() {
        let (_tree, node_id_to_u32, button_id, ..) = build_fixture();
        let mut u32_to_node_id = HashMap::new();
        for (&ego, &u32_id) in &node_id_to_u32 {
            u32_to_node_id.insert(u32_id, ego);
        }
        let ak_id = access_id(EPOCH, node_id_to_u32[&button_id]);
        assert_eq!(resolve_ego_id(EPOCH, &u32_to_node_id, ak_id), Some(button_id));

        // An unknown id must resolve to None, not panic or alias another node.
        assert_eq!(
            resolve_ego_id(EPOCH, &u32_to_node_id, access_id(EPOCH, 999_999)),
            None
        );
        assert_eq!(resolve_ego_id(EPOCH, &u32_to_node_id, AccessNodeId(0)), None);
    }

    #[test]
    fn an_id_from_an_earlier_document_never_resolves() {
        let (_tree, node_id_to_u32, button_id, ..) = build_fixture();
        let mut u32_to_node_id = HashMap::new();
        for (&ego, &u32_id) in &node_id_to_u32 {
            u32_to_node_id.insert(u32_id, ego);
        }
        // The AT asks about a node it saw before a navigation. That slot is
        // occupied in the current document too — answering with whatever now
        // sits there would fire the action at the wrong node.
        let stale = access_id(EPOCH - 1, node_id_to_u32[&button_id]);
        assert_eq!(resolve_ego_id(EPOCH, &u32_to_node_id, stale), None);
    }

    #[test]
    fn two_document_generations_share_no_node_ids() {
        // The condition behind the `accesskit_consumer` panic: if a reloaded
        // document reuses the previous one's ids, the consumer treats its
        // nodes as the old ones moved about, prunes a subtree it should not,
        // and then unwraps a `None` (tree.rs:350).
        let (tree, node_id_to_u32, ..) = build_fixture();
        let store = VariableStore::new();

        let first = build_a11y_tree(EPOCH, &tree, &node_id_to_u32, None, &store);
        // Same DOM, same u32 numbering — only the generation differs, which
        // is exactly the case a fresh load of the same page produces.
        let second = build_a11y_tree(EPOCH + 1, &tree, &node_id_to_u32, None, &store);

        assert!(!first.nodes.is_empty(), "fixture must produce nodes");
        assert_eq!(first.nodes.len(), second.nodes.len());

        let first_ids: std::collections::HashSet<AccessNodeId> =
            first.nodes.iter().map(|(id, _)| *id).collect();
        for (id, _) in &second.nodes {
            assert!(
                !first_ids.contains(id),
                "id {id:?} is reused across document generations"
            );
        }
        assert!(!first_ids.contains(&second.focus));
    }

    /// Builds a DOM of `parents.len() + 1` nodes: node 0 is the root, and
    /// node `i + 1` is appended to node `parents[i]`.
    ///
    /// Returned alongside the same zero-based numbering
    /// `TabState::rebuild_node_mappings` produces, so a pair of these models
    /// exactly what two successive document loads hand the accessibility
    /// layer.
    fn dom_shaped(parents: &[usize]) -> (Tree<MizuNode>, HashMap<EgoNodeId, u32>) {
        let mut tree = Tree::new(node(Primitive::Doc, &[]));
        let mut ids = vec![tree.root().id()];
        for &parent in parents {
            let child = tree
                .get_mut(ids[parent])
                .expect("parent exists")
                .append(node(Primitive::Box, &[]))
                .id();
            ids.push(child);
        }
        let numbering = tree
            .nodes()
            .enumerate()
            .map(|(i, n)| (n.id(), i as u32))
            .collect();
        (tree, numbering)
    }

    /// Document shapes covering the structural moves a navigation can make:
    /// nodes appearing, disappearing, nesting deeper, and lifting closer to
    /// the root.
    const SHAPES: &[&[usize]] = &[
        &[],
        &[0],
        &[0, 0],
        &[0, 1],
        &[0, 0, 0],
        &[0, 1, 0],
        &[0, 1, 1],
        &[0, 1, 2],
        &[0, 0, 1],
        &[0, 0, 1, 1],
        &[0, 1, 1, 0],
        &[0, 1, 2, 0],
        &[0, 1, 0, 2],
    ];

    /// A `TreeChangeHandler` that records nothing: the test is about whether
    /// the consumer survives the update, not about what it would announce.
    struct IgnoreChanges;

    impl accesskit_consumer::TreeChangeHandler for IgnoreChanges {
        fn node_added(&mut self, _node: &accesskit_consumer::Node) {}
        fn node_updated(
            &mut self,
            _old_node: &accesskit_consumer::DetachedNode,
            _new_node: &accesskit_consumer::Node,
        ) {
        }
        fn focus_moved(
            &mut self,
            _old_node: Option<&accesskit_consumer::DetachedNode>,
            _new_node: Option<&accesskit_consumer::Node>,
            _current_state: &accesskit_consumer::TreeState,
        ) {
        }
        fn node_removed(
            &mut self,
            _node: &accesskit_consumer::DetachedNode,
            _current_state: &accesskit_consumer::TreeState,
        ) {
        }
    }

    #[test]
    fn no_sequence_of_documents_panics_the_accesskit_consumer() {
        // Regression test for a crash on navigation: the platform-side
        // consumer panicked while applying the update for the newly loaded
        // document (`accesskit_consumer` 0.18, tree.rs:350).
        //
        // Feeding every ordered pair of shapes through the real consumer,
        // rather than one hand-picked pair, because the trigger is not
        // obvious from the outside: it fires when a node looks like it moved
        // while its former parent vanished, and which shape transitions do
        // that depends on the consumer's internal bookkeeping order. With ids
        // reused between the two documents, 53 of the 169 pairs below panic;
        // with `access_id`'s generations making them disjoint, none do.
        let store = VariableStore::new();

        for (generation, (before, after)) in SHAPES
            .iter()
            .flat_map(|a| SHAPES.iter().map(move |b| (*a, *b)))
            .enumerate()
        {
            let (before_dom, before_ids) = dom_shaped(before);
            let (after_dom, after_ids) = dom_shaped(after);
            // Two consecutive generations, exactly as two document loads get.
            let epoch = (generation as u32) * 2 + 1;
            let first = build_a11y_tree(epoch, &before_dom, &before_ids, None, &store);
            let second = build_a11y_tree(epoch + 1, &after_dom, &after_ids, None, &store);

            // `update_and_process_changes`, not `update`: the panic lives in
            // the change-notification pass, which is the one a real platform
            // adapter runs in order to emit events to the AT.
            let mut consumer = accesskit_consumer::Tree::new(first, true);
            consumer.update_and_process_changes(second, &mut IgnoreChanges);
        }
    }

    #[test]
    fn every_node_in_an_update_is_reachable_from_its_root() {
        // The other half of the same guarantee: within one update, no node
        // may be left dangling. A node the consumer cannot reach from the
        // root is pruned, and pruning a node the update also declared is
        // what turns into the panic.
        let (tree, node_id_to_u32, ..) = build_fixture();
        let store = VariableStore::new();
        let update = build_a11y_tree(EPOCH, &tree, &node_id_to_u32, None, &store);

        let by_id: HashMap<AccessNodeId, &AccessNode> =
            update.nodes.iter().map(|(id, n)| (*id, n)).collect();
        let root = update.tree.as_ref().expect("update carries a tree").root;

        let mut reached = std::collections::HashSet::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if !reached.insert(id) {
                continue;
            }
            let node = by_id
                .get(&id)
                .unwrap_or_else(|| panic!("child {id:?} is referenced but never declared"));
            stack.extend(node.children().iter().copied());
        }
        assert_eq!(
            reached.len(),
            update.nodes.len(),
            "every declared node must hang off the root"
        );
    }
}
