    use super::input::*;
    use super::manager::*;
    use super::navigate::*;
    use crate::core::errors::MizuError;
    use crate::core::types::{StringInterner, Symbol, VariableStore};
    use crate::parser::MizuDimension;
    use crate::parser::{MizuNode, Primitive, StyleRules};
    use crate::render::chrome_vello::CHROME_HEIGHT;
    use ego_tree::Tree;
    use rustc_hash::FxHashMap;
    use std::collections::HashMap;

    #[test]
    fn test_manager_resize_viewport() {
        let tree = Tree::new(MizuNode {
            primitive: Primitive::Window,
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert("class".to_string(), "window".to_string());
                attrs
            },
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });

        let mut styles = HashMap::new();
        let root_style = StyleRules {
            width: Some(MizuDimension::Percent(100.0)),
            height: Some(MizuDimension::Percent(100.0)),
            ..Default::default()
        };
        styles.insert("window".to_string(), root_style);

        let mut manager = MizuWindowManager::new(
            tree,
            styles,
            FxHashMap::default(),
            #[cfg(feature = "insecure-dev")]
            false,
        )
        .expect("Manager created");

        manager
            .resize_viewport(800.0, 600.0)
            .expect("Initial resize ok");

        let layout = manager
            .taffy
            .layout(manager.root_taffy_id)
            .expect("Layout exists");
        assert_eq!(layout.size.width, 800.0);
        assert_eq!(layout.size.height, 600.0 - CHROME_HEIGHT);

        manager
            .resize_viewport(1024.0, 768.0)
            .expect("Second resize ok");
        let layout = manager
            .taffy
            .layout(manager.root_taffy_id)
            .expect("Layout exists");
        assert_eq!(layout.size.width, 1024.0);
        assert_eq!(layout.size.height, 768.0 - CHROME_HEIGHT);
    }

    fn make_minimal_manager() -> MizuWindowManager {
        let tree = Tree::new(MizuNode {
            primitive: Primitive::Window,
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert("class".to_string(), "window".to_string());
                attrs
            },
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });
        let mut styles = HashMap::new();
        styles.insert("window".to_string(), StyleRules::default());
        MizuWindowManager::new(
            tree,
            styles,
            FxHashMap::default(),
            #[cfg(feature = "insecure-dev")]
            false,
        )
        .expect("Manager created")
    }

    #[test]
    fn redirect_counter_allows_up_to_max_then_stops() {
        let mut manager = make_minimal_manager();
        for hop in 1..=*MAX_REDIRECTS {
            assert!(
                manager.register_redirect(),
                "redirect hop {hop} should be permitted (<= MAX_REDIRECTS)"
            );
        }
        assert!(
            !manager.register_redirect(),
            "redirect hop {} must be refused (exceeds MAX_REDIRECTS)",
            *MAX_REDIRECTS + 1
        );
    }

    #[test]
    fn redirect_counter_reset_clears_budget() {
        let mut manager = make_minimal_manager();
        for _ in 0..*MAX_REDIRECTS {
            assert!(manager.register_redirect());
        }
        assert!(
            !manager.register_redirect(),
            "budget exhausted before reset"
        );
        manager.reset_redirect_count();
        assert!(
            manager.register_redirect(),
            "after reset, a fresh navigation chain may redirect again"
        );
    }

    // --- Navigation security / URL resolution tests ----------------------------

    #[test]
    fn test_remote_origin_cannot_navigate_file() {
        let result =
            resolve_navigate_url("mizu://shop.example.com/index.mizu", "file:///etc/passwd");
        assert!(
            result.is_none(),
            "file:// navigation from mizu:// origin must be blocked"
        );
    }

    #[test]
    fn test_unknown_scheme_origin_is_not_treated_as_remote() {
        // `http://` and `https://` are not valid Mizu schemes and are rejected
        // by navigate_to_url before they can become the current URL.
        // resolve_navigate_url therefore does NOT treat them as remote origins.
        assert!(
            resolve_navigate_url("http://example.com/page", "file:///etc/hosts").is_some(),
            "http:// is not a recognised Mizu origin — file:// block does not apply"
        );
        assert!(
            resolve_navigate_url("https://example.com/page", "file:///etc/hosts").is_some(),
            "https:// is not a recognised Mizu origin — file:// block does not apply"
        );
    }

    #[test]
    fn test_relative_path_from_file_url() {
        let result = resolve_navigate_url("file:///home/user/app/index.mizu", "dettaglio.mizu");
        let url = result.expect("relative navigation from file:// must succeed");
        assert!(url.starts_with("file:///"), "must be a file:// URL: {url}");
        assert!(
            url.ends_with("dettaglio.mizu"),
            "must point to dettaglio.mizu: {url}"
        );
        assert!(
            url.contains("app"),
            "must be resolved into the same directory: {url}"
        );
    }

    #[test]
    fn test_bare_url_normalised_to_mizu() {
        let result = resolve_navigate_url("mizu://origin.com/index.mizu", "other.com/page");
        let url = result.expect("bare URL navigation must succeed");
        assert!(
            url.starts_with("mizu://"),
            "bare URL must be normalised to mizu://: {url}"
        );
    }

    #[test]
    fn test_file_origin_can_navigate_file() {
        let result = resolve_navigate_url(
            "file:///home/user/app/index.mizu",
            "file:///home/user/app/about.mizu",
        );
        assert!(
            result.is_some(),
            "file:// origin must be allowed to navigate to file:// within sandbox"
        );
        assert_eq!(result.unwrap(), "file:///home/user/app/about.mizu");
    }

    // --- Sandbox enforcement tests -------------------------------------------

    #[test]
    fn test_file_url_path_traversal_blocked() {
        // Relative ".." traversal must be blocked.
        let result = resolve_navigate_url("file:///home/user/app/index.mizu", "../../etc/passwd");
        assert!(
            result.is_none(),
            "path traversal via '..' must be blocked by sandbox, got: {result:?}"
        );

        // Absolute file:// outside the sandbox must be blocked.
        let result = resolve_navigate_url("file:///home/user/app/index.mizu", "file:///etc/passwd");
        assert!(
            result.is_none(),
            "absolute file:// outside sandbox must be blocked, got: {result:?}"
        );
    }

    #[test]
    fn test_file_url_legitimate_relative_navigation_allowed() {
        // Same-directory relative navigation must succeed and stay in sandbox.
        let result = resolve_navigate_url("file:///home/user/app/index.mizu", "about.mizu");
        let url = result.expect("same-directory navigation must succeed");
        assert!(url.starts_with("file:///"), "must be a file:// URL: {url}");
        assert!(url.ends_with("about.mizu"), "must target about.mizu: {url}");
        assert!(
            url.contains("app"),
            "must stay inside the sandbox directory: {url}"
        );
    }

    #[test]
    fn test_clipboard_local_origin_stealth_copy_blocked() {
        // A document (local or remote) must not copy to clipboard without a
        // qualifying user gesture — stealth exfiltration via background timers
        // is the primary threat for file:// origins.
        let tree = Tree::new(MizuNode {
            primitive: Primitive::Window,
            attributes: {
                let mut m = HashMap::new();
                m.insert("id".to_string(), "sensitive-data".to_string());
                m.insert("content".to_string(), "local secret".to_string());
                m
            },
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });
        let store = crate::core::types::VariableStore::new();
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

    // --- Clipboard security tests -------------------------------------------

    #[test]
    fn test_clipboard_copy_without_user_gesture_fails() {
        let tree = Tree::new(MizuNode {
            primitive: Primitive::Window,
            attributes: {
                let mut m = HashMap::new();
                m.insert("id".to_string(), "my-node".to_string());
                m.insert("content".to_string(), "Copy me!".to_string());
                m
            },
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });
        let store = VariableStore::new();
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
            primitive: Primitive::Window,
            attributes: HashMap::new(),
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });
        let store = VariableStore::new();
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
            primitive: Primitive::Window,
            attributes: {
                let mut m = HashMap::new();
                m.insert("id".to_string(), "label".to_string());
                m.insert("content".to_string(), "Copy me!".to_string());
                m
            },
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });
        let store = VariableStore::new();
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

    // --- Keyboard focus order / activation tests (ux-1) ---------------------

    fn click_event_block() -> crate::parser::EventBlock {
        crate::parser::EventBlock::Click {
            action: crate::parser::Action::Assign {
                target: "clicked".to_string(),
                expr: crate::parser::Expr::Literal(crate::core::types::Value::Bool(true)),
            },
        }
    }

    fn window_node() -> MizuNode {
        MizuNode {
            primitive: Primitive::Window,
            attributes: HashMap::new(),
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        }
    }

    fn plain_box_node() -> MizuNode {
        MizuNode {
            primitive: Primitive::Box,
            attributes: HashMap::new(),
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        }
    }

    fn clickable_box_node() -> MizuNode {
        let mut events = HashMap::new();
        events.insert("click".to_string(), click_event_block());
        MizuNode {
            primitive: Primitive::Box,
            attributes: HashMap::new(),
            events,
            iterator_context: None,
            conditional_classes: Vec::new(),
        }
    }

    fn input_node(name: &str) -> MizuNode {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), name.to_string());
        MizuNode {
            primitive: Primitive::Input,
            attributes: attrs,
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        }
    }

    fn button_node() -> MizuNode {
        let mut events = HashMap::new();
        events.insert("click".to_string(), click_event_block());
        MizuNode {
            primitive: Primitive::Button,
            attributes: HashMap::new(),
            events,
            iterator_context: None,
            conditional_classes: Vec::new(),
        }
    }

    #[test]
    fn focusable_nodes_in_order_excludes_plain_includes_click_box() {
        // window -> [plain box, clickable box, input, button] in document order.
        let tree = Tree::new(window_node());
        let mut manager = MizuWindowManager::new(
            tree,
            HashMap::new(),
            FxHashMap::default(),
            #[cfg(feature = "insecure-dev")]
            false,
        )
        .expect("manager created");

        let plain_id = manager.dom.root_mut().append(plain_box_node()).id();
        let click_box_id = manager.dom.root_mut().append(clickable_box_node()).id();
        let input_id = manager.dom.root_mut().append(input_node("a")).id();
        let button_id = manager.dom.root_mut().append(button_node()).id();
        manager.rebuild_node_mappings();

        let order = manager.focusable_nodes_in_order();
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
            FxHashMap::default(),
            #[cfg(feature = "insecure-dev")]
            false,
        )
        .expect("manager created");

        let a = manager.dom.root_mut().append(input_node("a")).id();
        let b = manager.dom.root_mut().append(input_node("b")).id();
        let c = manager.dom.root_mut().append(input_node("c")).id();
        manager.rebuild_node_mappings();

        // Nothing focused: Tab focuses the first, Shift-Tab focuses the last.
        assert_eq!(manager.next_focus_target(false), Some(a));
        assert_eq!(manager.next_focus_target(true), Some(c));

        // Forward advance a -> b -> c -> wraps to a.
        manager.focused_node = Some(a);
        assert_eq!(manager.next_focus_target(false), Some(b));
        manager.focused_node = Some(b);
        assert_eq!(manager.next_focus_target(false), Some(c));
        manager.focused_node = Some(c);
        assert_eq!(
            manager.next_focus_target(false),
            Some(a),
            "Tab from the last focusable node must wrap to the first"
        );

        // Shift-Tab reverses: a -> wraps to c.
        manager.focused_node = Some(a);
        assert_eq!(
            manager.next_focus_target(true),
            Some(c),
            "Shift-Tab from the first focusable node must wrap to the last"
        );
        manager.focused_node = Some(c);
        assert_eq!(manager.next_focus_target(true), Some(b));
    }

    #[test]
    fn dispatch_click_gesture_sets_gesture_and_emits_single_click() {
        // Security regression (MNT ux-1 guardrail): keyboard activation of a
        // focused button must reuse the exact mouse-click gesture sequence —
        // `has_user_gesture = true` plus exactly one `UiEvent::Click` for that
        // node, no more, no less. The keyboard Enter/Space handler in
        // event_loop.rs calls this same `dispatch_click_gesture` helper, so
        // pinning its behavior here pins keyboard activation as well.
        let tree = Tree::new(window_node());
        let mut manager = MizuWindowManager::new(
            tree,
            HashMap::new(),
            FxHashMap::default(),
            #[cfg(feature = "insecure-dev")]
            false,
        )
        .expect("manager created");

        let button_id = manager.dom.root_mut().append(button_node()).id();
        manager.rebuild_node_mappings();

        // Replace the real logic channel with a test channel so the emitted
        // UiEvent can be observed directly.
        let (test_tx, test_rx) = std::sync::mpsc::channel();
        manager.logic_tx = test_tx;
        manager.has_user_gesture = false;

        let dispatched = dispatch_click_gesture(&mut manager, button_id);
        assert!(dispatched, "dispatch must succeed for a live DOM node");
        assert!(
            manager.has_user_gesture,
            "keyboard activation must set has_user_gesture, exactly like a mouse click"
        );

        let events: Vec<_> = test_rx.try_iter().collect();
        assert_eq!(
            events.len(),
            1,
            "exactly one UiEvent must be emitted, got: {events:?}"
        );
        match &events[0] {
            crate::network::UiEvent::Click { node_id } => {
                let expected_u32 = *manager.node_id_to_u32.get(&button_id).unwrap();
                assert_eq!(*node_id, expected_u32);
            }
            other => panic!("expected UiEvent::Click, got: {other:?}"),
        }
    }
