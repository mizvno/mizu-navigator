//! Tests for `input.rs`: clipboard copy (gesture-gated), click-gesture
//! dispatch, and `type "file"` inputs (native picker mocked at the
//! `apply_file_selection` seam — no real OS dialog).

use super::*;

// --- Clipboard security tests -------------------------------------------

#[test]
fn test_clipboard_local_origin_stealth_copy_blocked() {
    // A document (local or remote) must not copy to clipboard without a
    // qualifying user gesture — stealth exfiltration via background timers
    // is the primary threat for file:// origins.
    let tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: {
            let mut m = FxHashMap::default();
            m.insert("id".to_string(), "sensitive-data".to_string());
            m.insert("content".to_string(), "local secret".to_string());
            m
        },
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });
    let store = crate::core::types::VariableStore::new().freeze();
    // No user gesture (has_user_gesture = false) — must be blocked.
    let result = apply_clipboard_action(
        "sensitive-data",
        &tree,
        &FxHashMap::default(),
        &HashMap::new(),
        &store,
        false,
    );
    assert!(
        matches!(
            result,
            Err(crate::core::errors::MizuError::SecurityViolation(_))
        ),
        "stealth clipboard copy (no gesture) must be blocked with SecurityViolation: {result:?}"
    );
}

#[test]
fn test_clipboard_copy_without_user_gesture_fails() {
    let tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: {
            let mut m = FxHashMap::default();
            m.insert("id".to_string(), "my-node".to_string());
            m.insert("content".to_string(), "Copy me!".to_string());
            m
        },
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });
    let store = VariableStore::new().freeze();
    let result = apply_clipboard_action(
        "my-node",
        &tree,
        &FxHashMap::default(),
        &HashMap::new(),
        &store,
        false,
    );
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "clipboard must be blocked without a user gesture, got: {result:?}"
    );
}

#[test]
fn test_clipboard_arbitrary_text_injection_rejected() {
    // The builtin only accepts a DOM node id — a non-existent id must fail
    // even when a gesture is present (no arbitrary text can be injected).
    let tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: FxHashMap::default(),
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });
    let store = VariableStore::new().freeze();
    let result = apply_clipboard_action(
        "nonexistent-id",
        &tree,
        &FxHashMap::default(),
        &HashMap::new(),
        &store,
        true,
    );
    assert!(
        matches!(result, Err(MizuError::ExecutionError(_))),
        "must fail when the target node does not exist: {result:?}"
    );
}

#[test]
fn test_clipboard_extracts_text_node_content() {
    let tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: {
            let mut m = FxHashMap::default();
            m.insert("id".to_string(), "label".to_string());
            m.insert("content".to_string(), "Copy me!".to_string());
            m
        },
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });
    let store = VariableStore::new().freeze();
    let text = apply_clipboard_action(
        "label",
        &tree,
        &FxHashMap::default(),
        &HashMap::new(),
        &store,
        true,
    )
    .expect("clipboard copy with gesture must succeed");
    assert_eq!(text, "Copy me!");
}

#[test]
fn dispatch_click_gesture_emits_exactly_one_click_event() {
    // Security regression (MNT ux-1 guardrail): keyboard activation of a
    // focused button must reuse the exact mouse-click gesture sequence —
    // exactly one `UiEvent::Click` for that node, no more, no less. The
    // keyboard Enter/Space handler in event_loop.rs calls this same
    // `dispatch_click_gesture` helper, so pinning its behavior here pins
    // keyboard activation as well.
    //
    // The `Click` variant is now the whole of the gesture: the logic worker
    // stamps `WorkerResponse::gesture` from the event variant, so emitting
    // this event is exactly what grants agency — and emitting it twice, or
    // for the wrong node, would grant it twice or to the wrong handler.
    // There is no separate ambient flag left to assert on.
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

    let button_id = manager
        .active_mut()
        .dom
        .root_mut()
        .append(button_node())
        .id();
    manager.active_mut().rebuild_node_mappings();

    // Replace the real logic channel with a test channel so the emitted
    // UiEvent can be observed directly.
    let (test_tx, test_rx) = std::sync::mpsc::channel();
    manager.logic_tx = test_tx;

    let dispatched = {
        let (t, c) = manager.split_active();
        dispatch_click_gesture(t, c.logic_tx, button_id)
    };
    assert!(dispatched, "dispatch must succeed for a live DOM node");

    let events: Vec<_> = test_rx.try_iter().collect();
    assert_eq!(
        events.len(),
        1,
        "exactly one UiEvent must be emitted, got: {events:?}"
    );
    match &events[0] {
        (_, crate::network::UiEvent::Click { node_id }) => {
            let expected_u32 = *manager.active_mut().node_id_to_u32.get(&button_id).unwrap();
            assert_eq!(*node_id, expected_u32);
        }
        other => panic!("expected UiEvent::Click, got: {other:?}"),
    }
}

// --- `type "file"` inputs: native picker + $form (mocked — no real OS dialog) ---

#[test]
fn parse_accept_extensions_splits_and_strips_dots_and_mime_patterns() {
    let extensions = parse_accept_extensions(".png, .jpg,image/*, gif ");
    assert_eq!(
        extensions,
        vec!["png".to_string(), "jpg".to_string(), "gif".to_string()]
    );
}

#[test]
fn selecting_a_file_yields_filehandle_in_submit_form_fields() {
    // Mocks the `rfd` call: `apply_file_selection` takes the already-
    // picked path directly rather than invoking a real OS dialog, which
    // is exactly the seam `pick_file_path`/`apply_file_selection`'s
    // split exists to provide.
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

    let form_id = manager.active_mut().dom.root_mut().append(form_node()).id();
    let file_input_id;
    let submit_id;
    {
        let mut form_ref = manager.active_mut().dom.get_mut(form_id).unwrap();
        file_input_id = form_ref
            .append(file_input_node("avatar", Some(".png,.jpg")))
            .id();
        submit_id = form_ref.append(submit_button_node()).id();
    }
    manager.active_mut().rebuild_node_mappings();

    let (test_tx, test_rx) = std::sync::mpsc::channel();
    manager.logic_tx = test_tx;

    let file_u32 = manager.active_mut().node_id_to_u32[&file_input_id];
    apply_file_selection(
        manager.active_mut(),
        file_u32,
        Some(std::path::PathBuf::from("/home/user/pictures/cat.png")),
    );

    assert!({
        let (t, c) = manager.split_active();
        dispatch_form_submit(t, c.logic_tx, submit_id)
    });

    match test_rx.try_recv() {
        Ok((_, crate::network::UiEvent::SubmitForm { fields, .. })) => match fields.get("avatar") {
            Some(crate::core::types::Value::FileHandle(handle)) => {
                assert_eq!(handle.filename, "cat.png");
            }
            other => panic!("expected Value::FileHandle for `avatar`, got {other:?}"),
        },
        other => panic!("expected UiEvent::SubmitForm, got {other:?}"),
    }
}

#[test]
fn cancelling_the_file_dialog_leaves_the_field_null() {
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

    let form_id = manager.active_mut().dom.root_mut().append(form_node()).id();
    let file_input_id;
    let submit_id;
    {
        let mut form_ref = manager.active_mut().dom.get_mut(form_id).unwrap();
        file_input_id = form_ref.append(file_input_node("avatar", None)).id();
        submit_id = form_ref.append(submit_button_node()).id();
    }
    manager.active_mut().rebuild_node_mappings();

    let (test_tx, test_rx) = std::sync::mpsc::channel();
    manager.logic_tx = test_tx;

    // Cancelling the dialog: `apply_file_selection` receives `None`.
    let file_u32 = manager.active_mut().node_id_to_u32[&file_input_id];
    apply_file_selection(manager.active_mut(), file_u32, None);

    assert!({
        let (t, c) = manager.split_active();
        dispatch_form_submit(t, c.logic_tx, submit_id)
    });

    match test_rx.try_recv() {
        Ok((_, crate::network::UiEvent::SubmitForm { fields, .. })) => {
            assert!(
                fields.get("avatar").is_some_and(|v| v
                    .budget_eq(&crate::core::types::Value::Null, &mut u64::MAX, u64::MAX)
                    .unwrap_or(false)),
                "an unselected/cancelled file field must submit Null, not an error"
            );
        }
        other => panic!("expected UiEvent::SubmitForm, got {other:?}"),
    }
}

#[test]
fn file_input_click_focuses_no_text_caret() {
    // A `type "file"` input must never take the plain text-caret focus —
    // clicking it routes to the native picker instead (see
    // `dispatch_dom_click`'s `is_file_input` branch).
    let tree = Tree::new(window_node());
    let (mut manager, _keepalive) = make_manager_with(tree, HashMap::new());
    let file_input_id = manager
        .active_mut()
        .dom
        .root_mut()
        .append(file_input_node("avatar", None))
        .id();
    manager.active_mut().rebuild_node_mappings();

    assert!(is_file_input(&manager.active().dom, file_input_id));
    let root_id = manager.active().dom.root().id();
    assert!(!is_file_input(&manager.active().dom, root_id));
}
