#![forbid(unsafe_code)]

use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::render::chrome_vello::{
    CHROME_HEIGHT, ChromeHitZone, ChromeKeyAction, ChromeState, chrome_hit_zone, paint_chrome,
    url_text_left,
};
use ego_tree::{NodeId as EgoNodeId, Tree};
use taffy::{TaffyTree, geometry::Size, style::AvailableSpace};
use vello::{AaConfig, Renderer, RendererOptions, Scene, kurbo::Affine, util::RenderContext};
use winit::{
    event::{Event, MouseScrollDelta, WindowEvent},
    keyboard::NamedKey,
    window::{Window, WindowBuilder},
};

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol, Value, VariableStore};
use crate::network::{ReloadPayload, RuntimeAction, UiEvent, WorkerResponse};
use crate::parser::logic::{ComputedBinding, MizuFunction};
use crate::parser::{Action, EventBlock, MizuNode, MizuOverflow, Primitive, StyleRules};
use crate::render::hit_test::hit_test;
use crate::render::layout_bridge::{EachExpansion, expand_each_nodes};
use crate::render::security::{CapabilityPolicy, get_raw_domain};
use crate::render::vello_pipeline::{PaintContext, paint_node};

pub use crate::render::image_codec::{AnimatedImage, AssetSlot, Frame, decode_image_bytes};

/// Maximum number of consecutive server redirects honoured for a single
/// user-initiated navigation before the chain is aborted.  Prevents a hostile
/// or misconfigured server from trapping the client in an infinite redirect
/// loop.
const MAX_REDIRECTS: u32 = 10;

/// Encapsulates the application state, DOM, and Layout definitions.
pub struct MizuWindowManager {
    /// The active winit window instance.
    pub window: Option<Arc<Window>>,
    /// The unlinked DOM tree.
    pub dom: Tree<MizuNode>,
    /// The active CSS rules map.
    pub style_rules: HashMap<String, StyleRules>,
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
    /// Priority queue of active timers (deadline -> list of node IDs).
    pub timer_queue: BTreeMap<std::time::Instant, Vec<EgoNodeId>>,
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
}

impl MizuWindowManager {
    /// Creates a new manager by compiling the DOM styles into Taffy layout components.
    ///
    /// `allow_insecure`: forwarded to the network thread — when `true`, TLS cert
    /// verification is skipped (development only).
    pub fn new(
        dom: Tree<MizuNode>,
        style_rules: HashMap<String, StyleRules>,
        logic_fns: FxHashMap<Symbol, MizuFunction>,
        #[cfg(feature = "insecure-dev")] allow_insecure: bool,
    ) -> Result<Self, MizuError> {
        let mut taffy = TaffyTree::new();
        let mut node_to_taffy_id = HashMap::new();

        let empty_cache = HashMap::new();
        let default_chrome_url = "mizu://localhost/index.mizu";
        let root_taffy_id = crate::render::layout_bridge::build_taffy_tree(
            dom.root(),
            &style_rules,
            &mut taffy,
            &mut node_to_taffy_id,
            &empty_cache,
            default_chrome_url,
        )?;

        let (network_tx, rx) = tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let (tx, network_rx) =
            tokio::sync::mpsc::channel(crate::network::worker::MAX_UI_CHANNEL_CAPACITY);
        crate::network::worker::spawn_network_thread(
            rx,
            tx,
            #[cfg(feature = "insecure-dev")]
            allow_insecure,
        );

        let (logic_tx, logic_worker_rx) = std::sync::mpsc::channel();
        let (logic_worker_tx, logic_rx) = std::sync::mpsc::channel();
        crate::parser::logic_worker::LogicWorker::spawn(logic_worker_rx, logic_worker_tx);

        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();

        let mut manager = Self {
            window: None,
            dom,
            style_rules,
            taffy,
            node_to_taffy_id,
            root_taffy_id,
            font_cx,
            layout_cx: parley::LayoutContext::new(),
            store: VariableStore::new(),
            logic_fns,
            scroll_offsets: HashMap::new(),
            timer_queue: BTreeMap::new(),
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
        };

        manager.rebuild_node_mappings();
        manager.rebuild_dependency_index();
        manager.trigger_logic_reload();
        manager.setup_timers();
        Ok(manager)
    }

    /// Setup the timer priority queue from the dom tree.
    pub fn setup_timers(&mut self) {
        self.timer_queue.clear();
        let now = std::time::Instant::now();
        for node_ref in self.dom.nodes() {
            if let Some(EventBlock::Every { interval, .. }) = node_ref.value().events.get("every") {
                let mut interval_ms = match interval {
                    crate::parser::layout::Interval::Literal(ms) => *ms,
                    crate::parser::layout::Interval::Variable(var_name) => {
                        let val = self.store.get(var_name).ok();
                        match val {
                            Some(Value::Float(f)) => *f as u64,
                            Some(Value::Int(i)) => *i as u64,
                            _ => continue,
                        }
                    }
                };
                if interval_ms < 16 {
                    interval_ms = 16;
                }
                let deadline = now + std::time::Duration::from_millis(interval_ms);
                self.timer_queue
                    .entry(deadline)
                    .or_default()
                    .push(node_ref.id());
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
        let mut every_actions = HashMap::new();

        for node in self.dom.nodes() {
            let id = node.id();
            if let Some(&u32_id) = self.node_id_to_u32.get(&id) {
                for event_block in node.value().events.values() {
                    match event_block {
                        EventBlock::Click { action } => {
                            click_actions.insert(u32_id, action.clone());
                        }
                        EventBlock::Every { action, .. } => {
                            every_actions.insert(u32_id, action.clone());
                        }
                        _ => {}
                    }
                }
            }
        }

        let mut interner = self.store.interner.clone();
        for node in self.dom.nodes() {
            for event in node.value().events.values() {
                match event {
                    EventBlock::Click { action } | EventBlock::Every { action, .. } => {
                        if let Action::Assign { target, .. } = action {
                            interner.get_or_intern(target);
                        }
                    }
                    _ => {}
                }
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
            every_actions,
            interner,
            initial_variables,
            url_registry: self.url_registry.clone(),
            document_domain: get_raw_domain(&self.chrome_state.url),
            computed_bindings: self.computed_bindings.clone(),
        })));
    }

    /// Reloads the document completely, resetting layout and logic state.
    pub fn reload_document(
        &mut self,
        dom: Tree<MizuNode>,
        style_rules: HashMap<String, StyleRules>,
        logic_fns: FxHashMap<Symbol, MizuFunction>,
        interner: StringInterner,
        computed_bindings: Vec<ComputedBinding>,
    ) -> Result<(), MizuError> {
        let mut taffy = TaffyTree::new();
        let mut node_to_taffy_id = HashMap::new();

        let root_taffy_id = crate::render::layout_bridge::build_taffy_tree(
            dom.root(),
            &style_rules,
            &mut taffy,
            &mut node_to_taffy_id,
            &self.image_cache,
            &self.chrome_state.url,
        )?;

        self.dom = dom;
        self.style_rules = style_rules;
        self.logic_fns = logic_fns;
        self.computed_bindings = computed_bindings;
        self.taffy = taffy;
        self.node_to_taffy_id = node_to_taffy_id;
        self.root_taffy_id = root_taffy_id;

        self.scroll_offsets.clear();
        self.timer_queue.clear();
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

        let content_height = (height - CHROME_HEIGHT).max(0.0);
        let viewport_size = Size {
            width: AvailableSpace::Definite(width),
            height: AvailableSpace::MaxContent,
        };

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
        self.each_expansion = new_expansion;

        let dom = &self.dom;
        let style_rules = &self.style_rules;
        let font_cx = &mut self.font_cx;
        let layout_cx = &mut self.layout_cx;
        let store = &self.store;
        let text_layouts = &mut self.text_layouts;
        let text_dimensions = &mut self.text_dimensions;
        let dirty_nodes = &mut self.dirty_nodes;

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
        self.redirect_count <= MAX_REDIRECTS
    }

    /// Executes a declarative capability action.
    pub fn execute_capability_action(&mut self, action: RuntimeAction) {
        crate::render::security::execute_capability_action(
            &mut self.store,
            &self.network_tx,
            &self.logic_tx,
            &self.chrome_state.url,
            &mut self.capability_policy,
            action,
        );
    }
}

/// Resolves and validates a navigation URL given the current document's URL.
///
/// Returns `None` if the navigation is blocked:
/// * A `mizu://` document attempting to navigate to a `file://` resource.
/// * A `file://` document attempting to navigate outside its **Sandbox Base
///   Directory** (the parent folder of the currently-loaded document) via a
///   relative path containing `..` or via an absolute `file://` URL that
///   points outside the sandbox.
///
/// Returns `Some(resolved_url)` otherwise:
/// * Relative paths from a `file://` document are resolved to absolute
///   `file://` URLs using the sandbox base directory.
/// * Bare hostnames / paths with no scheme are normalised to `mizu://`.
///
/// Note: `http://` and `https://` are not valid Mizu schemes and are rejected
/// at dispatch time in `navigate_to_url` before they can ever become the
/// current URL.
pub(crate) fn resolve_navigate_url(current_url: &str, target: &str) -> Option<String> {
    let origin_is_remote = current_url.starts_with("mizu://");
    let origin_is_file = current_url.starts_with("file://");

    // Block A: remote document must not navigate to local files.
    if origin_is_remote && target.starts_with("file://") {
        return None;
    }

    let mut url = target.to_owned();

    // Block B: resolve relative paths from a local-file document.
    if !url.contains("://") && origin_is_file {
        let current = current_url.strip_prefix("file:///").unwrap_or(current_url);
        let current_path = std::path::Path::new(current);
        let base_dir = current_path.parent().unwrap_or(std::path::Path::new("."));
        let resolved = base_dir.join(&url);

        // Fail-closed sandbox check: the resolved path must stay inside base_dir.
        if !crate::render::security::file_sandbox_contains(base_dir, &resolved) {
            tracing::warn!(
                current = %current_url,
                target = %url,
                "SecurityViolation: relative path escapes file:// sandbox base directory"
            );
            return None;
        }

        let canonical = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
        let path_str = canonical.to_string_lossy().replace('\\', "/");
        return Some(format!("file:///{}", path_str));
    }

    // Block C: absolute file:// from a file:// origin — enforce sandbox.
    if origin_is_file && url.starts_with("file://") {
        let current = current_url.strip_prefix("file:///").unwrap_or(current_url);
        let current_path = std::path::Path::new(current);
        let base_dir = current_path.parent().unwrap_or(std::path::Path::new("."));
        let target_path_str = url
            .strip_prefix("file:///")
            .or_else(|| url.strip_prefix("file://"))
            .unwrap_or(url.as_str());
        let target_path = std::path::Path::new(target_path_str);

        if !crate::render::security::file_sandbox_contains(base_dir, target_path) {
            tracing::warn!(
                current = %current_url,
                target = %url,
                "SecurityViolation: absolute file:// target escapes sandbox base directory"
            );
            return None;
        }
        return Some(url);
    }

    // Normalise bare hostname/path (no scheme) to mizu://.
    if !url.contains("://") {
        url = format!("mizu://{}", url);
    }

    Some(url)
}

/// Returns the sandbox base directory for a `file://` document URL, or `None`
/// for non-`file://` origins.
///
/// The sandbox base is the parent directory of the currently-loaded document.
/// All local asset fetches from this document are restricted to this subtree.
pub(crate) fn chrome_url_to_file_sandbox_base(chrome_url: &str) -> Option<String> {
    let file_path = chrome_url.strip_prefix("file:///")?;
    std::path::Path::new(file_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Applies a successfully-fetched document: parses all blocks from `source`
/// and reloads the manager's DOM, styles, logic, and URL registry.
///
/// Called both from the file:// fast path in `navigate_to_url` (synchronous
/// disk read) and from `process_network_result` (async QUIC fetch result).
fn handle_navigate_success(manager: &mut MizuWindowManager, url: String, source: String) {
    tracing::debug!(url = %url, "navigate success");
    manager.chrome_state.url = url.clone();
    manager.chrome_state.loading = false;
    manager.reset_redirect_count();
    manager
        .store
        .set("window_url", crate::core::types::Value::from(url.clone()));

    let current_dir = std::env::current_dir().unwrap_or_default();
    match crate::parser::split_source_with_origin(
        &source,
        &current_dir,
        crate::parser::Origin::Network,
    ) {
        Ok(blocks) => {
            let mut new_interner = crate::core::types::StringInterner::new();
            let logic_fns = if !blocks.logic_block.trim().is_empty() {
                match crate::parser::logic::parse_logic(&blocks.logic_block, &mut new_interner) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(error = ?e, "logic parse error during navigation");
                        FxHashMap::default()
                    }
                }
            } else {
                FxHashMap::default()
            };
            let new_computed = if !blocks.logic_block.trim().is_empty() {
                match crate::parser::logic::parse_computed(&blocks.logic_block, &mut new_interner) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!(error = ?e, "computed parse error during navigation");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let style_rules = if !blocks.style_block.trim().is_empty() {
                match crate::parser::style::parse_style(&blocks.style_block) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = ?e, "style parse error during navigation");
                        HashMap::new()
                    }
                }
            } else {
                HashMap::new()
            };
            let new_url_registry = if !blocks.urls_block.trim().is_empty() {
                match crate::parser::urls::parse_urls(&blocks.urls_block, &mut new_interner) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(error = ?e, "urls parse error during navigation");
                        rustc_hash::FxHashMap::default()
                    }
                }
            } else {
                rustc_hash::FxHashMap::default()
            };
            match crate::parser::layout::parse_layout_with_urls(
                &blocks.layout_block,
                &mut new_interner,
                Some(&new_url_registry),
                url.starts_with("mizu://"),
            ) {
                Ok(dom) => {
                    manager.url_registry = new_url_registry;
                    if let Err(e) = manager.reload_document(
                        dom,
                        style_rules,
                        logic_fns,
                        new_interner,
                        new_computed,
                    ) {
                        tracing::error!(error = ?e, "document reload error");
                    } else {
                        tracing::debug!("document reloaded");
                    }
                }
                Err(e) => {
                    tracing::error!(error = ?e, "layout parse error during navigation");
                }
            }
        }
        Err(e) => {
            tracing::error!(error = ?e, "source split error during navigation");
        }
    }
}

/// Dispatches a single [`crate::network::NetworkResult`] received from the
/// network worker onto the manager's state.
///
/// Called from the `AboutToWait` drain loop — never from a blocking context.
fn process_network_result(manager: &mut MizuWindowManager, res: crate::network::NetworkResult) {
    use crate::network::NetworkResult;
    match res {
        NetworkResult::Success { target_var, data } => {
            let _ = manager
                .logic_tx
                .send(crate::network::UiEvent::UpdateVariable {
                    name: target_var,
                    value: data,
                });
            let _ = manager
                .logic_tx
                .send(crate::network::UiEvent::UpdateVariable {
                    name: "stato_navigazione".to_string(),
                    value: crate::core::types::Value::from("Completato.".to_string()),
                });
        }
        NetworkResult::Error(e) => {
            tracing::error!(error = ?e, "network error");
            manager.chrome_state.loading = false;
            let _ = manager
                .logic_tx
                .send(crate::network::UiEvent::UpdateVariable {
                    name: "stato_navigazione".to_string(),
                    value: crate::core::types::Value::from(format!("Errore: {e}")),
                });
        }
        NetworkResult::NavigateSuccess { url, source } => {
            handle_navigate_success(manager, url, source);
        }
        NetworkResult::Redirect { new_url } => {
            if manager.register_redirect() {
                tracing::debug!(
                    url = %new_url,
                    count = manager.redirect_count,
                    "redirecting"
                );
                manager.chrome_state.url = new_url.clone();
                manager.chrome_state.loading = true;
                let _ = manager
                    .network_tx
                    .send(crate::network::NetworkCmd::Navigate { url: new_url });
            } else {
                tracing::error!(
                    limit = MAX_REDIRECTS,
                    "redirect limit exceeded; aborting navigation"
                );
                manager.chrome_state.loading = false;
                let _ = manager
                    .logic_tx
                    .send(crate::network::UiEvent::UpdateVariable {
                        name: "stato_navigazione".to_string(),
                        value: crate::core::types::Value::from(
                            "Errore: troppi redirect".to_string(),
                        ),
                    });
            }
        }
        NetworkResult::FetchImageSuccess { url, image } => {
            manager.fetching_images.remove(&url);
            manager
                .image_cache
                .insert(url.clone(), AssetSlot::Ready(image));
            let mut new_taffy = taffy::TaffyTree::new();
            let mut new_node_map = HashMap::new();
            match crate::render::layout_bridge::build_taffy_tree(
                manager.dom.root(),
                &manager.style_rules,
                &mut new_taffy,
                &mut new_node_map,
                &manager.image_cache,
                &manager.chrome_state.url,
            ) {
                Ok(new_root) => {
                    manager.taffy = new_taffy;
                    manager.node_to_taffy_id = new_node_map;
                    manager.root_taffy_id = new_root;
                }
                Err(e) => {
                    tracing::error!(error = ?e, "taffy rebuild failed after image fetch");
                }
            }
        }
        NetworkResult::FetchImageFailed { url, error } => {
            manager.fetching_images.remove(&url);
            manager.image_cache.insert(url.clone(), AssetSlot::Failed);
            tracing::error!(url = %url, error = ?error, "image load failed");
        }
    }

    // Request a layout recalc + redraw for every network result.
    // Clone the Arc so resize_viewport can take &mut manager without a borrow conflict.
    if let Some(w) = manager.window.clone() {
        let physical_size = w.inner_size();
        let logical_width = physical_size.width as f32 / w.scale_factor() as f32;
        let logical_height = physical_size.height as f32 / w.scale_factor() as f32;
        let _ = manager.resize_viewport(logical_width, logical_height);
        w.request_redraw();
    }
}

/// Triggers a navigation to `url`, handling protocol normalisation for both
/// local-file (`file:///`) and network (`mizu://`) schemes.
///
/// URLs with any other scheme (e.g. `http://`, `https://`) are ignored with a
/// warning — Mizu only supports the `mizu://` network protocol.
fn navigate_to_url(manager: &mut MizuWindowManager, url: String) {
    // A fresh user/logic-initiated navigation starts a new redirect chain.
    manager.reset_redirect_count();
    let url = match resolve_navigate_url(&manager.chrome_state.url, &url) {
        Some(u) => u,
        None => {
            tracing::warn!(
                current = %manager.chrome_state.url,
                target = %url,
                "blocked: remote document attempted file:// navigation"
            );
            return;
        }
    };
    manager.chrome_state.url = url.clone();
    // Reset capability budget for the new origin on every navigation.
    manager.capability_policy = CapabilityPolicy::new(&url);
    if url.starts_with("file://") {
        if let Some(path) = url.strip_prefix("file:///")
            && let Ok(content) = std::fs::read_to_string(path)
        {
            handle_navigate_success(manager, url, content);
        }
    } else if url.starts_with("mizu://") {
        let _ = manager
            .network_tx
            .send(crate::network::NetworkCmd::Navigate { url });
    } else {
        tracing::warn!(url = %url, "navigation to unrecognised scheme ignored");
    }
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

/// Connects the rendering manager to the Winit event loop.
///
/// `allow_insecure`: when `true`, TLS certificate verification is skipped on
/// QUIC connections (development only).  When `false` (the default), every
/// `mizu://` connection must present a valid TLS certificate; the client drops
/// connections that fail verification.
#[allow(clippy::too_many_arguments)]
pub fn run_window_loop(
    dom: Tree<MizuNode>,
    style_rules: HashMap<String, StyleRules>,
    logic_fns: FxHashMap<Symbol, MizuFunction>,
    interner: StringInterner,
    url_registry: crate::parser::UrlRegistry,
    initial_url: String,
    #[cfg(feature = "insecure-dev")] allow_insecure: bool,
    computed_bindings: Vec<ComputedBinding>,
) -> Result<(), MizuError> {
    let event_loop = winit::event_loop::EventLoopBuilder::<()>::with_user_event()
        .build()
        .map_err(|e| MizuError::ParseError(e.to_string()))?;
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);

    let mut manager = MizuWindowManager::new(
        dom,
        style_rules,
        logic_fns,
        #[cfg(feature = "insecure-dev")]
        allow_insecure,
    )?;
    manager.store = VariableStore::with_interner(interner);
    manager.url_registry = url_registry;
    manager.computed_bindings = computed_bindings;

    // Inject the startup URL into the store
    manager.store.set(
        "window_url",
        crate::core::types::Value::from(initial_url.clone()),
    );
    manager.chrome_state.url = initial_url;

    // Pre-seed the state: evaluate all zero-arity functions and populate the store.
    for (&sym, func) in &manager.logic_fns {
        if func.params.is_empty()
            && let Ok(val) = crate::parser::logic::evaluate(
                &func.body,
                &mut manager.store,
                &manager.logic_fns,
                0,
            )
        {
            manager.store.set_symbol(sym, val);
        }
    }

    // Pre-seed comp vars in the render store.
    {
        let all_syms: rustc_hash::FxHashSet<Symbol> = manager
            .store
            .state_machine
            .global_store
            .keys()
            .copied()
            .collect();
        let computed = manager.computed_bindings.clone();
        let fns = manager.logic_fns.clone();
        crate::parser::logic::recompute_computed_bindings(
            &mut manager.store,
            &computed,
            &fns,
            &all_syms,
        );
        manager.store.state_machine.undo_log.clear();
    }

    // Rebuild node mappings and dependency index using the correct, fully-populated interner.
    // This ensures that variable dependency tracking works correctly from startup.
    manager.rebuild_node_mappings();
    manager.rebuild_dependency_index();
    manager.trigger_logic_reload();
    manager.store.interner.freeze();
    manager.setup_timers();


    let root_node = manager.dom.root().value();
    if root_node.primitive != Primitive::Window {
        return Err(MizuError::ParseError(
            "Root element must be a Window".into(),
        ));
    }

    let title = root_node
        .attributes
        .get("title")
        .cloned()
        .unwrap_or_else(|| "Mizu Application".to_string());

    let window = Arc::new(
        WindowBuilder::new()
            .with_title(title)
            .build(&event_loop)
            .map_err(|e| MizuError::ParseError(format!("Failed to build window: {e}")))?,
    );

    let initial_size = window.inner_size();
    let scale_factor = window.scale_factor();
    let logical_width = initial_size.width as f64 / scale_factor;
    let logical_height = initial_size.height as f64 / scale_factor;
    manager.resize_viewport(logical_width as f32, logical_height as f32)?;

    let mut render_cx = RenderContext::new()
        .map_err(|e| MizuError::ParseError(format!("Vello context error: {e}")))?;
    let mut surface = pollster::block_on(render_cx.create_surface(
        window.clone(),
        initial_size.width,
        initial_size.height,
        wgpu::PresentMode::AutoVsync,
    ))
    .map_err(|e| MizuError::ParseError(format!("Vello surface error: {e}")))?;

    let device = &render_cx.devices[surface.dev_id].device;
    let mut renderer = Renderer::new(
        device,
        RendererOptions {
            surface_format: Some(surface.config.format),
            use_cpu: false,
            antialiasing_support: vello::AaSupport::all(),
            num_init_threads: None,
        },
    )
    .map_err(|e| MizuError::ParseError(format!("Vello renderer error: {e}")))?;

    let mut last_mouse_logical_x = 0.0f32;
    let mut last_mouse_logical_y = 0.0f32;
    let mut mouse_dragging_url_bar = false;

    manager.window = Some(window.clone());

    let res = event_loop.run(move |event, elwt| {
        if let Event::WindowEvent {
            event: ref window_event,
            ..
        } = event
        {
            let window = match manager.window.as_ref() {
                Some(w) => w.clone(),
                None => return,
            };
            match window_event {
                WindowEvent::CloseRequested => {
                    elwt.exit();
                }
                WindowEvent::Resized(physical_size) => {
                    if physical_size.width > 0 && physical_size.height > 0 {
                        render_cx.resize_surface(
                            &mut surface,
                            physical_size.width,
                            physical_size.height,
                        );
                        let scale_factor = window.scale_factor();
                        let logical_width = physical_size.width as f64 / scale_factor;
                        let logical_height = physical_size.height as f64 / scale_factor;

                        let now = std::time::Instant::now();
                        if now.duration_since(manager.last_layout_time)
                            >= std::time::Duration::from_millis(16)
                        {
                            if let Err(e) =
                                manager.resize_viewport(logical_width as f32, logical_height as f32)
                            {
                                tracing::error!("layout recalculation failed: {e}");
                                elwt.exit();
                            } else {
                                manager.last_layout_time = now;
                                manager.pending_resize = None;
                                window.request_redraw();
                            }
                        } else {
                            manager.pending_resize =
                                Some((logical_width as f32, logical_height as f32));
                        }
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let scale_factor = window.scale_factor();
                    last_mouse_logical_x = position.x as f32 / scale_factor as f32;
                    last_mouse_logical_y = position.y as f32 / scale_factor as f32;

                    // URL bar drag-selection
                    if mouse_dragging_url_bar && manager.chrome_state.focused {
                        let bar_left =
                            url_text_left(window.inner_size().width as f32 / scale_factor as f32);
                        let cs = &mut manager.chrome_state;
                        let fc = &mut manager.font_cx;
                        let lc = &mut manager.layout_cx;
                        cs.extend_selection_to_x(last_mouse_logical_x, bar_left, fc, lc);
                        window.request_redraw();
                        return;
                    }

                    let mut hit_node_id = None;
                    if last_mouse_logical_y >= CHROME_HEIGHT {
                        hit_node_id = hit_test(
                            &manager.dom,
                            &manager.taffy,
                            &manager.node_to_taffy_id,
                            &manager.scroll_offsets,
                            last_mouse_logical_x,
                            last_mouse_logical_y - CHROME_HEIGHT + manager.root_scroll_offset_y,
                        );
                    }
                    let mut is_button = false;

                    if let Some(hit_id) = hit_node_id {
                        let mut temp_hit = Some(hit_id);
                        while let Some(id) = temp_hit {
                            if let Some(node_ref) = manager.dom.get(id) {
                                if node_ref.value().primitive == Primitive::Button {
                                    is_button = true;
                                    break;
                                }
                                temp_hit = node_ref.parent().map(|p| p.id());
                            } else {
                                break;
                            }
                        }
                    }

                    if is_button {
                        window.set_cursor_icon(winit::window::CursorIcon::Pointer);
                    } else {
                        window.set_cursor_icon(winit::window::CursorIcon::Default);
                    }
                }
                WindowEvent::MouseInput {
                    state: winit::event::ElementState::Released,
                    button: winit::event::MouseButton::Left,
                    ..
                } => {
                    mouse_dragging_url_bar = false;
                }
                WindowEvent::MouseInput {
                    state: winit::event::ElementState::Pressed,
                    button: winit::event::MouseButton::Left,
                    ..
                } => {
                    let logical_width =
                        window.inner_size().width as f32 / window.scale_factor() as f32;

                    if last_mouse_logical_y < CHROME_HEIGHT {
                        // Click in the chrome area — route to chrome hit zones
                        let zone = chrome_hit_zone(
                            last_mouse_logical_x,
                            last_mouse_logical_y,
                            logical_width,
                        );
                        match zone {
                            ChromeHitZone::BackButton => {
                                tracing::debug!("back button clicked (history nyi)");
                            }
                            ChromeHitZone::ReloadButton => {
                                tracing::debug!("reload button clicked");
                                manager.chrome_state.loading = true;
                                let url = manager.chrome_state.url.clone();
                                navigate_to_url(&mut manager, url);
                            }
                            ChromeHitZone::UrlBar => {
                                let bar_left = url_text_left(logical_width);
                                manager.chrome_state.focused = true;
                                mouse_dragging_url_bar = true;
                                {
                                    let cs = &mut manager.chrome_state;
                                    let fc = &mut manager.font_cx;
                                    let lc = &mut manager.layout_cx;
                                    cs.set_cursor_from_click(
                                        last_mouse_logical_x,
                                        bar_left,
                                        fc,
                                        lc,
                                    );
                                }
                                if manager.focused_node.is_some() {
                                    manager.focused_node = None;
                                }
                            }
                            ChromeHitZone::Background => {
                                // Chrome background — just blur URL bar and DOM focus
                                if manager.chrome_state.focused {
                                    manager.chrome_state.focused = false;
                                }
                                if manager.focused_node.is_some() {
                                    manager.focused_node = None;
                                }
                            }
                        }
                        window.request_redraw();
                        return;
                    }

                    // Click in DOM content area
                    manager.chrome_state.focused = false;

                    let hit_node_id = hit_test(
                        &manager.dom,
                        &manager.taffy,
                        &manager.node_to_taffy_id,
                        &manager.scroll_offsets,
                        last_mouse_logical_x,
                        last_mouse_logical_y - CHROME_HEIGHT + manager.root_scroll_offset_y,
                    );
                    let mut action_node_id = None;
                    let mut new_focus = None;
                    let mut current_hit = hit_node_id;

                    while let Some(id) = current_hit {
                        if let Some(node_ref) = manager.dom.get(id) {
                            if node_ref.value().primitive == crate::parser::Primitive::Input {
                                new_focus = Some(id);
                            }
                            if node_ref.value().events.contains_key("click") {
                                action_node_id = Some(id);
                            }
                            if action_node_id.is_some() {
                                break;
                            }
                            current_hit = node_ref.parent().map(|p| p.id());
                        } else {
                            break;
                        }
                    }

                    if manager.focused_node != new_focus {
                        manager.focused_node = new_focus;
                        window.request_redraw();
                    }

                    if let Some(node_id) = action_node_id
                        && let Some(&u32_id) = manager.node_id_to_u32.get(&node_id)
                    {
                        // Mark user gesture before dispatching — clipboard actions in this
                        // response batch are therefore authorised.
                        manager.has_user_gesture = true;
                        let _ = manager.logic_tx.send(UiEvent::Click { node_id: u32_id });
                        window.request_redraw();
                    }
                }
                WindowEvent::KeyboardInput {
                    event: key_event, ..
                } => {
                    if !key_event.state.is_pressed() {
                        return;
                    }

                    // ── Chrome URL bar has focus — route all keys there ───────
                    if manager.chrome_state.focused {
                        let action = {
                            let cs = &mut manager.chrome_state;
                            cs.handle_key(
                                &key_event.logical_key,
                                key_event.text.as_deref(),
                                manager.modifiers,
                            )
                        };
                        match action {
                            ChromeKeyAction::Navigate(url) => {
                                manager.chrome_state.loading = true;
                                navigate_to_url(&mut manager, url);
                            }
                            ChromeKeyAction::Reload => {
                                let url = manager.chrome_state.url.clone();
                                manager.chrome_state.loading = true;
                                navigate_to_url(&mut manager, url);
                            }
                            ChromeKeyAction::Back => {
                                tracing::debug!("back: history nyi");
                            }
                            ChromeKeyAction::Copy => {
                                if let Some(text) = manager.chrome_state.copy_text()
                                    && let Ok(mut cb) = arboard::Clipboard::new()
                                {
                                    let _ = cb.set_text(text);
                                }
                            }
                            ChromeKeyAction::Cut => {
                                if let Some(text) = manager.chrome_state.cut_text()
                                    && let Ok(mut cb) = arboard::Clipboard::new()
                                {
                                    let _ = cb.set_text(text);
                                }
                            }
                            ChromeKeyAction::Paste => {
                                if let Ok(mut cb) = arboard::Clipboard::new()
                                    && let Ok(text) = cb.get_text()
                                {
                                    manager.chrome_state.paste_text(&text);
                                }
                            }
                            ChromeKeyAction::Handled | ChromeKeyAction::Ignored => {}
                        }
                        window.request_redraw();
                        return;
                    }

                    // ── Global shortcut: Escape exits ────────────────────────
                    if let winit::keyboard::Key::Named(NamedKey::Escape) = key_event.logical_key {
                        elwt.exit();
                    }
                }
                WindowEvent::ModifiersChanged(modifiers) => {
                    manager.modifiers = modifiers.state();
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    let scale = window.scale_factor() as f32;
                    let delta_y = match delta {
                        MouseScrollDelta::LineDelta(_dx, dy) => -dy * 20.0,
                        MouseScrollDelta::PixelDelta(physical) => -(physical.y as f32) / scale,
                    };

                    let mut candidate = hit_test(
                        &manager.dom,
                        &manager.taffy,
                        &manager.node_to_taffy_id,
                        &manager.scroll_offsets,
                        last_mouse_logical_x,
                        last_mouse_logical_y - CHROME_HEIGHT + manager.root_scroll_offset_y,
                    );

                    let mut scrolled = false;
                    while let Some(node_id) = candidate {
                        let is_scroll = manager
                            .dom
                            .get(node_id)
                            .and_then(|n| {
                                n.value().attributes.get("class").and_then(|cls| {
                                    let cls_name = cls.strip_prefix('.').unwrap_or(cls);
                                    manager.style_rules.get(cls_name)
                                })
                            })
                            .map(|rules| rules.overflow == MizuOverflow::Scroll)
                            .unwrap_or(false);

                        if is_scroll {
                            let max_scroll =
                                if let Some(&t_id) = manager.node_to_taffy_id.get(&node_id) {
                                    if let Ok(container_layout) = manager.taffy.layout(t_id) {
                                        let container_h = container_layout.size.height;
                                        let mut content_h: f32 = 0.0;
                                        if let Some(node_ref) = manager.dom.get(node_id) {
                                            for child in node_ref.children() {
                                                if let Some(&c_t_id) =
                                                    manager.node_to_taffy_id.get(&child.id())
                                                    && let Ok(child_layout) =
                                                        manager.taffy.layout(c_t_id)
                                                {
                                                    let bottom = child_layout.location.y
                                                        + child_layout.size.height;
                                                    if bottom > content_h {
                                                        content_h = bottom;
                                                    }
                                                }
                                            }
                                        }
                                        (content_h - container_h).max(0.0)
                                    } else {
                                        0.0
                                    }
                                } else {
                                    0.0
                                };

                            let current =
                                manager.scroll_offsets.get(&node_id).copied().unwrap_or(0.0);
                            let new_offset = (current + delta_y).clamp(0.0, max_scroll);
                            manager.scroll_offsets.insert(node_id, new_offset);
                            scrolled = true;
                            break;
                        }

                        candidate = manager
                            .dom
                            .get(node_id)
                            .and_then(|n| n.parent().map(|p| p.id()));
                    }

                    if scrolled {
                        window.request_redraw();
                    } else if last_mouse_logical_y >= CHROME_HEIGHT {
                        // No scrollable DOM container — scroll root document
                        let phys = window.inner_size();
                        let sf = window.scale_factor() as f32;
                        let viewport_h = phys.height as f32 / sf - CHROME_HEIGHT;
                        let content_h = manager
                            .taffy
                            .layout(manager.root_taffy_id)
                            .map(|l| l.size.height)
                            .unwrap_or(0.0);
                        let max_scroll = (content_h - viewport_h).max(0.0);
                        manager.root_scroll_offset_y =
                            (manager.root_scroll_offset_y + delta_y).clamp(0.0, max_scroll);
                        window.request_redraw();
                    }
                }
                WindowEvent::RedrawRequested => {
                    let physical_size = window.inner_size();
                    let scale = window.scale_factor();
                    let width = physical_size.width;
                    let height = physical_size.height;
                    if width == 0 || height == 0 {
                        return;
                    }

                    let device = &render_cx.devices[surface.dev_id].device;
                    let queue = &render_cx.devices[surface.dev_id].queue;

                    // Resolve background color from window style rule
                    let mut bg_color = vello::peniko::Color::rgba8(255, 255, 255, 255);
                    if let Some(rules) = manager.style_rules.get("window")
                        && let Some(crate::parser::style::MizuBackground::Solid(c)) =
                            &rules.background
                    {
                        bg_color = vello::peniko::Color::rgba8(c.r, c.g, c.b, c.a);
                    }

                    let elapsed_ms = manager.start_time.elapsed().as_millis() as u64;
                    let logical_width = width as f32 / scale as f32;

                    let mut scene = Scene::new();

                    // ── Layer 1: DOM content, clipped below the chrome bar ────
                    let chrome_phys = CHROME_HEIGHT as f64 * scale;
                    let content_clip =
                        vello::kurbo::Rect::new(0.0, chrome_phys, width as f64, height as f64);
                    scene.push_layer(
                        vello::peniko::BlendMode::new(
                            vello::peniko::Mix::Normal,
                            vello::peniko::Compose::SrcOver,
                        ),
                        1.0,
                        Affine::IDENTITY,
                        &content_clip,
                    );

                    let dom_transform = Affine::scale(scale)
                        * Affine::translate((
                            0.0,
                            (CHROME_HEIGHT - manager.root_scroll_offset_y) as f64,
                        ));

                    let has_animations;
                    {
                        let chrome_url_snapshot = manager.chrome_state.url.clone();
                        let mut ctx = PaintContext {
                            tree: &manager.dom,
                            taffy: &manager.taffy,
                            node_to_taffy_id: &manager.node_to_taffy_id,
                            style_rules: &manager.style_rules,
                            font_cx: &mut manager.font_cx,
                            layout_cx: &mut manager.layout_cx,
                            transform: dom_transform,
                            store: &mut manager.store,
                            scroll_offsets: &manager.scroll_offsets,
                            focused_node: manager.focused_node,
                            image_cache: &mut manager.image_cache,
                            fetching_images: &mut manager.fetching_images,
                            elapsed_ms,
                            network_tx: &manager.network_tx,
                            chrome_url: &chrome_url_snapshot,
                            has_animations: false,
                            text_layouts: &manager.text_layouts,
                            item_bindings: std::collections::HashMap::new(),
                            each_groups: &manager.each_expansion.groups,
                            taffy_id_overrides: std::collections::HashMap::new(),
                        };
                        paint_node(manager.dom.root().id(), &mut ctx, &mut scene, (0.0, 0.0));
                        has_animations = ctx.has_animations;
                    } // font_cx / layout_cx borrows released here

                    scene.pop_layer();

                    // ── Layer 2: Chrome bar (always on top) ──────────────────
                    {
                        let cs = &manager.chrome_state;
                        let fc = &mut manager.font_cx;
                        let lc = &mut manager.layout_cx;
                        paint_chrome(
                            &mut scene,
                            cs,
                            logical_width,
                            Affine::scale(scale),
                            elapsed_ms,
                            fc,
                            lc,
                        );
                    }

                    if has_animations || manager.chrome_state.loading {
                        window.request_redraw();
                    }

                    // ── Render scene directly to swapchain surface ───────────
                    let surface_texture = match surface.surface.get_current_texture() {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::error!("surface texture acquire failed: {e}");
                            return;
                        }
                    };

                    let render_params = vello::RenderParams {
                        base_color: bg_color,
                        width,
                        height,
                        antialiasing_method: AaConfig::Area,
                    };

                    if let Err(e) = renderer.render_to_surface(
                        device,
                        queue,
                        &scene,
                        &surface_texture,
                        &render_params,
                    ) {
                        tracing::error!("render_to_surface failed: {e}");
                        return;
                    }

                    surface_texture.present();

                    // Expose scroll state to the logic store
                    manager.store.set(
                        "root_scroll_y",
                        crate::core::types::Value::Float(manager.root_scroll_offset_y as f64),
                    );
                }
                _ => {}
            }
        } else if let Event::AboutToWait = event {
            // Drain all pending network results without blocking.  Collecting
            // into a Vec first avoids a split-borrow conflict between
            // `manager.network_rx` (needs &mut) and the rest of `manager`
            // (needed by process_network_result).
            let network_msgs: Vec<_> =
                std::iter::from_fn(|| manager.network_rx.try_recv().ok()).collect();
            for res in network_msgs {
                process_network_result(&mut manager, res);
            }

            let mut state_changed = false;
            let mut mutated_symbols = Vec::new();
            while let Ok(res) = manager.logic_rx.try_recv() {
                match res {
                    Ok(response) => {
                        for (name, val) in response.state_update.mutated_variables {
                            let sym = manager.store.interner.get_or_intern(&name);
                            manager.store.set(name, val);
                            state_changed = true;
                            mutated_symbols.push(sym);
                        }
                        for action in response.runtime_actions {
                            if let crate::network::RuntimeAction::Navigate { url } = &action {
                                // Navigate actions must go through navigate_to_url so
                                // chrome_state and the capability policy are updated.
                                manager.chrome_state.loading = true;
                                let url = url.clone();
                                navigate_to_url(&mut manager, url);
                            } else if let crate::network::RuntimeAction::CopyToClipboard {
                                node_id,
                            } = &action
                            {
                                // Clipboard is intercepted here (not in execute_capability_action)
                                // so we can enforce the user-gesture gate and do DOM lookup.
                                let node_id = node_id.clone();
                                match apply_clipboard_action(
                                    &node_id,
                                    &manager.dom,
                                    &manager.local_inputs,
                                    &manager.node_id_to_u32,
                                    &manager.store,
                                    manager.has_user_gesture,
                                ) {
                                    Ok(text) => {
                                        if let Ok(mut cb) = arboard::Clipboard::new() {
                                            let _ = cb.set_text(text);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "clipboard action rejected");
                                    }
                                }
                            } else {
                                manager.execute_capability_action(action);
                            }
                        }
                        // User-gesture activation is transitory: consume it after each
                        // action batch so subsequent batches without a click are blocked.
                        manager.has_user_gesture = false;
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "logic worker error");
                    }
                }
            }

            if state_changed || manager.typing_layout_dirty {
                manager.setup_timers();

                let mut layout_dirty = manager.typing_layout_dirty;
                manager.typing_layout_dirty = false;

                for sym in mutated_symbols {
                    if let Some(nodes) = manager.dependency_index.get(&sym) {
                        for &node_id in nodes {
                            manager.dirty_nodes.insert(node_id);

                            let current_width =
                                if let Some(&taffy_node) = manager.node_to_taffy_id.get(&node_id) {
                                    if let Ok(layout) = manager.taffy.layout(taffy_node) {
                                        Some(layout.size.width)
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };

                            let old_dims = manager.text_dimensions.get(&node_id).copied();

                            if let Some((new_dims, layout)) =
                                crate::render::text_engine::calculate_node_text(
                                    node_id,
                                    &manager.dom,
                                    &manager.style_rules,
                                    &mut manager.font_cx,
                                    &mut manager.layout_cx,
                                    &manager.store,
                                    current_width,
                                )
                            {
                                manager.text_layouts.insert(node_id, layout);
                                manager.text_dimensions.insert(node_id, new_dims);
                                manager.dirty_nodes.remove(&node_id);

                                let dimensions_changed = match old_dims {
                                    Some(old) => {
                                        (old.0 - new_dims.0).abs() > f32::EPSILON
                                            || (old.1 - new_dims.1).abs() > f32::EPSILON
                                    }
                                    None => true,
                                };

                                if dimensions_changed
                                    && let Some(&taffy_node) =
                                        manager.node_to_taffy_id.get(&node_id)
                                {
                                    let _ = manager.taffy.mark_dirty(taffy_node);
                                    layout_dirty = true;
                                }
                            }
                        }
                    }
                }

                if layout_dirty {
                    let physical_size = window.inner_size();
                    let logical_width = physical_size.width as f32 / window.scale_factor() as f32;
                    let logical_height = physical_size.height as f32 / window.scale_factor() as f32;
                    if let Err(e) = manager.resize_viewport(logical_width, logical_height) {
                        tracing::error!("layout recalculation failed after state update: {e}");
                    }
                }
                window.request_redraw();
            }

            let now = std::time::Instant::now();
            let mut redraw = false;
            let mut next_wakeup = manager.timer_queue.keys().next().copied();

            if let Some((w, h)) = manager.pending_resize {
                let elapsed = now.duration_since(manager.last_layout_time);
                if elapsed >= std::time::Duration::from_millis(16) {
                    if let Err(e) = manager.resize_viewport(w, h) {
                        tracing::error!("throttled layout recalculation failed: {e}");
                    }
                    manager.last_layout_time = now;
                    manager.pending_resize = None;
                    redraw = true;
                } else {
                    let wake_time = manager.last_layout_time + std::time::Duration::from_millis(16);
                    next_wakeup = Some(next_wakeup.map(|t| t.min(wake_time)).unwrap_or(wake_time));
                }
            }

            while let Some(&deadline) = manager.timer_queue.keys().next() {
                if now >= deadline {
                    if let Some(node_ids) = manager.timer_queue.remove(&deadline) {
                        for node_id in node_ids {
                            if let Some(node_ref) = manager.dom.get(node_id)
                                && let Some(EventBlock::Every { interval, .. }) =
                                    node_ref.value().events.get("every")
                            {
                                if let Some(&u32_id) = manager.node_id_to_u32.get(&node_id) {
                                    let _ =
                                        manager.logic_tx.send(UiEvent::Timer { node_id: u32_id });
                                }

                                let mut interval_ms = match interval {
                                    crate::parser::layout::Interval::Literal(ms) => *ms,
                                    crate::parser::layout::Interval::Variable(var_name) => {
                                        let val = manager.store.get(var_name).ok();
                                        match val {
                                            Some(Value::Float(f)) => *f as u64,
                                            Some(Value::Int(i)) => *i as u64,
                                            _ => 16,
                                        }
                                    }
                                };
                                if interval_ms < 16 {
                                    interval_ms = 16;
                                }
                                let next_deadline =
                                    now + std::time::Duration::from_millis(interval_ms);
                                manager
                                    .timer_queue
                                    .entry(next_deadline)
                                    .or_default()
                                    .push(node_id);
                            }
                        }
                    }
                } else {
                    break;
                }
            }

            if redraw {
                let physical_size = window.inner_size();
                let logical_width = physical_size.width as f32 / window.scale_factor() as f32;
                let logical_height = physical_size.height as f32 / window.scale_factor() as f32;
                if let Err(e) = manager.resize_viewport(logical_width, logical_height) {
                    tracing::error!("layout recalculation failed after timer: {e}");
                }
                window.request_redraw();
            }

            next_wakeup = manager
                .timer_queue
                .keys()
                .next()
                .copied()
                .map(|t| {
                    if let Some(w) = next_wakeup {
                        t.min(w)
                    } else {
                        t
                    }
                })
                .or(next_wakeup);

            // While a network fetch is in flight, poll every 16 ms so the
            // try_recv drain fires regularly and the UI stays responsive.
            if manager.chrome_state.loading {
                let poll_deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(16);
                next_wakeup = Some(
                    next_wakeup
                        .map(|d: std::time::Instant| d.min(poll_deadline))
                        .unwrap_or(poll_deadline),
                );
            }

            if let Some(deadline) = next_wakeup {
                elwt.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(deadline));
            } else {
                elwt.set_control_flow(winit::event_loop::ControlFlow::Wait);
            }
        }
    });

    res.map_err(|e| MizuError::ParseError(format!("Event loop error: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::MizuDimension;
    use crate::parser::Primitive;

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
        for hop in 1..=MAX_REDIRECTS {
            assert!(
                manager.register_redirect(),
                "redirect hop {hop} should be permitted (<= MAX_REDIRECTS)"
            );
        }
        assert!(
            !manager.register_redirect(),
            "redirect hop {} must be refused (exceeds MAX_REDIRECTS)",
            MAX_REDIRECTS + 1
        );
    }

    #[test]
    fn redirect_counter_reset_clears_budget() {
        let mut manager = make_minimal_manager();
        for _ in 0..MAX_REDIRECTS {
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
}
