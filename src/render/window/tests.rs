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
