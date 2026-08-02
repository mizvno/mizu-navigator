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

use accesskit::{
    Node as AccessNode, NodeBuilder, NodeClassSet, NodeId as AccessNodeId, Role,
    Tree as AccessTree, TreeUpdate,
};
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

    build_node(
        epoch,
        root_ego_id,
        dom,
        node_id_to_u32,
        store,
        &mut classes,
        &mut nodes,
    );

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
mod tests;
