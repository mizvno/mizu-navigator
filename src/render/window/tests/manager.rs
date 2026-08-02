//! Tests for `manager.rs`: viewport resize, redirect/timer budgets (per
//! tab), and tab lifecycle (open/close/switch, id/epoch uniqueness,
//! per-origin storage isolation).

use super::*;

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

// ---- Tab lifecycle & per-tab isolation (invariant T1) ----

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
