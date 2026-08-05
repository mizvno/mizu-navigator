//! Construction and lifecycle: `TabState::new`, `MizuWindowManager::new`/
//! `new_headless`/test-only constructors, `resize_viewport`,
//! `refresh_virtualized_windows`, `execute_capability_action`, plus the
//! smaller per-tab bookkeeping methods (`setup_timers`, node/dependency
//! index rebuilding, timer-tick throttling, `inspector_sources`).

use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, HashMap};

use ego_tree::{NodeId as EgoNodeId, Tree};
use taffy::TaffyTree;

use crate::core::errors::MizuError;
use crate::core::types::{Symbol, Value, VariableStore};
use crate::network::{ReloadPayload, RuntimeAction, TabId, UiEvent};
use crate::parser::logic::{MizuFunction, TimerInterval};
use crate::parser::style::StyleVariant;
use crate::parser::{EventBlock, MizuNode, StyleRules};
use crate::render::chrome_vello::{CHROME_HEIGHT, ChromeState};
use crate::render::layout_bridge::EachExpansion;
use crate::render::preferences::UserPreferences;
use crate::render::responsive::{RenderEnvironment, ViewportSize};
use crate::render::security::get_raw_domain;
use crate::render::window::AssetSlot;
use crate::render::window::history::{HistoryLog, HistorySidebarState, HistoryStack};

use super::capability::execute_tab_capability_action;
use super::reload::reload_tab_document;
#[cfg(test)]
use super::types::TestChannelKeepAlive;
use super::types::{
    IMAGE_CACHE_CAPACITY, MAX_INFLIGHT_TIMER_TICKS, MAX_REDIRECTS, MizuWindowManager,
    ReloadedDocument, TabDocument, TabState, lock_spawn_gate, next_a11y_epoch,
};
use super::viewport::refresh_tab_virtualized_windows;
use super::viewport::resize_tab_viewport;

impl TabState {
    /// Builds a tab from a freshly parsed document.
    ///
    /// Spawns no threads and loads no fonts: `TabState` owns neither the
    /// `FontContext`/`LayoutContext` nor any channel endpoint (all
    /// window-level), so this is just taffy-tree construction. That is what
    /// lets tests build tabs cheaply instead of paying two thread spawns and
    /// a system-font enumeration per fixture.
    ///
    /// `image_cache` is borrowed from the window because the decoded-image
    /// cache is shared across tabs (keyed by URL, which is already a global
    /// namespace); it holds decoded pixels only, never document state.
    pub fn new(
        id: TabId,
        doc: TabDocument,
        env: RenderEnvironment,
        initial_url: &str,
        image_cache: &mut lru::LruCache<String, AssetSlot>,
        storage_usage: &crate::render::security::StorageUsageLedger,
    ) -> Result<Self, MizuError> {
        let TabDocument {
            dom,
            style_rules,
            style_variants,
            logic_fns,
        } = doc;
        let mut taffy = TaffyTree::new();
        let mut node_to_taffy_id = HashMap::new();
        let root_taffy_id = crate::render::layout_bridge::build_taffy_tree(
            dom.root(),
            &mut crate::render::layout_bridge::TaffyBuildContext {
                style_rules_map: &style_rules,
                taffy: &mut taffy,
                node_to_taffy_id: &mut node_to_taffy_id,
                image_cache,
                chrome_url: initial_url,
                variants: &style_variants,
                env: &env,
            },
        )?;

        let chrome_state = ChromeState {
            committed_url: initial_url.to_string(),
            url: initial_url.to_string(),
            cursor: initial_url.len(),
            ..ChromeState::default()
        };

        let mut tab = Self {
            id,
            dom,
            style_rules,
            style_variants,
            viewport_size: env.viewport,
            layout_stale: false,
            taffy,
            node_to_taffy_id,
            root_taffy_id,
            store: VariableStore::new().freeze(),
            logic_fns,
            scroll_offsets: HashMap::new(),
            focused_node: None,
            chrome_state,
            root_scroll_offset_y: 0.0,
            each_row_height_estimate: HashMap::new(),
            each_container_offset_y: HashMap::new(),
            node_id_to_u32: HashMap::new(),
            u32_to_node_id: HashMap::new(),
            next_u32_id: 0,
            // Superseded by `rebuild_node_mappings` at the end of this
            // constructor, before any accessibility tree is built.
            a11y_epoch: 0,
            dependency_index: HashMap::new(),
            text_layouts: HashMap::new(),
            text_dimensions: HashMap::new(),
            dirty_nodes: std::collections::HashSet::new(),
            typing_layout_dirty: false,
            local_inputs: FxHashMap::default(),
            local_file_selections: FxHashMap::default(),
            url_registry: rustc_hash::FxHashMap::default(),
            each_expansion: EachExpansion::default(),
            redirect_count: 0,
            computed_bindings: Vec::new(),
            capability_policy: crate::render::security::capability_policy_for(
                initial_url,
                storage_usage,
            ),
            root_timers: Vec::new(),
            root_timer_queue: BTreeMap::new(),
            inflight_timer_ticks: 0,
            inspector: crate::render::inspector::InspectorState::new(),
            inspector_log: crate::render::inspector::log::InspectorLog::new(),
            recent_mutations: FxHashMap::default(),
            history: HistoryStack::default(),
            pending_scroll_restore: None,
        };
        tab.rebuild_node_mappings();
        tab.rebuild_dependency_index();
        Ok(tab)
    }
}

impl MizuWindowManager {
    /// Creates a new manager by compiling the DOM styles into Taffy layout components.
    ///
    /// Spawns the two shared background workers (network + logic) **once** for
    /// the whole window: opening further tabs allocates only a [`TabState`]
    /// and never another thread.
    ///
    /// `allow_insecure`: forwarded to the network thread — when `true`, TLS cert
    /// verification is skipped (development only).
    pub fn new(
        dom: Tree<MizuNode>,
        style_rules: HashMap<String, StyleRules>,
        style_variants: Vec<StyleVariant>,
        logic_fns: FxHashMap<Symbol, MizuFunction>,
        #[cfg(feature = "insecure-dev")] allow_insecure: bool,
    ) -> Result<Self, MizuError> {
        let (network_tx, rx) = tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let (tx, network_rx) =
            tokio::sync::mpsc::channel(*crate::network::worker::MAX_UI_CHANNEL_CAPACITY);

        let (logic_tx, logic_worker_rx) = std::sync::mpsc::channel();
        let (logic_worker_tx, logic_rx) = std::sync::mpsc::channel();

        // Both spawns under one gate: see `SPAWN_GATE`. Scoped tightly so the
        // rest of construction — font loading, layout — stays parallel.
        {
            let _gate = lock_spawn_gate();
            crate::network::worker::spawn_network_thread(
                rx,
                tx,
                #[cfg(feature = "insecure-dev")]
                allow_insecure,
            );
            // ── The multi-process cutover ────────────────────────────────
            // Document logic now runs in sandboxed `mizu-worker` processes,
            // one per tab, instead of the shared in-process `LogicWorker`
            // thread. The router satisfies the identical channel contract
            // (`Sender<(TabId, UiEvent)>` in, `Receiver<(TabId, Result<..>)>`
            // out), so every event dispatch site and the idle-loop drain are
            // untouched by the swap.
            //
            // The legacy `LogicWorker` is intentionally left compiling and
            // reachable — see `render::security::broker::ActionOrigin` — so a
            // human can fall back to it during GUI validation on native
            // hardware by restoring the one line below:
            //
            //     crate::parser::logic_worker::LogicWorker::spawn(
            //         logic_worker_rx, logic_worker_tx)?;
            //
            // Failing to start the router is fatal to construction rather
            // than silently degrading to the in-process path: a browser that
            // quietly stopped sandboxing documents would be strictly worse
            // than one that refuses to open.
            crate::worker_host::bridge::spawn_router(logic_worker_rx, logic_worker_tx)?;
        }

        // Embedded IBM Plex fonts only — no OS font-directory FFI backend.
        // See `render::embedded_fonts::new_font_context` for why
        // `parley::FontContext::new()` would not actually achieve this
        // (it eagerly loads system fonts regardless of whether
        // `load_system_fonts()` is called afterward).
        let font_cx = crate::render::embedded_fonts::new_font_context();

        let default_chrome_url = "mizu://localhost/index.mizu";
        // Placeholder viewport: the real window doesn't exist yet at this
        // point in startup. `resize_viewport` rebuilds the taffy tree's
        // styles against the real size as soon as the window is created
        // (see `event_loop::run_window_loop`), so this is superseded within
        // the same startup sequence, before the first frame ever paints.
        let initial_viewport = ViewportSize {
            width: 800.0,
            height: 600.0 - CHROME_HEIGHT,
        };
        let preferences = UserPreferences::default();
        let mut image_cache = lru::LruCache::new(IMAGE_CACHE_CAPACITY);
        let storage_usage = crate::render::security::StorageUsageLedger::new();

        let tab = TabState::new(
            TabId(0),
            TabDocument {
                dom,
                style_rules,
                style_variants,
                logic_fns,
            },
            RenderEnvironment {
                viewport: initial_viewport,
                color_scheme: preferences.color_scheme,
            },
            default_chrome_url,
            &mut image_cache,
            &storage_usage,
        )?;

        let mut manager = Self {
            window: None,
            tabs: vec![tab],
            active_tab: 0,
            next_tab_id: 1,
            window_logical_size: (initial_viewport.width, initial_viewport.height),
            font_cx,
            layout_cx: parley::LayoutContext::new(),
            network_tx,
            network_rx,
            logic_tx,
            logic_rx,
            modifiers: winit::keyboard::ModifiersState::default(),
            image_cache,
            fetching_images: FxHashMap::default(),
            start_time: std::time::Instant::now(),
            last_layout_time: std::time::Instant::now(),
            pending_resize: None,
            preferences,
            history_log: HistoryLog::load_from_disk(),
            history_sidebar: HistorySidebarState::default(),
            storage_usage,
        };

        manager.active().trigger_logic_reload(&manager.logic_tx);
        manager.active_mut().setup_timers();
        Ok(manager)
    }

    /// Test-only constructor: builds a manager around `tabs` with **no**
    /// network thread, **no** logic-worker thread, and **no**
    /// `load_system_fonts()` call.
    ///
    /// The production [`Self::new`] pays two thread spawns plus a system-font
    /// enumeration; every test fixture that used it paid them too. Since
    /// [`TabState`] owns no channel endpoints and no font context, tabs can be
    /// built without any of that — so tests exercise the real logic while
    /// keeping the process's thread count flat, which is also what makes the
    /// "opening tabs spawns no threads" assertion meaningful.
    ///
    /// Holds the receiving ends of the dummy channels alive so that senders
    /// don't observe a disconnected peer mid-test.
    /// `storage_usage` must be the same ledger the tabs were built against, or
    /// a fixture would give the window and its own tabs two different notions
    /// of what an origin has spent.
    #[cfg(test)]
    pub(crate) fn new_headless(
        tabs: Vec<TabState>,
        storage_usage: crate::render::security::StorageUsageLedger,
    ) -> (Self, TestChannelKeepAlive) {
        assert!(
            !tabs.is_empty(),
            "a manager must always own at least one tab"
        );
        let (network_tx, network_cmd_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let (network_result_tx, network_rx) = tokio::sync::mpsc::channel(16);
        let (logic_tx, logic_event_rx) = std::sync::mpsc::channel();
        let (logic_response_tx, logic_rx) = std::sync::mpsc::channel();
        let next_tab_id = tabs.iter().map(|t| t.id.0).max().unwrap_or(0) + 1;
        let manager = Self {
            window: None,
            tabs,
            active_tab: 0,
            next_tab_id,
            window_logical_size: (800.0, 600.0 - CHROME_HEIGHT),
            font_cx: parley::FontContext::new(),
            layout_cx: parley::LayoutContext::new(),
            network_tx,
            network_rx,
            logic_tx,
            logic_rx,
            modifiers: winit::keyboard::ModifiersState::default(),
            image_cache: lru::LruCache::new(IMAGE_CACHE_CAPACITY),
            fetching_images: FxHashMap::default(),
            start_time: std::time::Instant::now(),
            last_layout_time: std::time::Instant::now(),
            pending_resize: None,
            preferences: UserPreferences::default(),
            history_log: HistoryLog::default(),
            history_sidebar: HistorySidebarState::default(),
            storage_usage,
        };
        (
            manager,
            TestChannelKeepAlive {
                _network_cmd_rx: network_cmd_rx,
                _network_result_tx: network_result_tx,
                _logic_event_rx: logic_event_rx,
                _logic_response_tx: logic_response_tx,
            },
        )
    }

    /// Recomputes the active tab's Taffy layout for a new viewport boundary.
    ///
    /// Equivalent to `resize_viewport_with_dirty_lists(width, height, None)`: every
    /// `Each` block is fully rebuilt. Use this for physical window resizes and
    /// inspector toggle, where the `TaffyTree` is recreated from scratch.
    pub fn resize_viewport(&mut self, width: f32, height: f32) -> Result<(), MizuError> {
        self.resize_viewport_with_dirty_lists(width, height, None)
    }

    /// Inner implementation of viewport recomputation for the active tab.
    /// `dirty_list_names` controls whether `expand_each_nodes` does a full
    /// rebuild (`None`) or only rebuilds the `Each` blocks whose backing list
    /// variables are in the provided set (`Some(set)`).
    pub fn resize_viewport_with_dirty_lists(
        &mut self,
        width: f32,
        height: f32,
        dirty_list_names: Option<std::collections::HashSet<String>>,
    ) -> Result<(), MizuError> {
        self.window_logical_size = (width, height);
        // Only the visible tab is relaid out now; the rest are flagged and
        // catch up on activation. Relaying out every tab on every resize tick
        // would cost N taffy rebuilds per frame of a drag, all of them
        // invisible.
        let active = self.active_tab;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if i != active {
                tab.layout_stale = true;
            }
        }
        let (tab, mut ctx) = self.split_active();
        resize_tab_viewport(tab, &mut ctx, width, height, dirty_list_names)
    }

    /// Reloads the active tab's document completely, resetting layout and logic state.
    pub fn reload_document(&mut self, doc: ReloadedDocument) -> Result<(), MizuError> {
        let (tab, mut ctx) = self.split_active();
        reload_tab_document(tab, &mut ctx, doc, true)
    }

    /// Scroll-driven virtualization refresh for the active tab.
    pub fn refresh_virtualized_windows(&mut self, viewport_height: f32) -> Result<bool, MizuError> {
        let (tab, mut ctx) = self.split_active();
        refresh_tab_virtualized_windows(tab, &mut ctx, viewport_height)
    }

    /// Executes a declarative capability action against the active tab.
    pub fn execute_capability_action(&mut self, action: RuntimeAction) {
        let (tab, ctx) = self.split_active();
        execute_tab_capability_action(tab, &ctx, action);
    }
}

impl TabState {
    /// Resolves a root-timer interval to milliseconds, clamped to ≥ 16 ms.
    ///
    /// Variable intervals are read from the store; an unset or non-numeric
    /// variable yields `None` (the timer is skipped until the variable exists).
    pub(crate) fn resolve_root_timer_interval(&self, interval: &TimerInterval) -> Option<u64> {
        let ms = match interval {
            TimerInterval::Millis(ms) => *ms,
            TimerInterval::Variable(var_name) => match self.store.get(var_name).ok() {
                // Both numeric variants: `timer_ms = 500` binds a `Value::Int`
                // (an integer literal), `timer_ms = 500.0` a `Value::Decimal`.
                // Matching only one of them would make the timer silently never
                // fire depending on how the interval was written.
                Some(Value::Int(i)) => (*i).max(0) as u64,
                Some(Value::Decimal(i)) => (*i / crate::core::types::DECIMAL_SCALE).max(0) as u64,
                _ => return None,
            },
        };
        Some(ms.max(16))
    }

    /// Setup the timer priority queue from this tab's document's root `timer`
    /// declarations (the only timer form Mizu supports).
    pub fn setup_timers(&mut self) {
        self.root_timer_queue.clear();
        let now = std::time::Instant::now();
        for (idx, rt) in self.root_timers.iter().enumerate() {
            if let Some(interval_ms) = self.resolve_root_timer_interval(&rt.interval) {
                let deadline = now + std::time::Duration::from_millis(interval_ms);
                self.root_timer_queue.entry(deadline).or_default().push(idx);
            }
        }
    }

    /// Rebuilds bidirectional u32 mappings for all DOM nodes.
    ///
    /// Numbering restarts from zero, so the ids handed out here mean nothing
    /// outside the mapping they belong to. [`Self::a11y_epoch`] advances with
    /// every rebuild to say so: it is what stops the accessibility layer from
    /// reading a fresh document's node 3 as the previous document's node 3.
    pub fn rebuild_node_mappings(&mut self) {
        self.a11y_epoch = next_a11y_epoch();
        self.node_id_to_u32.clear();
        self.u32_to_node_id.clear();
        let mut next_id = 0;
        for node in self.dom.nodes() {
            let id = node.id();
            self.node_id_to_u32.insert(id, next_id);
            self.u32_to_node_id.insert(next_id, id);
            next_id += 1;
        }
        self.next_u32_id = next_id;
    }

    /// Rebuilds the inverted dependency index for the document's variables.
    pub fn rebuild_dependency_index(&mut self) {
        self.dependency_index.clear();
        for node in self.dom.nodes() {
            let id = node.id();
            let val = node.value();
            if let Some(text) = val.attributes.get("content") {
                let vars = crate::render::text_engine::extract_placeholders(text);
                for var in vars {
                    if let Some(sym) = self.store.interner.get(&var) {
                        self.dependency_index.entry(sym).or_default().push(id);
                    }
                }
            }
        }
    }

    /// Triggers the logic worker reload event to reset this tab's remote state.
    ///
    /// Takes the sender explicitly because the channel is window-level (one
    /// shared worker for every tab) while everything else this reads is
    /// per-tab.
    pub fn trigger_logic_reload(&self, logic_tx: &std::sync::mpsc::Sender<(TabId, UiEvent)>) {
        let mut click_actions = HashMap::new();
        let mut submit_actions = HashMap::new();

        for node in self.dom.nodes() {
            let id = node.id();
            if let Some(&u32_id) = self.node_id_to_u32.get(&id) {
                for event_block in node.value().events.values() {
                    match event_block {
                        EventBlock::Click { action } => {
                            click_actions.insert(u32_id, action.clone());
                        }
                        EventBlock::Submit { action } => {
                            submit_actions.insert(u32_id, action.clone());
                        }
                    }
                }
            }
        }

        // Measured, not assumed: a full parse of this repo's largest example
        // document interns only 21 symbols (~458 bytes estimated clone
        // payload) -- see StringInterner's Clone impl for the measurement
        // method and the reasoning for not optimizing this call. This fires
        // on document reload, not per frame/interaction.
        let interner = self.store.interner.clone();

        let mut initial_variables = Vec::new();
        for (&sym, val) in &self.store.evaluator.global_store {
            if !matches!(val, crate::core::types::Value::Null)
                && let Some(name) = self.store.interner.resolve(sym)
            {
                initial_variables.push((name.to_string(), val.clone()));
            }
        }

        let _ = logic_tx.send((
            self.id,
            UiEvent::Reload(Box::new(ReloadPayload {
                logic_fns: self.logic_fns.clone(),
                click_actions,
                submit_actions,
                root_timer_actions: self
                    .root_timers
                    .iter()
                    .map(|rt| rt.action.clone())
                    .collect(),
                interner,
                initial_variables,
                url_registry: self.url_registry.clone(),
                // For file:// documents, relative `api` endpoints resolve against
                // localhost — the only meaningful host during local development
                // (get_raw_domain would yield a filesystem-derived token that is
                // not a routable hostname).
                document_domain: if self.chrome_state.committed_url.starts_with("file://") {
                    "localhost".to_string()
                } else {
                    get_raw_domain(&self.chrome_state.committed_url)
                },
                computed_bindings: self.computed_bindings.clone(),
            })),
        ));
    }

    /// Marks a node's cached text layout stale (after typing or a focus change
    /// that swaps placeholder ↔ value rendering) and schedules a layout pass on
    /// the next `AboutToWait` tick via `typing_layout_dirty`.
    pub fn mark_text_dirty(&mut self, id: EgoNodeId) {
        self.dirty_nodes.insert(id);
        if let Some(&taffy_id) = self.node_to_taffy_id.get(&id) {
            let _ = self.taffy.mark_dirty(taffy_id);
        }
        self.typing_layout_dirty = true;
    }

    /// Resets this tab's redirect hop counter.  Called whenever a navigation is
    /// initiated by the user (or a logic action) and when one completes, so the
    /// [`MAX_REDIRECTS`] budget applies per navigation chain, not globally —
    /// and, per invariant T1, per tab rather than per process.
    pub fn reset_redirect_count(&mut self) {
        self.redirect_count = 0;
    }

    /// Registers a single redirect hop.  Returns `true` if navigation may
    /// proceed, or `false` once [`MAX_REDIRECTS`] has been exceeded — in which
    /// case the caller must stop re-navigating.
    pub fn register_redirect(&mut self) -> bool {
        self.redirect_count += 1;
        self.redirect_count <= *MAX_REDIRECTS
    }

    /// Admission gate for a root-timer tick: `true` if one may be dispatched to
    /// the logic worker now, recording it as in flight.
    ///
    /// The channel to the worker is unbounded, and deliberately so — a `Click`,
    /// a `SubmitForm`, a network `UpdateVariable` or a `Reload` must never be
    /// dropped, because each carries state the document would otherwise lose.
    /// Timer ticks are the one event class where dropping is not only safe but
    /// correct (a tick that could not be serviced is a tick that did not
    /// happen), and they are also the only class a document controls the rate
    /// of. So the bound lives here, at the one source that needs it, instead of
    /// on the channel where it would silently discard the other four.
    ///
    /// Without it the producer is decoupled from the consumer: each tick costs
    /// the worker a full action execution plus a computed-binding recompute,
    /// so a document that outruns it — trivial, since [`MAX_ROOT_TIMERS`]
    /// independent timers may each fire every 16 ms — grows the queue without
    /// limit, and the UI thread then drains an unbounded backlog in a single
    /// frame. `MAX_INSTRUCTIONS` does not help: it bounds one execution, never
    /// how many are demanded per second.
    ///
    /// [`MAX_ROOT_TIMERS`]: crate::parser::logic::MAX_ROOT_TIMERS
    pub fn may_dispatch_timer_tick(&mut self) -> bool {
        if self.inflight_timer_ticks >= MAX_INFLIGHT_TIMER_TICKS {
            return false;
        }
        self.inflight_timer_ticks += 1;
        true
    }

    /// Records that the worker answered for this tab, freeing tick capacity.
    ///
    /// Called for *every* drained [`WorkerResponse`], not only for ones a timer
    /// produced. That deliberate imprecision is what makes the counter
    /// self-healing rather than merely approximate: an event whose response the
    /// tab never sees (worker-side state not yet created, a response dropped
    /// with a closed tab) would otherwise pin the counter high forever and
    /// silently stop the document's timers for good. Miscounting can only ever
    /// release capacity early — never withhold it — so the failure mode is a
    /// slightly looser bound, never a wedged document. The counter saturates at
    /// zero because responses are not in 1:1 correspondence with ticks.
    pub fn release_timer_tick(&mut self) {
        self.inflight_timer_ticks = self.inflight_timer_ticks.saturating_sub(1);
    }

    /// Clears the in-flight tick accounting. Called on document reload, where
    /// the worker's per-tab state is rebuilt from scratch and any tick still
    /// outstanding against the *previous* document can never be answered.
    pub fn reset_timer_ticks(&mut self) {
        self.inflight_timer_ticks = 0;
    }

    /// Read-only data sources handed to the inspector's row builder.
    ///
    /// Every source here is per-tab, so this needs no window-level borrow at
    /// all — the inspector always describes exactly one document.
    pub fn inspector_sources(&self) -> crate::render::inspector::model::InspectorSources<'_> {
        crate::render::inspector::model::InspectorSources {
            dom: &self.dom,
            taffy: &self.taffy,
            node_to_taffy_id: &self.node_to_taffy_id,
            style_rules: &self.style_rules,
            store: &self.store,
            logic_fns: &self.logic_fns,
            computed_bindings: &self.computed_bindings,
            url_registry: &self.url_registry,
            root_timers: &self.root_timers,
            root_timer_queue: &self.root_timer_queue,
            capability_policy: &self.capability_policy,
            log: &self.inspector_log,
            recent_mutations: &self.recent_mutations,
            each_expansion: &self.each_expansion,
        }
    }
}
