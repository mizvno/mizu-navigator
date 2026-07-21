//! Form submission and clipboard/text-extraction helpers.

use rustc_hash::FxHashMap;
use std::collections::HashMap;

use ego_tree::NodeId as EgoNodeId;

use crate::core::errors::MizuError;
use crate::core::types::VariableStore;
use crate::network::UiEvent;

use super::manager::MizuWindowManager;

/// Maximum number of bytes a single input field accepts from typing or
/// pasting.  Prevents unbounded memory growth from key-repeat or a huge paste.
///
/// An unmeasured starting value, overridable for a single run via
/// `MIZU_INPUT_MAX_BYTES` (see the module doc on [`crate::core::config`]).
static INPUT_MAX_BYTES: std::sync::LazyLock<usize> =
    std::sync::LazyLock::new(|| crate::core::config::env_override("MIZU_INPUT_MAX_BYTES", 4096));

/// Appends the printable characters of `text` to `buf`, respecting
/// [`INPUT_MAX_BYTES`].  Control characters are dropped.  Returns `true` if at
/// least one character was appended.
pub(super) fn push_input_text(buf: &mut String, text: &str) -> bool {
    let mut changed = false;
    for c in text.chars().filter(|c| !c.is_control()) {
        if buf.len() + c.len_utf8() > *INPUT_MAX_BYTES {
            break;
        }
        buf.push(c);
        changed = true;
    }
    changed
}

/// Finds the nearest `form` ancestor of `node` (including `node` itself).
fn find_form_ancestor(
    dom: &ego_tree::Tree<crate::parser::MizuNode>,
    node: EgoNodeId,
) -> Option<EgoNodeId> {
    let mut cur = dom.get(node)?;
    loop {
        if cur.value().primitive == crate::parser::Primitive::Form {
            return Some(cur.id());
        }
        cur = cur.parent()?;
    }
}

/// Collects `name` → typed-text pairs from every `input` descendant of the
/// form containing `member` (a submit button or an input inside the form).
///
/// Values come from `local_inputs` (the live text buffers); inputs the user
/// never touched submit an empty string.  Returns `None` when `member` is not
/// inside any `form` node.
fn collect_form_fields(
    dom: &ego_tree::Tree<crate::parser::MizuNode>,
    node_id_to_u32: &HashMap<EgoNodeId, u32>,
    local_inputs: &FxHashMap<u32, String>,
    member: EgoNodeId,
) -> Option<FxHashMap<String, crate::core::types::Value>> {
    let form_id = find_form_ancestor(dom, member)?;
    let form = dom.get(form_id)?;
    let mut fields = FxHashMap::default();
    for desc in form.descendants() {
        let v = desc.value();
        if v.primitive == crate::parser::Primitive::Input
            && let Some(name) = v.attributes.get("name")
        {
            let text = node_id_to_u32
                .get(&desc.id())
                .and_then(|u| local_inputs.get(u))
                .cloned()
                .unwrap_or_default();
            fields.insert(name.clone(), crate::core::types::Value::from(text));
        }
    }
    Some(fields)
}

/// Returns the first node inside `member`'s enclosing form that carries a
/// `submit -> …` event (the form's submit button).  Used to submit on Enter.
pub(super) fn find_form_submitter(
    dom: &ego_tree::Tree<crate::parser::MizuNode>,
    member: EgoNodeId,
) -> Option<EgoNodeId> {
    let form_id = find_form_ancestor(dom, member)?;
    let form = dom.get(form_id)?;
    form.descendants()
        .find(|d| d.value().events.contains_key("submit"))
        .map(|d| d.id())
}

/// Dispatches a click gesture for `node_id` — the exact user-gesture
/// sequence the mouse click handler uses (`has_user_gesture = true`, then a
/// single `UiEvent::Click`). Shared by the mouse click handler and keyboard
/// activation (Enter/Space) so the two are observationally identical: same
/// gesture flag, same single event. Returns `true` if dispatched (`node_id`
/// must have a u32 mapping, which every live DOM node has).
pub(super) fn dispatch_click_gesture(manager: &mut MizuWindowManager, node_id: EgoNodeId) -> bool {
    let Some(&u32_id) = manager.node_id_to_u32.get(&node_id) else {
        return false;
    };
    if let Some(node_ref) = manager.dom.get(node_id) {
        manager.inspector_log.push_event(
            crate::render::inspector::log::EventKind::Click,
            crate::render::inspector::model::node_label(node_ref.value(), None),
        );
    }
    // Mark user gesture before dispatching — clipboard actions in this
    // response batch are therefore authorised.
    manager.has_user_gesture = true;
    let _ = manager.logic_tx.send(UiEvent::Click { node_id: u32_id });
    true
}

/// Dispatches a form submission triggered by `submitter` (a node carrying a
/// `submit` event): gathers the enclosing form's fields from the live input
/// buffers and forwards them to the logic worker together with the
/// submitter's id.  Returns `true` when the submission was dispatched.
pub(super) fn dispatch_form_submit(manager: &mut MizuWindowManager, submitter: EgoNodeId) -> bool {
    let Some(&submitter_u32) = manager.node_id_to_u32.get(&submitter) else {
        return false;
    };
    let Some(fields) = collect_form_fields(
        &manager.dom,
        &manager.node_id_to_u32,
        &manager.local_inputs,
        submitter,
    ) else {
        tracing::warn!("submit event outside any form node; ignored");
        return false;
    };
    manager.has_user_gesture = true;
    manager.inspector_log.push_event(
        crate::render::inspector::log::EventKind::Submit,
        format!("form submit ({} fields)", fields.len()),
    );
    let _ = manager.logic_tx.send(UiEvent::SubmitForm {
        submitter_node_id: submitter_u32,
        fields,
    });
    true
}
/// Extracts the text content of the DOM node identified by `node_id_str`.
///
/// For `Input` nodes the live locally-typed value is returned; for all other
/// nodes the `content` attribute (with variable interpolation) is used.
/// Returns [`MizuError::ExecutionError`] when no node with the given `id`
/// attribute exists in the tree.
pub(crate) fn extract_node_text(
    node_id_str: &str,
    dom: &ego_tree::Tree<crate::parser::MizuNode>,
    local_inputs: &FxHashMap<u32, String>,
    node_id_to_u32: &HashMap<EgoNodeId, u32>,
    store: &VariableStore,
) -> Result<String, MizuError> {
    for node_ref in dom.nodes() {
        let val = node_ref.value();
        if val.attributes.get("id").map(String::as_str) == Some(node_id_str) {
            let ego_id = node_ref.id();
            if val.primitive == crate::parser::Primitive::Input {
                if let Some(&u32_id) = node_id_to_u32.get(&ego_id)
                    && let Some(text) = local_inputs.get(&u32_id)
                {
                    return Ok(text.clone());
                }
                return Ok(String::new());
            }
            let content = val
                .attributes
                .get("content")
                .map(String::as_str)
                .unwrap_or("");
            return store.interpolate(content);
        }
    }
    Err(MizuError::ExecutionError(format!(
        "copy_to_clipboard: no DOM node with id={node_id_str:?}"
    )))
}

/// Copies the text content of the DOM node identified by `node_id_str` —
/// but only when `has_user_gesture` is `true`.
///
/// Returns the text that would be written to the clipboard on success, or an
/// error:
/// * [`MizuError::SecurityViolation`] when `has_user_gesture` is `false`
///   (no qualifying click preceded this call).
/// * [`MizuError::ExecutionError`] when the target DOM node does not exist.
pub(crate) fn apply_clipboard_action(
    node_id_str: &str,
    dom: &ego_tree::Tree<crate::parser::MizuNode>,
    local_inputs: &FxHashMap<u32, String>,
    node_id_to_u32: &HashMap<EgoNodeId, u32>,
    store: &VariableStore,
    has_user_gesture: bool,
) -> Result<String, MizuError> {
    if !has_user_gesture {
        return Err(MizuError::SecurityViolation(
            "copy_to_clipboard requires a user gesture (click)".to_string(),
        ));
    }
    extract_node_text(node_id_str, dom, local_inputs, node_id_to_u32, store)
}
