//! `MizuWindowManager` and its lifecycle/state methods.

use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};

use crate::render::chrome_vello::ChromeState;
use ego_tree::{NodeId as EgoNodeId, Tree};
use taffy::{TaffyTree, geometry::Size, style::AvailableSpace};
use winit::window::Window;

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol, Value, VariableStore};
use crate::network::{ReloadPayload, RuntimeAction, UiEvent, WorkerResponse};
use crate::parser::logic::{ComputedBinding, MizuFunction, RootTimer, TimerInterval};
use crate::parser::style::StyleVariant;
use crate::parser::{Action, EventBlock, MizuNode, StyleRules};
use crate::render::chrome_vello::CHROME_HEIGHT;
use crate::render::layout_bridge::{EachExpansion, expand_each_nodes};
use crate::render::responsive::{RenderEnvironment, ViewportSize};
use crate::render::security::get_raw_domain;
use super::AssetSlot;
use super::history::HistoryStack;
use crate::render::preferences::UserPreferences;
use crate::render::security::CapabilityPolicy;

/// Maximum number of consecutive server redirects honoured for a single
/// user-initiated navigation before the chain is aborted.  Prevents a hostile
/// or misconfigured server from trapping the client in an infinite redirect
/// loop.
pub(super) static MAX_REDIRECTS: LazyLock<u32> =
    LazyLock::new(|| crate::core::config::CONFIG.max_redirects);

/// Encapsulates the application state, DOM, and Layout definitions.
pub struct MizuWindowManager {
    /// The active winit window instance.
    pub window: Option<Arc<Window>>,
    /// The unlinked DOM tree.
    pub dom: Tree<MizuNode>,
    /// The active CSS rules map.
    pub style_rules: HashMap<String, StyleRules>,
    /// Breakpoint/color-scheme style variants (ux-6) — see
    /// `docs/design/responsive.md`. Resolved against `viewport_size` and
    /// `preferences.color_scheme` on every taffy-tree (re)build.
    pub style_variants: Vec<StyleVariant>,
    /// The content viewport size (window size, `height` excluding the chrome
    /// bar) last used to build `taffy`. Updated by `resize_viewport`; feeds
    /// `vw`/`vh`/`vmin`/`vmax` resolution and `@min-width`/`@max-width`
    /// variant selection.
    pub viewport_size: ViewportSize,
    /// The taffy layout engine instance.
    pub taffy: TaffyTree<EgoNodeId>,
    /// Mapping of DOM Node IDs to Taffy Node IDs.
    pub node_to_taffy_id: HashMap<EgoNodeId, taffy::prelude::NodeId>,
    /// The Taffy ID of the root node.
    pub root_taffy_id: taffy::prelude::NodeId,
    /// Parley font context.
    pub font_cx: parley::FontContext,
    /// Parley layout context.
    pub layout_cx: parley::LayoutContext<vello::peniko::Color>,
    /// The runtime variable store for state.
    pub store: VariableStore,
    /// The set of functions defined in the logic block.
    pub logic_fns: FxHashMap<Symbol, MizuFunction>,
    /// Logical scroll offsets for each container (in pixels).
    pub scroll_offsets: HashMap<EgoNodeId, f32>,
    /// Async-compatible sender for commands to the background network thread.
    pub network_tx: tokio::sync::mpsc::UnboundedSender<crate::network::NetworkCmd>,
    /// Currently focused node for text input.
    pub focused_node: Option<EgoNodeId>,
    /// Chrome bar UI state (URL, cursor, selection, focus, loading).
    pub chrome_state: ChromeState,
    /// Vertical scroll offset of the root document (logical pixels).
    pub root_scroll_offset_y: f32,
    /// Keyboard modifiers state.
    pub modifiers: winit::keyboard::ModifiersState,
    /// Cache for decoded images used in `background-image` and `image` tags.
    pub image_cache: HashMap<String, AssetSlot>,
    /// Track currently fetching images to avoid duplicate requests.
    pub fetching_images: std::collections::HashSet<String>,
    /// Global start time of the engine for animations.
    pub start_time: std::time::Instant,
    /// Last layout calculation time
    pub last_layout_time: std::time::Instant,
    /// Pending resize dimensions
    pub pending_resize: Option<(f32, f32)>,
    /// Sender to the dedicated logic worker thread.
    pub logic_tx: std::sync::mpsc::Sender<UiEvent>,
    /// Receiver for updates from the logic worker thread.
    pub logic_rx: std::sync::mpsc::Receiver<Result<WorkerResponse, MizuError>>,
    /// Bidirectional node mapping: EgoNodeId to u32.
    pub node_id_to_u32: HashMap<EgoNodeId, u32>,
    /// Bidirectional node mapping: u32 to EgoNodeId.
    pub u32_to_node_id: HashMap<u32, EgoNodeId>,
    /// Next u32 allocator for the bidirectional node mapping.
    pub next_u32_id: u32,
    /// Inverted dependency index mapping global variables to the DOM nodes that depend on them.
    pub dependency_index: HashMap<crate::core::types::Symbol, Vec<EgoNodeId>>,
    /// Cache of Parley text layouts.
    pub text_layouts: HashMap<EgoNodeId, parley::Layout<vello::peniko::Color>>,
    /// Cache of text dimensions.
    pub text_dimensions: HashMap<EgoNodeId, (f32, f32)>,
    /// Set of DOM nodes that have dirty visual text state.
    pub dirty_nodes: std::collections::HashSet<EgoNodeId>,
    /// Flag indicating that layout recalculation was deferred due to text typing.
    pub typing_layout_dirty: bool,
    /// Current values of locally typed text fields.
    /// NOT sent to the worker during typing — only collected on Submit.
    pub local_inputs: FxHashMap<u32, String>,
    /// URL registry — compile-time endpoint aliases resolved from the `urls` block.
    pub url_registry: crate::parser::UrlRegistry,
    /// Expanded Taffy subtrees for every `Each` node in the document.
    /// Rebuilt by [`expand_each_nodes`] at the start of each `resize_viewport`
    /// call so that layout always reflects the current list lengths.
    pub each_expansion: EachExpansion,
    /// Number of consecutive redirects followed since the last user-initiated
    /// navigation.  Reset to 0 when the user starts a navigation and when a
    /// navigation completes successfully; capped by [`MAX_REDIRECTS`].
    pub redirect_count: u32,
    /// Computed (derived) variable bindings in topological order.
    pub computed_bindings: Vec<ComputedBinding>,
    /// Whether the most recent user interaction was a qualifying gesture
    /// (e.g. a mouse click) that activates clipboard access.  Cleared after
    /// each action batch is processed.
    pub has_user_gesture: bool,
    /// Per-origin capability budget (storage quota + rate limit).  Reset on
    /// every navigation so that cross-origin documents cannot inherit each
    /// other's budgets.
    pub capability_policy: CapabilityPolicy,
    /// Bounded tokio receiver for [`crate::network::NetworkResult`] messages
    /// from the network worker.  Drained each frame via `try_recv()` so the UI
    /// thread never blocks on network I/O.
    pub network_rx: tokio::sync::mpsc::Receiver<crate::network::NetworkResult>,
    /// Root-level `timer` declarations from the `logic` block, in declaration
    /// order.  Fired by the UI clock; actions execute in the logic worker.
    pub root_timers: Vec<RootTimer>,
    /// Priority queue of pending root-timer deadlines (deadline → indices
    /// into `root_timers`).  Rebuilt by [`Self::setup_timers`].
    pub root_timer_queue: BTreeMap<std::time::Instant, Vec<usize>>,
    /// Live inspector UI state (panel visibility, tab, selection, scroll).
    pub inspector: crate::render::inspector::InspectorState,
    /// Always-on bounded log of runtime events and network activity, consumed
    /// by the inspector's Events and Network tabs.
    pub inspector_log: crate::render::inspector::log::InspectorLog,
    /// Instant of the most recent mutation per variable, used by the
    /// inspector's Logic tab to flash freshly-changed values.  Bounded by the
    /// interner size (entries are overwritten, never accumulated).
    pub recent_mutations: FxHashMap<Symbol, std::time::Instant>,
    /// In-memory session history (Back/Forward stacks). See `window::history`.
    pub history: HistoryStack,
    /// Scroll offset to restore once the in-flight history navigation's
    /// document finishes loading. Set by `navigate_back`/`navigate_forward`,
    /// consumed (and cleared) in `handle_navigate_success`; also cleared on
    /// any non-history navigation and on a blocked verdict so a failed or
    /// unrelated navigation can never apply a stale restore.
    pub pending_scroll_restore: Option<f32>,
    /// Detected OS appearance/accessibility preferences (ux-5): light/dark
    /// is real (from `winit::window::Theme`); high-contrast/reduced-motion
    /// are always `false` today — see `render::preferences`'s module doc.
    /// Read by the chrome paint path to choose a `ChromePalette`.
    pub preferences: UserPreferences,
}

impl MizuWindowManager {
    /// Creates a new manager by compiling the DOM styles into Taffy layout components.
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
        let mut taffy = TaffyTree::new();
        let mut node_to_taffy_id = HashMap::new();

        let empty_cache = HashMap::new();
        let default_chrome_url = "mizu://localhost/index.mizu";
        // Placeholder viewport: the real window doesn't exist yet at this
        // point in startup. `resize_viewport` rebuilds the taffy tree's
        // styles against the real size as soon as the window is created
        // (see `event_loop::run_window_loop`), so this is superseded within
        // the same startup sequence, before the first frame ever paints.
        let initial_env = RenderEnvironment {
            viewport: ViewportSize {
                width: 800.0,
                height: 600.0 - CHROME_HEIGHT,
            },
            color_scheme: UserPreferences::default().color_scheme,
        };
        let root_taffy_id = crate::render::layout_bridge::build_taffy_tree(
            dom.root(),
            &style_rules,
            &mut taffy,
            &mut node_to_taffy_id,
            &empty_cache,
            default_chrome_url,
            &style_variants,
            &initial_env,
        )?;

        let (network_tx, rx) = tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let (tx, network_rx) =
            tokio::sync::mpsc::channel(*crate::network::worker::MAX_UI_CHANNEL_CAPACITY);
        crate::network::worker::spawn_network_thread(
            rx,
            tx,
            #[cfg(feature = "insecure-dev")]
            allow_insecure,
        );

        let (logic_tx, logic_worker_rx) = std::sync::mpsc::channel();
        let (logic_worker_tx, logic_rx) = std::sync::mpsc::channel();
        crate::parser::logic_worker::LogicWorker::spawn(logic_worker_rx, logic_worker_tx)?;

        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();

        let mut manager = Self {
            window: None,
            dom,
            style_rules,
            style_variants,
            viewport_size: initial_env.viewport,
            taffy,
            node_to_taffy_id,
            root_taffy_id,
            font_cx,
            layout_cx: parley::LayoutContext::new(),
            store: VariableStore::new(),
            logic_fns,
            scroll_offsets: HashMap::new(),
            network_tx,
            focused_node: None,
            chrome_state: ChromeState::default(),
            root_scroll_offset_y: 0.0,
            modifiers: winit::keyboard::ModifiersState::default(),
            image_cache: HashMap::new(),
            fetching_images: std::collections::HashSet::new(),
            start_time: std::time::Instant::now(),
            last_layout_time: std::time::Instant::now(),
            pending_resize: None,
            logic_tx,
            logic_rx,
            node_id_to_u32: HashMap::new(),
            u32_to_node_id: HashMap::new(),
            next_u32_id: 0,
            dependency_index: HashMap::new(),
            text_layouts: HashMap::new(),
            text_dimensions: HashMap::new(),
            dirty_nodes: std::collections::HashSet::new(),
            typing_layout_dirty: false,
            local_inputs: FxHashMap::default(),
            url_registry: rustc_hash::FxHashMap::default(),
            each_expansion: EachExpansion::default(),
            redirect_count: 0,
            computed_bindings: Vec::new(),
            has_user_gesture: false,
            capability_policy: CapabilityPolicy::new(default_chrome_url),
            network_rx,
            root_timers: Vec::new(),
            root_timer_queue: BTreeMap::new(),
            inspector: crate::render::inspector::InspectorState::new(),
            inspector_log: crate::render::inspector::log::InspectorLog::new(),
            recent_mutations: FxHashMap::default(),
            history: HistoryStack::default(),
            pending_scroll_restore: None,
            preferences: UserPreferences::default(),
        };

        manager.rebuild_node_mappings();
        manager.rebuild_dependency_index();
        manager.trigger_logic_reload();
        manager.setup_timers();
        Ok(manager)
    }

    /// Resolves a root-timer interval to milliseconds, clamped to ≥ 16 ms.
    ///
    /// Variable intervals are read from the store; an unset or non-numeric
    /// variable yields `None` (the timer is skipped until the variable exists).
    pub(super) fn resolve_root_timer_interval(&self, interval: &TimerInterval) -> Option<u64> {
        let ms = match interval {
            TimerInterval::Millis(ms) => *ms,
            TimerInterval::Variable(var_name) => match self.store.get(var_name).ok() {
                
                Some(Value::Int(i)) => (*i / crate::core::types::DECIMAL_SCALE) as u64,
                _ => return None,
            },
        };
        Some(ms.max(16))
    }

    /// Setup the timer priority queue from the document's root `timer`
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
    pub fn rebuild_node_mappings(&mut self) {
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
                    let sym = self.store.interner.get_or_intern(&var);
                    self.dependency_index.entry(sym).or_default().push(id);
                }
            }
        }
    }

    /// Triggers the logic worker reload event to reset the remote state.
    pub fn trigger_logic_reload(&self) {
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

        let mut interner = self.store.interner.clone();
        for node in self.dom.nodes() {
            for event in node.value().events.values() {
                match event {
                    EventBlock::Click { action } | EventBlock::Submit { action } => {
                        if let Action::Assign { target, .. } = action {
                            interner.get_or_intern(target);
                        }
                    }
                }
            }
        }
        if !submit_actions.is_empty() {
            // The `$form` magic record must survive the interner freeze so
            // the logic worker can populate it on submission.
            interner.get_or_intern("$form");
        }
        // Root-timer assign targets must also survive the freeze.
        for rt in &self.root_timers {
            if let Action::Assign { target, .. } = &rt.action {
                interner.get_or_intern(target);
            }
        }

        let mut initial_variables = Vec::new();
        for (&sym, val) in &self.store.state_machine.global_store {
            if !matches!(val, crate::core::types::Value::Null)
                && let Some(name) = self.store.interner.resolve(sym)
            {
                initial_variables.push((name.to_string(), val.clone()));
            }
        }

        let _ = self.logic_tx.send(UiEvent::Reload(Box::new(ReloadPayload {
            logic_fns: self.logic_fns.clone(),
            click_actions,
            submit_actions,
            root_timer_actions: self.root_timers.iter().map(|rt| rt.action.clone()).collect(),
            interner,
            initial_variables,
            url_registry: self.url_registry.clone(),
            // For file:// documents, relative `api` endpoints resolve against
            // localhost — the only meaningful host during local development
            // (get_raw_domain would yield a filesystem-derived token that is
            // not a routable hostname).
            document_domain: if self.chrome_state.url.starts_with("file://") {
                "localhost".to_string()
            } else {
                get_raw_domain(&self.chrome_state.url)
            },
            computed_bindings: self.computed_bindings.clone(),
        })));
    }

    /// Reloads the document completely, resetting layout and logic state.
    #[allow(clippy::too_many_arguments)]
    pub fn reload_document(
        &mut self,
        dom: Tree<MizuNode>,
        style_rules: HashMap<String, StyleRules>,
        style_variants: Vec<StyleVariant>,
        logic_fns: FxHashMap<Symbol, MizuFunction>,
        interner: StringInterner,
        computed_bindings: Vec<ComputedBinding>,
        root_timers: Vec<RootTimer>,
    ) -> Result<(), MizuError> {
        self.root_timers = root_timers;
        // Old node ids die with the old tree — drop inspector selection state.
        self.inspector.reset_document_state();
        self.recent_mutations.clear();
        let mut taffy = TaffyTree::new();
        let mut node_to_taffy_id = HashMap::new();

        let env = RenderEnvironment {
            viewport: self.viewport_size,
            color_scheme: self.preferences.color_scheme,
        };
        let root_taffy_id = crate::render::layout_bridge::build_taffy_tree(
            dom.root(),
            &style_rules,
            &mut taffy,
            &mut node_to_taffy_id,
            &self.image_cache,
            &self.chrome_state.url,
            &style_variants,
            &env,
        )?;

        self.dom = dom;
        // Keep the OS window title in sync with the newly loaded document's
        // `window "..."` title attribute (falls back to the same default
        // used at startup, matching `render::window::event_loop`).
        if let Some(window) = self.window.as_ref() {
            let title = self
                .dom
                .root()
                .value()
                .attributes
                .get("title")
                .cloned()
                .unwrap_or_else(|| "Mizu Application".to_string());
            window.set_title(&title);
        }
        self.style_rules = style_rules;
        self.style_variants = style_variants;
        self.logic_fns = logic_fns;
        self.computed_bindings = computed_bindings;
        self.taffy = taffy;
        self.node_to_taffy_id = node_to_taffy_id;
        self.root_taffy_id = root_taffy_id;

        self.scroll_offsets.clear();
        self.root_timer_queue.clear();
        self.focused_node = None;
        self.root_scroll_offset_y = 0.0;
        self.chrome_state.focused = false;
        self.chrome_state.selection = None;
        self.text_layouts.clear();
        self.text_dimensions.clear();
        self.dirty_nodes.clear();
        self.local_inputs.clear();
        // The new Taffy tree has fresh node IDs; the old synthetic IDs are invalid.
        self.each_expansion = EachExpansion::default();

        self.rebuild_node_mappings();
        self.store = VariableStore::with_interner(interner);
        self.store
            .set("window_url", Value::from(self.chrome_state.url.clone()));
        self.rebuild_dependency_index();

        self.trigger_logic_reload();
        // Freeze the UI interner so any runtime symbol additions (network results,
        // form fields not declared in logic) are flagged in logs — the logic worker
        // already holds a pre-freeze clone, so post-freeze symbols would diverge
        // between threads if they were ever used as raw IDs in inter-thread messages.
        self.store.interner.freeze();

        self.setup_timers();
        Ok(())
    }

    /// Recomputes the entire Taffy layout relative to the newly requested viewport boundary.
    pub fn resize_viewport(&mut self, width: f32, height: f32) -> Result<(), MizuError> {
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }

        // The docked inspector panel reduces the document's usable width.
        // Centralised here so every call site (resize, F12 toggle, timers)
        // automatically lays the document out in the remaining space.
        let width = if self.inspector.open {
            (width - crate::render::inspector::PANEL_WIDTH).max(120.0)
        } else {
            width
        };

        let content_height = (height - CHROME_HEIGHT).max(0.0);
        let viewport_size = Size {
            width: AvailableSpace::Definite(width),
            height: AvailableSpace::MaxContent,
        };

        // ux-6: re-resolve breakpoint/color-scheme variants and vw/vh/vmin/
        // vmax dimensions against the new content viewport before laying
        // out. This rebuilds the taffy tree's *styles* (not the DOM/logic
        // state) — the same construction `reload_document` uses, so a
        // resize's responsive re-styling is exactly as correct as a fresh
        // document load, just without re-parsing anything. Bounded by the
        // same ≥16ms debounce this function is already only called behind
        // (see `window::event_loop`'s `WindowEvent::Resized` handler) — "not
        // on every resize pixel", per the design memo.
        self.viewport_size = ViewportSize {
            width,
            height: content_height,
        };
        let env = RenderEnvironment {
            viewport: self.viewport_size,
            color_scheme: self.preferences.color_scheme,
        };
        let mut new_taffy = TaffyTree::new();
        let mut new_node_to_taffy_id = HashMap::new();
        let new_root_taffy_id = crate::render::layout_bridge::build_taffy_tree(
            self.dom.root(),
            &self.style_rules,
            &mut new_taffy,
            &mut new_node_to_taffy_id,
            &self.image_cache,
            &self.chrome_state.url,
            &self.style_variants,
            &env,
        )?;
        self.taffy = new_taffy;
        self.node_to_taffy_id = new_node_to_taffy_id;
        self.root_taffy_id = new_root_taffy_id;
        // The rebuilt tree has fresh synthetic-node bookkeeping — the old
        // each-expansion's `groups`/`original_children`/`all_synthetic_ids`
        // reference taffy node ids that no longer exist in `self.taffy`, so
        // they must not be reused (`expand_each_nodes`'s "restore the
        // previous expansion" step would otherwise operate on stale/
        // possibly-reused ids). `truncated` is keyed by `EgoNodeId`, which
        // *is* still meaningful, and is kept so the budget-change log below
        // compares against the real previous count instead of always
        // reading 0 (which would log a spurious "budget exceeded" on every
        // resize of a document with any truncated list).
        let prev_truncated = std::mem::take(&mut self.each_expansion.truncated);
        self.each_expansion = EachExpansion::default();

        if let Ok(mut style) = self.taffy.style(self.root_taffy_id).cloned() {
            style.min_size.height = taffy::style::Dimension::Length(content_height);
            style.size.height = taffy::style::Dimension::Auto;
            let _ = self.taffy.set_style(self.root_taffy_id, style);
        }

        // Expand `Each` nodes in Taffy to match the current list lengths.
        // Must run before `compute_layout_with_measure` so Taffy sees the
        // full N-row tree and produces correct per-item positions.
        let new_expansion = expand_each_nodes(
            &self.dom,
            &self.store,
            &mut self.taffy,
            &self.node_to_taffy_id,
            &self.each_expansion,
        )?;

        for (node_id, &new_count) in &new_expansion.truncated {
            let old_count = prev_truncated.get(node_id).copied().unwrap_or(0);
            if new_count != old_count {
                let msg = format!("budget exceeded: clamped list to hide {} items", new_count);
                self.inspector_log.push_event(crate::render::inspector::log::EventKind::Layout, msg.clone());
                tracing::warn!("{}", msg);
            }
        }
        for (node_id, &old_count) in &prev_truncated {
            if !new_expansion.truncated.contains_key(node_id) {
                let msg = format!("budget restored: previously clamped {} items now visible", old_count);
                self.inspector_log.push_event(crate::render::inspector::log::EventKind::Layout, msg.clone());
                tracing::warn!("{}", msg);
            }
        }

        self.each_expansion = new_expansion;

        let dom = &self.dom;
        let style_rules = &self.style_rules;
        let style_variants = &self.style_variants;
        let render_env = RenderEnvironment {
            viewport: self.viewport_size,
            color_scheme: self.preferences.color_scheme,
        };
        let font_cx = &mut self.font_cx;
        let layout_cx = &mut self.layout_cx;
        let store = &self.store;
        let text_layouts = &mut self.text_layouts;
        let text_dimensions = &mut self.text_dimensions;
        let dirty_nodes = &mut self.dirty_nodes;
        let local_inputs = &self.local_inputs;
        let node_id_to_u32 = &self.node_id_to_u32;
        let focused_input = self.focused_node;

        self.taffy
            .compute_layout_with_measure(
                self.root_taffy_id,
                viewport_size,
                |_known_dimensions, available_space, _node_id, node_context, _style| {
                    if let Some(ego_id) = node_context {
                        let node_id = *ego_id;
                        if !dirty_nodes.contains(&node_id)
                            && let Some(&(w, h)) = text_dimensions.get(&node_id)
                        {
                            return taffy::geometry::Size {
                                width: w,
                                height: h,
                            };
                        }

                        let available_width = match available_space.width {
                            AvailableSpace::Definite(w) => Some(w),
                            AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                        };

                        if let Some((dims, layout)) =
                            crate::render::text_engine::calculate_node_text(
                                node_id,
                                dom,
                                style_rules,
                                font_cx,
                                layout_cx,
                                store,
                                available_width,
                                local_inputs,
                                node_id_to_u32,
                                focused_input,
                                style_variants,
                                &render_env,
                            )
                        {
                            text_dimensions.insert(node_id, dims);
                            text_layouts.insert(node_id, layout);
                            dirty_nodes.remove(&node_id);
                            return taffy::geometry::Size {
                                width: dims.0,
                                height: dims.1,
                            };
                        }
                    }
                    taffy::geometry::Size::ZERO
                },
            )
            .map_err(|e| MizuError::ParseError(format!("Layout computation error: {:?}", e)))?;

        Ok(())
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

    /// Resets the redirect hop counter.  Called whenever a navigation is
    /// initiated by the user (or a logic action) and when one completes, so the
    /// [`MAX_REDIRECTS`] budget applies per navigation chain, not globally.
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

    /// Executes a declarative capability action, recording network-visible
    /// dispatches (and policy blocks) in the inspector log.
    pub fn execute_capability_action(&mut self, action: RuntimeAction) {
        use crate::render::inspector::log::NetOutcome;
        use crate::render::security::CapabilityOutcome;

        // Describe network-visible actions before the action is moved.
        let described: Option<(String, String, Option<String>)> = match &action {
            RuntimeAction::ResolvedCall {
                method,
                url,
                target_variable,
                ..
            } => Some((method.clone(), url.clone(), Some(target_variable.0.to_string()))),
            RuntimeAction::StoreLocal { key, .. } => {
                Some(("STORE".to_string(), key.clone(), None))
            }
            RuntimeAction::DownloadMedia { url } => {
                Some(("MEDIA".to_string(), url.clone(), None))
            }
            _ => None,
        };

        let outcome = crate::render::security::execute_capability_action(
            &mut self.store,
            &self.network_tx,
            &self.logic_tx,
            &self.chrome_state.url,
            &mut self.capability_policy,
            action,
        );

        if let Some((verb, target, correlation)) = described {
            match outcome {
                CapabilityOutcome::Blocked(reason) => {
                    self.inspector_log.push_net_blocked(&verb, &target, reason);
                }
                CapabilityOutcome::Dispatched => {
                    if verb == "STORE" {
                        // Fire-and-forget: no completion message flows back.
                        self.inspector_log
                            .push_net_done(&verb, &target, NetOutcome::Ok);
                    } else {
                        self.inspector_log.push_net_start(&verb, &target, correlation);
                    }
                }
            }
        }
    }

    /// Read-only data sources handed to the inspector's row builder.
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
