//! Test suite for `render::window`, split to mirror its source modules:
//! [`manager`] (tab lifecycle, resize, redirect/timer budgets),
//! [`navigate`] (URL resolution, the N3/N5 origin-safety invariants),
//! [`history`] (back/forward through the navigation choke point),
//! [`focus`] (keyboard focus order), [`input`] (clipboard, file inputs,
//! click/submit dispatch), and [`event_loop`] (background timer throttling).
//!
//! Fixtures shared across every bucket — headless manager builders, node
//! constructors, and the navigation test helpers — live here so each
//! submodule can reach them via `use super::*;`, the same way they were all
//! in scope in the single file this replaced.

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

mod event_loop;
mod focus;
mod history;
mod input;
mod manager;
mod navigate;

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

fn make_minimal_manager() -> (MizuWindowManager, TestChannelKeepAlive) {
    let mut styles = HashMap::new();
    styles.insert("window".to_string(), StyleRules::default());
    make_manager_with(window_dom(), styles)
}

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

/// Points the active tab at `url` as if a document had committed there.
///
/// Tests set the *origin of record*, never the URL-bar buffer: the bar is
/// display state and no longer feeds any decision (see
/// `ChromeState::committed_url`), so seeding it would set up nothing at all.
fn commit_url(manager: &mut MizuWindowManager, url: &str) {
    let policy = crate::render::security::capability_policy_for(url, &manager.storage_usage);
    let tab = manager.active_mut();
    tab.chrome_state.committed_url = url.to_string();
    tab.chrome_state.set_displayed_url(url.to_string());
    tab.capability_policy = policy;
}

/// The URL of the last navigation the choke point dispatched, or `None` if it
/// dispatched none. Drains the command channel.
///
/// `NetworkCmd::Navigate` is emitted from exactly one place
/// (`navigate_to_url`'s `Allow` branch), so its presence is proof the
/// navigation was authorised there and nowhere else.
fn dispatched_navigation(keepalive: &mut TestChannelKeepAlive) -> Option<String> {
    keepalive
        .drain_network_cmds()
        .into_iter()
        .filter_map(|cmd| match cmd {
            crate::network::NetworkCmd::Navigate { url, .. } => Some(url),
            _ => None,
        })
        .next_back()
}

/// Delivers the document the network worker would have returned for a
/// dispatched navigation, committing it.
fn commit_dispatched_navigation(manager: &mut MizuWindowManager, url: String) {
    let tab_id = manager.active().id;
    let (t, mut c) = manager.split_active();
    process_network_result(
        t,
        &mut c,
        crate::network::NetworkResult::NavigateSuccess {
            tab: tab_id,
            url,
            source: "layout\n  doc\n".to_string(),
        },
    );
}

/// Runs a navigation end to end — through the choke point, then through the
/// commit — and returns the tab's committed URL afterwards.
///
/// A `mizu://` navigation only moves the origin once a document arrives, so a
/// test that asserts on the destination has to supply that document; one that
/// stops at the dispatch is asserting on a page that was never loaded.
fn navigate_and_commit(
    manager: &mut MizuWindowManager,
    keepalive: &mut TestChannelKeepAlive,
    url: &str,
    initiator: crate::render::navigation::NavigationInitiator,
) -> String {
    {
        let (t, mut c) = manager.split_active();
        navigate_to_url(t, &mut c, url.to_string(), initiator);
    }
    if let Some(dispatched) = dispatched_navigation(keepalive) {
        commit_dispatched_navigation(manager, dispatched);
    }
    manager.active().chrome_state.committed_url.clone()
}

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

/// Drives one `NavigationRedirect` result against the active tab, lets any
/// navigation the choke point authorised run to completion, and returns the
/// URL the tab ended up committed to.
fn redirect_to(
    manager: &mut MizuWindowManager,
    keepalive: &mut TestChannelKeepAlive,
    new_url: &str,
    initiator: crate::render::navigation::NavigationInitiator,
) -> String {
    let tab_id = manager.active().id;
    let _ = keepalive.drain_network_cmds();
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
    if let Some(url) = dispatched_navigation(keepalive) {
        commit_dispatched_navigation(manager, url);
    }
    manager.active().chrome_state.committed_url.clone()
}

/// Dispatches one `ResolvedCall` to `url` through the production capability
/// path and reports whether it reached the network.
fn resolved_call_reaches_network(
    manager: &mut MizuWindowManager,
    keepalive: &mut TestChannelKeepAlive,
    url: &str,
) -> bool {
    let _ = keepalive.drain_network_cmds();
    // The call's target variable has to be resolvable in the tab's frozen
    // interner, or the dispatch would be refused for that reason instead of
    // the one under test. `a_committed_navigation_does_move_the_origin` is the
    // positive control that keeps this helper honest.
    let target_variable = {
        let mut store = VariableStore::new();
        let sym = store.interner.get_or_intern("result");
        manager.active_mut().store = store.freeze();
        sym
    };
    {
        let (t, c) = manager.split_active();
        execute_tab_capability_action(
            t,
            &c,
            crate::network::RuntimeAction::ResolvedCall {
                method: "POST".to_string(),
                url: url.to_string(),
                payload: Some(crate::core::types::Value::from("local-secret".to_string())),
                target_variable,
                format: crate::parser::logic::PayloadFormat::Json,
                headers: vec![],
            },
        );
    }
    keepalive
        .drain_network_cmds()
        .iter()
        .any(|cmd| matches!(cmd, crate::network::NetworkCmd::Fetch { .. }))
}
