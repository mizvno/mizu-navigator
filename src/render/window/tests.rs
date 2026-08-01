use super::input::*;
use super::manager::*;
use super::navigate::*;
use crate::core::errors::MizuError;
use crate::core::types::VariableStore;
use crate::network::TabId;
use crate::parser::MizuDimension;
use crate::parser::{MizuNode, Primitive, StyleRules};
use crate::render::chrome_vello::CHROME_HEIGHT;
use crate::render::security::StorageUsageLedger;
use ego_tree::Tree;
use rustc_hash::FxHashMap;
use std::collections::HashMap;

#[test]
fn test_manager_resize_viewport() {
    let tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: {
            let mut attrs = FxHashMap::default();
            attrs.insert("class".to_string(), "window".to_string());
            attrs
        },
        events: FxHashMap::default(),
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

    let (mut manager, _keepalive) = make_manager_with(tree, styles);

    manager
        .resize_viewport(800.0, 600.0)
        .expect("Initial resize ok");

    let layout = manager
        .active()
        .taffy
        .layout(manager.active().root_taffy_id)
        .expect("Layout exists");
    assert_eq!(layout.size.width, 800.0);
    assert_eq!(layout.size.height, 600.0 - CHROME_HEIGHT);

    manager
        .resize_viewport(1024.0, 768.0)
        .expect("Second resize ok");
    let layout = manager
        .active()
        .taffy
        .layout(manager.active().root_taffy_id)
        .expect("Layout exists");
    assert_eq!(layout.size.width, 1024.0);
    assert_eq!(layout.size.height, 768.0 - CHROME_HEIGHT);
}

/// Default test URL. `mizu://localhost/...` gets the localhost capability
/// tier, matching what `MizuWindowManager::new` used before the tab split.
const TEST_URL: &str = "mizu://localhost/index.mizu";

/// Builds a single `TabState` for tests — no threads, no system fonts.
fn make_tab(
    id: u64,
    dom: Tree<MizuNode>,
    styles: HashMap<String, StyleRules>,
    url: &str,
    storage_usage: &StorageUsageLedger,
) -> TabState {
    let mut throwaway_cache = lru::LruCache::new(std::num::NonZeroUsize::new(8).unwrap());
    TabState::new(
        TabId(id),
        TabDocument {
            dom,
            style_rules: styles,
            style_variants: Vec::new(),
            logic_fns: FxHashMap::default(),
        },
        crate::render::responsive::RenderEnvironment {
            viewport: crate::render::responsive::ViewportSize {
                width: 800.0,
                height: 600.0 - CHROME_HEIGHT,
            },
            color_scheme: crate::render::preferences::ColorScheme::Dark,
        },
        url,
        &mut throwaway_cache,
        storage_usage,
    )
    .expect("tab created")
}

/// Builds a headless single-tab manager around `dom`/`styles`.
///
/// Returns the channel keep-alive alongside it; bind it (`let (mut m, _k)
/// = ...`) so the manager's senders keep a live peer for the test's
/// duration.
fn make_manager_with(
    dom: Tree<MizuNode>,
    styles: HashMap<String, StyleRules>,
) -> (MizuWindowManager, TestChannelKeepAlive) {
    let storage_usage = StorageUsageLedger::new();
    let tab = make_tab(0, dom, styles, TEST_URL, &storage_usage);
    MizuWindowManager::new_headless(vec![tab], storage_usage)
}

fn window_dom() -> Tree<MizuNode> {
    Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: {
            let mut attrs = FxHashMap::default();
            attrs.insert("class".to_string(), "window".to_string());
            attrs
        },
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    })
}

/// Spends the active tab's whole per-second storage write budget.
///
/// Used as the sentinel for "was `capability_policy` rebuilt?": the rate
/// counter is per page load, so a subsequent successful write proves the
/// navigation choke point ran. (The byte total deliberately survives a
/// rebuild — see `navigating_does_not_refill_an_exhausted_storage_quota` —
/// so it cannot serve as that sentinel.)
fn saturate_write_rate(manager: &mut MizuWindowManager) {
    let policy = &mut manager.active_mut().capability_policy;
    while policy.check_storage_write(1).is_ok() {}
}

/// Whether the active tab may perform a storage write right now.
fn write_rate_available(manager: &mut MizuWindowManager) -> bool {
    manager
        .active_mut()
        .capability_policy
        .check_storage_write(1)
        .is_ok()
}

fn make_minimal_manager() -> (MizuWindowManager, TestChannelKeepAlive) {
    let mut styles = HashMap::new();
    styles.insert("window".to_string(), StyleRules::default());
    make_manager_with(window_dom(), styles)
}

#[test]
fn redirect_counter_allows_up_to_max_then_stops() {
    let (mut manager, _keepalive) = make_minimal_manager();
    for hop in 1..=*MAX_REDIRECTS {
        assert!(
            manager.active_mut().register_redirect(),
            "redirect hop {hop} should be permitted (<= MAX_REDIRECTS)"
        );
    }
    assert!(
        !manager.active_mut().register_redirect(),
        "redirect hop {} must be refused (exceeds MAX_REDIRECTS)",
        *MAX_REDIRECTS + 1
    );
}

#[test]
fn redirect_counter_reset_clears_budget() {
    let (mut manager, _keepalive) = make_minimal_manager();
    for _ in 0..*MAX_REDIRECTS {
        assert!(manager.active_mut().register_redirect());
    }
    assert!(
        !manager.active_mut().register_redirect(),
        "budget exhausted before reset"
    );
    manager.active_mut().reset_redirect_count();
    assert!(
        manager.active_mut().register_redirect(),
        "after reset, a fresh navigation chain may redirect again"
    );
}

// --- Navigation security / URL resolution tests ----------------------------

#[test]
fn test_remote_origin_cannot_navigate_file() {
    let result = resolve_navigate_url("mizu://shop.example.com/index.mizu", "file:///etc/passwd");
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

// --- Clipboard security tests -------------------------------------------

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

// --- Keyboard focus order / activation tests (ux-1) ---------------------

fn click_event_block() -> crate::parser::EventBlock {
    let mut arena = crate::parser::logic::ExprArena::new();
    let root = arena.alloc(crate::parser::Expr::Literal(
        crate::core::types::Value::Bool(true),
    ));
    crate::parser::EventBlock::Click {
        action: crate::parser::Action::Assign {
            target: "clicked".to_string(),
            expr: crate::parser::logic::ExprTree { arena, root },
        },
    }
}

fn window_node() -> MizuNode {
    MizuNode {
        primitive: Primitive::Doc,
        attributes: FxHashMap::default(),
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

fn plain_box_node() -> MizuNode {
    MizuNode {
        primitive: Primitive::Box,
        attributes: FxHashMap::default(),
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

fn clickable_box_node() -> MizuNode {
    let mut events = FxHashMap::default();
    events.insert("click".to_string(), click_event_block());
    MizuNode {
        primitive: Primitive::Box,
        attributes: FxHashMap::default(),
        events,
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

fn input_node(name: &str) -> MizuNode {
    let mut attrs = FxHashMap::default();
    attrs.insert("name".to_string(), name.to_string());
    MizuNode {
        primitive: Primitive::Input,
        attributes: attrs,
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

fn button_node() -> MizuNode {
    let mut events = FxHashMap::default();
    events.insert("click".to_string(), click_event_block());
    MizuNode {
        primitive: Primitive::Button,
        attributes: FxHashMap::default(),
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

// --- History (ux-4): Back/Forward must route through N2 -----------------

#[test]
fn history_back_step_routes_through_navigation_choke_point() {
    // Security regression: a Back step must not swap `chrome_state.url`
    // directly. It must go through `navigate_to_url`'s Allow branch,
    // which — among other N5 lifecycle resets — always installs a fresh
    // `CapabilityPolicy`. We plant a sentinel in the current policy and
    // confirm it is gone after the Back step: that's only possible if
    // the real choke point ran, not a bypass.
    let (mut manager, _keepalive) = make_minimal_manager();
    manager.active_mut().chrome_state.url = "mizu://current.example/page".to_string();
    manager
        .active_mut()
        .history
        .record_navigation(super::history::HistoryEntry {
            url: "mizu://previous.example/page".to_string(),
            scroll_y: 77.0,
        });
    assert!(manager.active_mut().history.can_go_back());

    saturate_write_rate(&mut manager);

    {
        let (t, mut c) = manager.split_active();
        navigate_back(t, &mut c)
    };

    assert_eq!(
        manager.active_mut().chrome_state.url,
        "mizu://previous.example/page",
        "Back must navigate to the popped history entry"
    );
    assert!(
        write_rate_available(&mut manager),
        "capability_policy must have been freshly rebuilt by navigate_to_url's \
             Allow branch (N5) — a direct URL swap bypassing the choke point \
             would have left the exhausted write budget in place"
    );
    assert!(
        !manager.active_mut().history.can_go_back(),
        "the popped entry must be gone from the back stack"
    );
    assert!(
        manager.active_mut().history.can_go_forward(),
        "the page left behind must now be on the forward stack"
    );
}

#[test]
fn history_forward_step_routes_through_navigation_choke_point() {
    let (mut manager, _keepalive) = make_minimal_manager();
    manager.active_mut().chrome_state.url = "mizu://current.example/page".to_string();
    manager
        .active_mut()
        .history
        .record_navigation(super::history::HistoryEntry {
            url: "mizu://previous.example/page".to_string(),
            scroll_y: 0.0,
        });
    {
        let (t, mut c) = manager.split_active();
        navigate_back(t, &mut c)
    };
    assert!(manager.active_mut().history.can_go_forward());

    saturate_write_rate(&mut manager);
    {
        let (t, mut c) = manager.split_active();
        navigate_forward(t, &mut c)
    };

    assert_eq!(
        manager.active_mut().chrome_state.url,
        "mizu://current.example/page"
    );
    assert!(
        write_rate_available(&mut manager),
        "Forward must also route through the choke point's N5 rebuild"
    );
}

#[test]
fn history_back_with_empty_stack_fires_no_navigation() {
    // Disabled-button behavior: clicking Back with an empty back stack
    // must be a guaranteed no-op, not merely "unlikely to do anything".
    let (mut manager, _keepalive) = make_minimal_manager();
    assert!(!manager.active_mut().history.can_go_back());
    manager.active_mut().chrome_state.url = "mizu://only-page.example/".to_string();
    saturate_write_rate(&mut manager);

    {
        let (t, mut c) = manager.split_active();
        navigate_back(t, &mut c)
    };

    assert_eq!(
        manager.active_mut().chrome_state.url,
        "mizu://only-page.example/",
        "URL must be unchanged when back stack is empty"
    );
    assert!(
        !write_rate_available(&mut manager),
        "capability_policy must be untouched — no navigation occurred at all"
    );
    assert!(!manager.active_mut().history.can_go_forward());
}

#[test]
fn history_forward_with_empty_stack_fires_no_navigation() {
    let (mut manager, _keepalive) = make_minimal_manager();
    assert!(!manager.active_mut().history.can_go_forward());
    manager.active_mut().chrome_state.url = "mizu://only-page.example/".to_string();
    saturate_write_rate(&mut manager);

    {
        let (t, mut c) = manager.split_active();
        navigate_forward(t, &mut c)
    };

    assert_eq!(
        manager.active_mut().chrome_state.url,
        "mizu://only-page.example/"
    );
    assert!(!write_rate_available(&mut manager));
}

#[test]
fn fresh_navigation_after_back_clears_forward_stack() {
    // A -> B -> C, back to B, then a fresh navigation to D from B must
    // clear the forward stack (standard browser semantics).
    let (mut manager, _keepalive) = make_minimal_manager();
    manager.active_mut().chrome_state.url = "mizu://a.example/".to_string();
    {
        let (t, mut c) = manager.split_active();
        navigate_to_url(
            t,
            &mut c,
            "mizu://b.example/".to_string(),
            crate::render::navigation::NavigationInitiator::UserGesture,
        );
    }
    {
        let (t, mut c) = manager.split_active();
        navigate_to_url(
            t,
            &mut c,
            "mizu://c.example/".to_string(),
            crate::render::navigation::NavigationInitiator::UserGesture,
        );
    }
    assert_eq!(manager.active_mut().chrome_state.url, "mizu://c.example/");

    {
        let (t, mut c) = manager.split_active();
        navigate_back(t, &mut c)
    };
    assert_eq!(manager.active_mut().chrome_state.url, "mizu://b.example/");
    assert!(manager.active_mut().history.can_go_forward());

    {
        let (t, mut c) = manager.split_active();
        navigate_to_url(
            t,
            &mut c,
            "mizu://d.example/".to_string(),
            crate::render::navigation::NavigationInitiator::UserGesture,
        );
    }
    assert_eq!(manager.active_mut().chrome_state.url, "mizu://d.example/");
    assert!(
        !manager.active_mut().history.can_go_forward(),
        "a fresh navigation must clear the forward stack"
    );
    assert!(manager.active_mut().history.can_go_back());
}

// --- Timer dispatch admission (unbounded worker backlog) ---

#[test]
fn timer_ticks_are_capped_while_the_worker_is_behind() {
    // The channel to the logic worker is unbounded, so nothing but this gate
    // stops a document from queueing ticks faster than they can be serviced.
    let (mut manager, _keepalive) = make_minimal_manager();
    let tab = manager.active_mut();

    for i in 0..MAX_INFLIGHT_TIMER_TICKS {
        assert!(
            tab.may_dispatch_timer_tick(),
            "tick {i} must be admitted while under the cap"
        );
    }
    assert!(
        !tab.may_dispatch_timer_tick(),
        "a tick beyond the cap must be dropped, not queued"
    );
    assert_eq!(tab.inflight_timer_ticks, MAX_INFLIGHT_TIMER_TICKS);
}

#[test]
fn a_worker_response_frees_timer_capacity() {
    // Throttling, not disarming: the document must recover as soon as the
    // worker catches up.
    let (mut manager, _keepalive) = make_minimal_manager();
    let tab = manager.active_mut();

    while tab.may_dispatch_timer_tick() {}
    assert!(!tab.may_dispatch_timer_tick());

    tab.release_timer_tick();
    assert!(
        tab.may_dispatch_timer_tick(),
        "capacity freed by a response must be reusable"
    );
}

#[test]
fn releasing_more_than_was_dispatched_cannot_underflow() {
    // Responses are not in 1:1 correspondence with ticks (any response for the
    // tab releases capacity), so over-releasing is expected and must saturate
    // rather than wrap into an enormous budget.
    let (mut manager, _keepalive) = make_minimal_manager();
    let tab = manager.active_mut();

    for _ in 0..8 {
        tab.release_timer_tick();
    }
    assert_eq!(tab.inflight_timer_ticks, 0);

    for _ in 0..MAX_INFLIGHT_TIMER_TICKS {
        assert!(tab.may_dispatch_timer_tick());
    }
    assert!(
        !tab.may_dispatch_timer_tick(),
        "the cap must still hold after saturating releases"
    );
}

#[test]
fn reload_clears_outstanding_timer_ticks() {
    // A tick outstanding against the previous document can never be answered
    // once the worker rebuilds the tab's state, so it must not permanently
    // consume capacity.
    let (mut manager, _keepalive) = make_minimal_manager();
    let tab = manager.active_mut();

    while tab.may_dispatch_timer_tick() {}
    tab.reset_timer_ticks();

    assert_eq!(tab.inflight_timer_ticks, 0);
    assert!(tab.may_dispatch_timer_tick());
}

#[test]
fn timer_tick_budget_is_per_tab() {
    // T1: one document's backlog must not suppress another's timers.
    let ledger = StorageUsageLedger::new();
    let (mut manager, _keepalive) = MizuWindowManager::new_headless(
        vec![
            make_tab(0, window_dom(), HashMap::new(), TEST_URL, &ledger),
            make_tab(1, window_dom(), HashMap::new(), TEST_URL, &ledger),
        ],
        ledger,
    );

    while manager.tabs[0].may_dispatch_timer_tick() {}
    assert!(!manager.tabs[0].may_dispatch_timer_tick());
    assert!(
        manager.tabs[1].may_dispatch_timer_tick(),
        "a saturated tab must not consume another tab's dispatch capacity"
    );
}

// --- N3: server redirects may not manufacture user agency ---

/// Drives one `NavigationRedirect` result against the active tab and returns
/// the URL the tab ended up on.
fn redirect_to(
    manager: &mut MizuWindowManager,
    new_url: &str,
    initiator: crate::render::navigation::NavigationInitiator,
) -> String {
    let tab_id = manager.active().id;
    {
        let (t, mut c) = manager.split_active();
        process_network_result(
            t,
            &mut c,
            crate::network::NetworkResult::NavigationRedirect {
                tab: tab_id,
                new_url: new_url.to_string(),
                initiator,
            },
        );
    }
    manager.active_mut().chrome_state.url.clone()
}

#[test]
fn cross_origin_redirect_of_document_logic_navigation_is_blocked() {
    // Security regression: a document-logic `navigate` to its OWN origin is
    // allowed (no gesture needed), and the server answering it with
    // `Location: mizu://evil.example/` must not thereby obtain a cross-origin
    // navigation. Before the fix this site hardcoded
    // `RedirectOf(UserGesture)`, so one header cleared the N3 gate.
    let (mut manager, _keepalive) = make_minimal_manager();
    manager.active_mut().chrome_state.url = "mizu://own.example/".to_string();

    let landed = redirect_to(
        &mut manager,
        "mizu://evil.example/trap",
        crate::render::navigation::NavigationInitiator::DocumentLogic,
    );

    assert_eq!(
        landed, "mizu://own.example/",
        "a redirect of a document-logic navigation must not cross origin"
    );
}

#[test]
fn same_origin_redirect_of_document_logic_navigation_is_allowed() {
    // The block above must be about the origin hop, not about redirects: a
    // same-origin redirect of a logic navigation is ordinary and must work.
    let (mut manager, _keepalive) = make_minimal_manager();
    manager.active_mut().chrome_state.url = "mizu://own.example/".to_string();

    let landed = redirect_to(
        &mut manager,
        "mizu://own.example/next",
        crate::render::navigation::NavigationInitiator::DocumentLogic,
    );

    assert_eq!(landed, "mizu://own.example/next");
}

#[test]
fn cross_origin_redirect_of_user_gesture_navigation_is_allowed() {
    // The mirror image: real user agency still survives the redirect chain,
    // so the fix does not turn into a blanket ban on cross-origin redirects.
    let (mut manager, _keepalive) = make_minimal_manager();
    manager.active_mut().chrome_state.url = "mizu://own.example/".to_string();

    let landed = redirect_to(
        &mut manager,
        "mizu://other.example/page",
        crate::render::navigation::NavigationInitiator::UserGesture,
    );

    assert_eq!(landed, "mizu://other.example/page");
}

#[test]
fn redirect_chains_do_not_accumulate_agency() {
    // Hop 2 of a document-logic chain is still document logic: the initiator
    // arrives already wrapped as `RedirectOf(DocumentLogic)`, and re-wrapping
    // it must neither promote it nor nest without bound.
    let (mut manager, _keepalive) = make_minimal_manager();
    manager.active_mut().chrome_state.url = "mizu://own.example/".to_string();

    let landed = redirect_to(
        &mut manager,
        "mizu://evil.example/trap",
        crate::render::navigation::NavigationInitiator::RedirectOf(Box::new(
            crate::render::navigation::NavigationInitiator::DocumentLogic,
        )),
    );

    assert_eq!(
        landed, "mizu://own.example/",
        "agency must not be gained by adding redirect hops"
    );
}

// --- Bidi anti-spoofing (ux-7): programmatic chrome_state.url assignment ---

#[test]
fn navigate_to_url_strips_bidi_overrides_from_displayed_url() {
    // Security regression: a document-driven navigation (e.g. a
    // `navigate` action whose target happens to contain a bidi
    // override character) must not be able to plant one into the
    // address bar's display any more than typing one can
    // (chrome_vello.rs's insert_text is the other choke point).
    let (mut manager, _keepalive) = make_minimal_manager();
    manager.active_mut().chrome_state.url = "mizu://start.example/".to_string();

    {
        let (t, mut c) = manager.split_active();
        navigate_to_url(
            t,
            &mut c,
            "mizu://evil\u{202E}gnp.example/".to_string(),
            crate::render::navigation::NavigationInitiator::UserGesture,
        );
    }

    assert!(
        !manager.active_mut().chrome_state.url.contains('\u{202E}'),
        "the displayed URL must never contain an RLO override character, got: {:?}",
        manager.active_mut().chrome_state.url
    );
}

// --- `type "file"` inputs: native picker + $form (mocked — no real OS dialog) ---

fn file_input_node(name: &str, accept: Option<&str>) -> MizuNode {
    let mut attrs = FxHashMap::default();
    attrs.insert("name".to_string(), name.to_string());
    attrs.insert("type".to_string(), "file".to_string());
    if let Some(accept) = accept {
        attrs.insert("accept".to_string(), accept.to_string());
    }
    MizuNode {
        primitive: Primitive::Input,
        attributes: attrs,
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

fn submit_event_block() -> crate::parser::EventBlock {
    let mut arena = crate::parser::logic::ExprArena::new();
    let root = arena.alloc(crate::parser::Expr::Literal(
        crate::core::types::Value::Bool(true),
    ));
    crate::parser::EventBlock::Submit {
        action: crate::parser::Action::Assign {
            target: "submitted".to_string(),
            expr: crate::parser::logic::ExprTree { arena, root },
        },
    }
}

fn form_node() -> MizuNode {
    MizuNode {
        primitive: Primitive::Form,
        attributes: FxHashMap::default(),
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

fn submit_button_node() -> MizuNode {
    let mut events = FxHashMap::default();
    events.insert("submit".to_string(), submit_event_block());
    MizuNode {
        primitive: Primitive::Button,
        attributes: FxHashMap::default(),
        events,
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

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

// ---- Tab lifecycle & per-tab isolation (invariant T1) ----

/// Builds a headless manager with `n` tabs over the same trivial document.
fn make_multi_tab_manager(n: u64) -> (MizuWindowManager, TestChannelKeepAlive) {
    let mut styles = HashMap::new();
    styles.insert("window".to_string(), StyleRules::default());
    let storage_usage = StorageUsageLedger::new();
    let tabs = (0..n)
        .map(|i| make_tab(i, window_dom(), styles.clone(), TEST_URL, &storage_usage))
        .collect();
    MizuWindowManager::new_headless(tabs, storage_usage)
}

#[test]
fn opening_tabs_spawns_no_threads() {
    use std::sync::atomic::Ordering::SeqCst;
    // The spawn counters are process-wide and the other tests in this
    // file build real managers, so the totals only hold still while this
    // gate is held — see `manager::SPAWN_GATE`.
    let _gate = lock_spawn_gate();
    let base = crate::parser::logic_worker::SPAWN_COUNT.load(SeqCst)
        + crate::network::worker::SPAWN_COUNT.load(SeqCst);
    let (mut manager, _keepalive) = make_minimal_manager();
    for _ in 0..8 {
        manager
            .open_tab("mizu://localhost/blank.mizu")
            .expect("tab opens");
    }
    assert_eq!(manager.tabs.len(), 9);
    assert_eq!(
        crate::parser::logic_worker::SPAWN_COUNT.load(SeqCst)
            + crate::network::worker::SPAWN_COUNT.load(SeqCst),
        base,
        "tabs share the two window-level workers; opening one must spawn no thread"
    );
}

#[test]
fn open_tab_refuses_past_max() {
    let (mut manager, _keepalive) = make_minimal_manager();
    while manager.tabs.len() < MAX_OPEN_TABS {
        assert!(manager.open_tab(TEST_URL).is_some());
    }
    assert!(
        manager.open_tab(TEST_URL).is_none(),
        "the cap is what keeps tab creation from being a memory-exhaustion vector"
    );
}

#[test]
fn tab_ids_are_never_reused() {
    let (mut manager, _keepalive) = make_minimal_manager();
    let first = manager.open_tab(TEST_URL).expect("opens");
    assert!(manager.close_tab(first));
    let second = manager.open_tab(TEST_URL).expect("opens");
    assert_ne!(
        first, second,
        "a recycled id would let a late message resolve against a different interner"
    );
}

#[test]
fn a11y_epochs_are_never_reused_across_documents_or_tabs() {
    // `rebuild_node_mappings` renumbers nodes from zero, so the epoch is
    // the only thing telling the accessibility layer that node 3 of the
    // new mapping is not node 3 of the old one. Reuse is what makes the
    // accesskit consumer prune a live subtree and panic.
    let (mut manager, _keepalive) = make_minimal_manager();
    let mut seen = std::collections::HashSet::new();
    assert!(seen.insert(manager.active().a11y_epoch));

    for _ in 0..3 {
        manager.active_mut().rebuild_node_mappings();
        assert!(
            seen.insert(manager.active().a11y_epoch),
            "a document reload must not reuse an epoch"
        );
    }

    let second = manager.open_tab(TEST_URL).expect("opens");
    manager.switch_to_tab(second);
    assert!(
        seen.insert(manager.active().a11y_epoch),
        "tabs share one accesskit adapter, so their epochs must differ too"
    );
    assert!(
        manager.active().a11y_epoch != 0,
        "epoch 0 would make a node id indistinguishable from a bare u32 id"
    );
}

#[test]
fn close_tab_refuses_the_last_tab() {
    let (mut manager, _keepalive) = make_minimal_manager();
    let only = manager.active().id;
    assert!(
        !manager.close_tab(only),
        "closing the last tab is the caller's exit signal"
    );
    assert_eq!(manager.tabs.len(), 1);
}

#[test]
fn active_tab_index_stays_in_bounds_after_close() {
    for close_at in 0..4usize {
        let (mut manager, _keepalive) = make_multi_tab_manager(4);
        let victim = manager.tabs[close_at].id;
        manager.switch_to_tab(manager.tabs[3].id);
        assert!(manager.close_tab(victim));
        assert_eq!(manager.tabs.len(), 3);
        assert!(
            manager.active_tab_index() < manager.tabs.len(),
            "closing at position {close_at} left active_tab out of range"
        );
    }
}

#[test]
fn redirect_budget_is_per_tab() {
    let (mut manager, _keepalive) = make_multi_tab_manager(2);
    let b = manager.tabs[1].id;
    // Exhaust tab A's budget.
    while manager.tabs[0].register_redirect() {}
    assert!(
        manager
            .split_tab(b)
            .expect("tab b exists")
            .0
            .register_redirect(),
        "one tab's redirect chain must not consume another's loop protection"
    );
}

#[test]
fn write_rate_limit_is_per_tab() {
    // The *burst* budget belongs to the running document, so one tab
    // exhausting it must not stall another's writes.
    let (mut manager, _keepalive) = make_multi_tab_manager(2);
    while manager.tabs[0]
        .capability_policy
        .check_storage_write(1)
        .is_ok()
    {}

    assert!(
        manager.tabs[0]
            .capability_policy
            .check_storage_write(1)
            .is_err(),
        "the first tab's per-second write budget must be exhausted"
    );
    assert!(
        manager.tabs[1]
            .capability_policy
            .check_storage_write(1)
            .is_ok(),
        "one document's write burst must not consume another's"
    );
}

#[test]
fn storage_byte_quota_is_shared_by_every_tab_on_one_origin() {
    // The byte quota bounds data at rest, and both tabs write to the same
    // encrypted store, so they must draw on one budget. Two independent
    // budgets would mean opening a second tab doubles what an origin can
    // persist.
    let (mut manager, _keepalive) = make_multi_tab_manager(2);
    manager.tabs[0]
        .capability_policy
        .check_storage_write(4096)
        .expect("first write is within quota");

    assert_eq!(
        manager.tabs[1].capability_policy.bytes_stored(),
        4096,
        "a second tab on the same origin must see the bytes already spent"
    );
}

#[test]
fn storage_byte_quota_is_isolated_between_origins() {
    let ledger = StorageUsageLedger::new();
    let mut a = crate::render::security::capability_policy_for("mizu://a.example/x", &ledger);
    let b = crate::render::security::capability_policy_for("mizu://b.example/x", &ledger);

    a.check_storage_write(4096).expect("within quota");

    assert_eq!(a.bytes_stored(), 4096);
    assert_eq!(
        b.bytes_stored(),
        0,
        "one origin must never be charged for another's writes"
    );
}

#[test]
fn navigating_does_not_refill_an_exhausted_storage_quota() {
    // The bypass this closes: a same-origin `navigate` needs no user gesture,
    // and it rebuilds `capability_policy`. If the byte total lived on the
    // policy, a document could loop navigate → write-a-full-quota → navigate
    // and persist without bound.
    let (mut manager, _keepalive) = make_minimal_manager();
    let origin = "mizu://greedy.example/index.mizu";
    manager.active_mut().chrome_state.url = origin.to_string();
    manager.active_mut().capability_policy =
        crate::render::security::capability_policy_for(origin, &manager.storage_usage);

    let quota = manager.active().capability_policy.quota_bytes;
    manager
        .active_mut()
        .capability_policy
        .check_storage_write(quota)
        .expect("a write of exactly the quota must be accepted");

    // Same-origin navigation: allowed with no gesture, and it rebuilds the
    // policy through the choke point exactly as production does.
    {
        let (t, mut c) = manager.split_active();
        navigate_to_url(
            t,
            &mut c,
            "mizu://greedy.example/again.mizu".to_string(),
            crate::render::navigation::NavigationInitiator::DocumentLogic,
        );
    }

    assert_eq!(
        manager.active_mut().capability_policy.bytes_stored(),
        quota,
        "navigation must not zero the origin's accumulated byte total"
    );
    assert!(
        manager
            .active_mut()
            .capability_policy
            .check_storage_write(1)
            .is_err(),
        "an origin at its quota must stay at its quota across a navigation"
    );
}

// Gesture agency is no longer a per-tab field, so there is no cross-tab flag
// left to assert on here: it rides on `WorkerResponse::gesture`, and a
// response is routed to the tab whose id the worker echoed back
// (`drain_logic_worker_results`), so one tab's click cannot reach another
// tab's action batch. The per-event property that replaced it — a `RootTimer`
// batch is never marked as a gesture, even immediately after a `Click` — is
// pinned in the worker itself by
// `mizu_core::parser::logic_worker::tests::gesture_is_per_event_not_ambient`.

#[test]
fn switching_relayouts_a_stale_background_tab() {
    let (mut manager, _keepalive) = make_multi_tab_manager(2);
    let b = manager.tabs[1].id;
    manager.resize_viewport(1024.0, 768.0).expect("resize ok");
    assert!(
        manager.tabs[1].layout_stale,
        "a resize while backgrounded must flag the tab rather than relayout it"
    );
    manager.switch_to_tab(b);
    assert!(!manager.tabs[1].layout_stale);
    assert_eq!(manager.active().viewport_size.width, 1024.0);
}

#[test]
fn close_tab_purges_image_waiters() {
    let (mut manager, _keepalive) = make_multi_tab_manager(2);
    let a = manager.tabs[0].id;
    let b = manager.tabs[1].id;
    manager
        .fetching_images
        .insert("mizu://localhost/x.png".to_string(), vec![a, b]);
    manager.switch_to_tab(b);
    assert!(manager.close_tab(a));
    let waiters = &manager.fetching_images["mizu://localhost/x.png"];
    assert_eq!(
        waiters,
        &vec![b],
        "a closed tab must not stay on a waiter list"
    );
}

#[test]
fn tab_titles_are_stripped_of_bidi_overrides() {
    // A document-controlled `title` carrying an RLO could otherwise
    // render reversed over a neighbouring tab's label — a spoofing
    // vector, which is why the strip sanitises exactly like the URL bar.
    let raw = "safe\u{202E}gnp.eruces";
    let cleaned = crate::render::bidi::strip_bidi_overrides(raw);
    assert!(
        !cleaned.contains('\u{202E}'),
        "an RLO override must not survive into a painted tab title"
    );
}

#[test]
fn background_timers_are_throttled_but_still_fire() {
    use super::event_loop::background_timer_period;
    assert_eq!(
        background_timer_period(100),
        1000,
        "a hidden document must not wake the loop 10x a second"
    );
    assert_eq!(
        background_timer_period(5000),
        5000,
        "a slower timer keeps its own period; the clamp is a floor, not a rewrite"
    );
}

#[test]
fn reload_clears_each_block_measurements_keyed_by_the_old_tree() {
    // `each_row_height_estimate` / `each_container_offset_y` are keyed by
    // `EgoNodeId`. Carried across a document reload they would seed the
    // new tree's virtualization with another document's row heights.
    let (mut manager, _keepalive) = make_minimal_manager();
    let stale = manager.active().dom.root().id();
    manager
        .active_mut()
        .each_row_height_estimate
        .insert(stale, 42.0);
    manager
        .active_mut()
        .each_container_offset_y
        .insert(stale, 7.0);

    manager
        .reload_document(ReloadedDocument {
            dom: window_dom(),
            style_rules: HashMap::new(),
            style_variants: Vec::new(),
            logic_fns: FxHashMap::default(),
            interner: crate::core::types::StringInterner::new(),
            computed_bindings: Vec::new(),
            root_timers: Vec::new(),
        })
        .expect("reload ok");

    assert!(manager.active().each_row_height_estimate.is_empty());
    assert!(manager.active().each_container_offset_y.is_empty());
}
