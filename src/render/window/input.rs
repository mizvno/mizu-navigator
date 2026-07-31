//! Form submission and clipboard/text-extraction helpers.

use rustc_hash::FxHashMap;
use std::collections::HashMap;

use ego_tree::NodeId as EgoNodeId;

use crate::core::errors::MizuError;
use crate::core::types::VariableStore;
use crate::network::{TabId, UiEvent};

use super::manager::TabState;

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
pub(super) fn collect_form_fields(
    dom: &ego_tree::Tree<crate::parser::MizuNode>,
    node_id_to_u32: &HashMap<EgoNodeId, u32>,
    local_inputs: &FxHashMap<u32, String>,
    local_file_selections: &FxHashMap<u32, std::sync::Arc<crate::core::types::FileHandleData>>,
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
            let is_file_input = v.attributes.get("type").map(String::as_str) == Some("file");
            let value = if is_file_input {
                // A `type "file"` field carries a `FileHandle` exactly like
                // an ordinary field carries a `String` — same `$form` shape,
                // no code change needed downstream. An untouched/cancelled
                // file input submits `Null`, not an error.
                node_id_to_u32
                    .get(&desc.id())
                    .and_then(|u| local_file_selections.get(u))
                    .map(|handle| crate::core::types::Value::FileHandle(handle.clone()))
                    .unwrap_or(crate::core::types::Value::Null)
            } else {
                let text = node_id_to_u32
                    .get(&desc.id())
                    .and_then(|u| local_inputs.get(u))
                    .cloned()
                    .unwrap_or_default();
                crate::core::types::Value::from(text)
            };
            fields.insert(name.clone(), value);
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

/// Returns `true` if `node_id` is a `type "file"` input.
pub(super) fn is_file_input(
    dom: &ego_tree::Tree<crate::parser::MizuNode>,
    node_id: EgoNodeId,
) -> bool {
    dom.get(node_id)
        .map(|n| n.value().attributes.get("type").map(String::as_str) == Some("file"))
        .unwrap_or(false)
}

/// Parses an `accept "…"` attribute value into bare extensions (no leading
/// `.`) suitable for `rfd::FileDialog::add_filter`.
///
/// This is a convenience filter only, **not a security boundary** — most
/// native file dialogs still let the user type an arbitrary filename past
/// it. MIME-pattern tokens (containing `/`, e.g. `image/*`) aren't
/// expressible as an `rfd` extension filter and are silently skipped rather
/// than rejected; the real gates are the outbound MIME-by-extension-table
/// (never sniffed) and the request size budget applied when the file is
/// actually uploaded.
pub(super) fn parse_accept_extensions(accept: &str) -> Vec<String> {
    accept
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.contains('/'))
        .map(|s| s.trim_start_matches('.').to_string())
        .collect()
}

/// Opens the native file-picker dialog (blocking — invoked directly from the
/// click handler, so it runs within the same user-gesture context as the
/// click itself). Returns `None` if the user cancels.
///
/// Not unit-tested directly (would require a real OS dialog); the testable
/// surface is [`apply_file_selection`], which takes the picked path (or
/// `None`) as a plain argument.
fn pick_file_path(accept: Option<&str>) -> Option<std::path::PathBuf> {
    let mut dialog = rfd::FileDialog::new();
    if let Some(accept) = accept {
        let extensions = parse_accept_extensions(accept);
        if !extensions.is_empty() {
            let ext_refs: Vec<&str> = extensions.iter().map(String::as_str).collect();
            dialog = dialog.add_filter("Accepted files", &ext_refs);
        }
    }
    dialog.pick_file()
}

/// Records a picked file path (or clears the field on cancellation) for
/// `u32_id`. Pure w.r.t. its inputs — takes the already-picked path rather
/// than calling `rfd` itself, so this is the unit-testable half of file
/// selection (see [`pick_file_path`]'s doc comment).
///
/// Only path/filename metadata is stored — never the file's bytes (see
/// [`crate::core::types::FileHandleData`]).
pub(super) fn apply_file_selection(
    tab: &mut TabState,
    u32_id: u32,
    picked: Option<std::path::PathBuf>,
) {
    let Some(path) = picked else {
        // Cancelled: leave the field unset (Null on submit), not an error.
        tab.local_file_selections.remove(&u32_id);
        return;
    };
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    tab.local_file_selections.insert(
        u32_id,
        std::sync::Arc::new(crate::core::types::FileHandleData { path, filename }),
    );
}

/// Dispatches a click on a `type "file"` input: opens the native picker
/// (the OS dialog itself is the user-gesture gate here, exactly like G1 for
/// navigation — it can only be triggered from a real click) and records the
/// selection. Returns `true` if `node_id` resolved to a live node (matching
/// the other `dispatch_*` helpers' convention), regardless of whether the
/// user picked a file or cancelled.
pub(super) fn dispatch_file_input_click(tab: &mut TabState, node_id: EgoNodeId) -> bool {
    let Some(&u32_id) = tab.node_id_to_u32.get(&node_id) else {
        return false;
    };
    let accept = tab
        .dom
        .get(node_id)
        .and_then(|n| n.value().attributes.get("accept").cloned());
    let picked = pick_file_path(accept.as_deref());
    apply_file_selection(tab, u32_id, picked);
    true
}

/// Dispatches a click gesture for `node_id` — a single `UiEvent::Click`.
/// Shared by the mouse click handler and keyboard activation (Enter/Space) so
/// the two are observationally identical: the same single event. Returns
/// `true` if dispatched (`node_id` must have a u32 mapping, which every live
/// DOM node has).
///
/// This function and [`dispatch_form_submit`] are the *only* emitters of
/// `UiEvent::Click`/`UiEvent::SubmitForm`, which is what lets the logic
/// worker treat those two variants as user agency and stamp
/// [`crate::network::WorkerResponse::gesture`] on the resulting batch (gate
/// G1). Emitting either variant from a non-interactive path would forge a
/// gesture; route document-driven work through `RootTimer`/`UpdateVariable`
/// instead.
pub(super) fn dispatch_click_gesture(
    tab: &mut TabState,
    logic_tx: &std::sync::mpsc::Sender<(TabId, UiEvent)>,
    node_id: EgoNodeId,
) -> bool {
    let Some(&u32_id) = tab.node_id_to_u32.get(&node_id) else {
        return false;
    };
    if let Some(node_ref) = tab.dom.get(node_id) {
        tab.inspector_log.push_event(
            crate::render::inspector::log::EventKind::Click,
            crate::render::inspector::model::node_label(node_ref.value(), None),
        );
    }
    // The `Click` variant itself carries the agency: the worker stamps
    // `gesture: true` on exactly the response batch this event produces, so
    // navigation and clipboard actions in *that* batch — and no other — are
    // authorised.
    let _ = logic_tx.send((tab.id, UiEvent::Click { node_id: u32_id }));
    true
}

/// Dispatches a form submission triggered by `submitter` (a node carrying a
/// `submit` event): gathers the enclosing form's fields from the live input
/// buffers and forwards them to the logic worker together with the
/// submitter's id.  Returns `true` when the submission was dispatched.
pub(super) fn dispatch_form_submit(
    tab: &mut TabState,
    logic_tx: &std::sync::mpsc::Sender<(TabId, UiEvent)>,
    submitter: EgoNodeId,
) -> bool {
    let Some(&submitter_u32) = tab.node_id_to_u32.get(&submitter) else {
        return false;
    };
    let Some(fields) = collect_form_fields(
        &tab.dom,
        &tab.node_id_to_u32,
        &tab.local_inputs,
        &tab.local_file_selections,
        submitter,
    ) else {
        tracing::warn!("submit event outside any form node; ignored");
        return false;
    };
    tab.inspector_log.push_event(
        crate::render::inspector::log::EventKind::Submit,
        format!("form submit ({} fields)", fields.len()),
    );
    let _ = logic_tx.send((
        tab.id,
        UiEvent::SubmitForm {
            submitter_node_id: submitter_u32,
            fields,
        },
    ));
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
