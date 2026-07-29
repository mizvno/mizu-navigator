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
use crate::network::{ReloadPayload, RuntimeAction, TabId, UiEvent, WorkerResponse};
use crate::parser::logic::{ComputedBinding, MizuFunction, RootTimer, TimerInterval};
use crate::parser::style::StyleVariant;
use crate::parser::{Action, EventBlock, MizuNode, StyleRules};
use crate::render::chrome_vello::CHROME_HEIGHT;
use crate::render::layout_bridge::{EachExpansion, expand_each_nodes};
use crate::render::responsive::{RenderEnvironment, ViewportSize};
use crate::render::security::get_raw_domain;
use super::AssetSlot;
use super::history::{HistoryLog, HistorySidebarState, HistoryStack};
use crate::render::preferences::UserPreferences;
use crate::render::security::CapabilityPolicy;

/// Source of [`TabState::a11y_epoch`] values.
///
/// Process-wide rather than per-tab: every tab feeds the same long-lived
/// accesskit adapter, so two tabs numbering their nodes from zero would
/// collide with each other exactly as two successive documents in one tab do.
static A11Y_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Returns a generation number no mapping has used before.
///
/// Wrapping would need a document load every millisecond for seven weeks
/// straight; if it somehow happened, the counter skips zero and the worst
/// case is one confused accessibility update, not unsoundness.
fn next_a11y_epoch() -> u32 {
    let epoch = A11Y_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if epoch == 0 { A11Y_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed) } else { epoch }
}

/// Serialises worker spawning against anything observing the process-wide
/// `SPAWN_COUNT` totals.
///
/// Those counters back the "opening a tab spawns no thread" guarantee, but
/// they count the whole *process*, and `cargo test` runs tests as threads of
/// one process: a test asserting "the totals did not move" races against any
/// other test constructing a manager. Holding this gate for the length of an
/// observation makes the totals stable for that window.
///
/// The lock lives inside [`MizuWindowManager::new`] rather than in each
/// spawning test so that a future test cannot silently opt out of it by
/// forgetting to take it. In production it is taken once per window, with no
/// contention.
pub(crate) static SPAWN_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Locks [`SPAWN_GATE`], ignoring poisoning.
///
/// The gate guards no data — a panicking holder leaves nothing inconsistent
/// behind, so a poisoned lock is still perfectly usable and failing here
/// would only turn one test failure into a cascade.
pub(crate) fn lock_spawn_gate() -> std::sync::MutexGuard<'static, ()> {
    SPAWN_GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Maximum number of consecutive server redirects honoured for a single
/// user-initiated navigation before the chain is aborted.  Prevents a hostile
/// or misconfigured server from trapping the client in an infinite redirect
/// loop.
pub(super) static MAX_REDIRECTS: LazyLock<u32> =
    LazyLock::new(|| crate::core::config::CONFIG.max_redirects);

/// Hard cap on concurrently open tabs.
///
/// Each tab holds a whole DOM, taffy tree, `VariableStore`, frozen interner
/// and inspector log, so unbounded tab creation is a memory-exhaustion vector
/// the moment `open_tab` becomes reachable from document logic (it is not
/// today, and must not become so without a user-gesture gate).
pub const MAX_OPEN_TABS: usize = 32;

/// Maximum number of decoded images kept in `image_cache` before the least
/// recently used entry is evicted.
///
/// Non-zero by construction so the two `LruCache::new` sites need no runtime
/// unwrap (the crate denies `clippy::unwrap_used`).
const IMAGE_CACHE_CAPACITY: std::num::NonZeroUsize = match std::num::NonZeroUsize::new(200) {
    Some(n) => n,
    None => panic!("image cache capacity must be non-zero"),
};

/// All state belonging to **one open tab** — one document and everything
/// derived from it.
///
/// Split out of [`MizuWindowManager`] so several documents can be resident at
/// once in a single OS window. The partition rule is: state that a *document*
/// owns lives here; state the *window* or the *user* owns stays on the
/// manager.
///
/// Three fields are security-load-bearing and must never be read or written
/// across a tab boundary (invariant **T1**, `SECURITY-INVARIANTS.md`):
/// `capability_policy` (per-origin storage quota + write rate limit),
/// `redirect_count` (a per-navigation-chain budget), and `has_user_gesture`
/// (which gates N3's cross-origin navigation check and the clipboard).
/// Hoisting any of them back to window level is a security regression, not a
/// simplification.
///
/// Holds no `FontContext`, no `LayoutContext`, and no channel endpoints — all
/// of which are window-level — which is what makes [`TabState::new`] free of
/// thread spawning and system-font loading, and therefore cheap in tests.
pub struct TabState {
    /// Stable identity for this tab; never reused after close.
    pub id: TabId,
    /// The unlinked DOM tree.
    pub dom: Tree<MizuNode>,
    /// The active CSS rules map.
    pub style_rules: HashMap<String, StyleRules>,
    /// Breakpoint/color-scheme style variants (ux-6) — see
    /// `docs/design/responsive.md`. Resolved against `viewport_size` and the
    /// window's `preferences.color_scheme` on every taffy-tree (re)build.
    pub style_variants: Vec<StyleVariant>,
    /// The *content* viewport this tab was last laid out against: the window
    /// size minus the chrome bar, and minus the inspector panel when this
    /// tab's `inspector` is open. Per-tab rather than window-level precisely
    /// because `inspector.open` is per-tab, so two tabs in the same window
    /// legitimately lay out at different widths.
    pub viewport_size: ViewportSize,
    /// Set when the window resized (or the color scheme changed) while this
    /// tab was in the background. Consumed on the next activation, so a
    /// background tab is relaid out once on switch rather than on every
    /// resize tick of a window it isn't visible in.
    pub layout_stale: bool,
    /// The taffy layout engine instance.
    pub taffy: TaffyTree<EgoNodeId>,
    /// Mapping of DOM Node IDs to Taffy Node IDs.
    pub node_to_taffy_id: HashMap<EgoNodeId, taffy::prelude::NodeId>,
    /// The Taffy ID of the root node.
    pub root_taffy_id: taffy::prelude::NodeId,
    /// The runtime variable store for state.
    pub store: VariableStore,
    /// The set of functions defined in the logic block.
    pub logic_fns: FxHashMap<Symbol, MizuFunction>,
    /// Logical scroll offsets for each container (in pixels).
    pub scroll_offsets: HashMap<EgoNodeId, f32>,
    /// Currently focused node for text input.
    pub focused_node: Option<EgoNodeId>,
    /// Chrome bar state for this tab: the URL text buffer plus its edit
    /// cursor/selection/focus, and the loading flag.
    ///
    /// Kept whole rather than split window-level/per-tab because `url` is
    /// dual-purpose — it is both the URL bar's editable buffer *and* the
    /// origin-of-record that `check_navigation` and `CapabilityPolicy::new`
    /// read. Separating the buffer from its own origin would invite the two
    /// to disagree. Per-tab edit state also matches browser behaviour.
    pub chrome_state: ChromeState,
    /// Vertical scroll offset of the root document (logical pixels).
    pub root_scroll_offset_y: f32,
    /// Average measured row height (logical px) per `Each` block, refreshed
    /// after every layout pass. Persists across taffy rebuilds so
    /// virtualization windowing converges on a real estimate.
    pub each_row_height_estimate: HashMap<EgoNodeId, f32>,
    /// Absolute Y offset (logical px) of each `Each` container's top edge,
    /// captured every frame by `paint_each`.
    pub each_container_offset_y: HashMap<EgoNodeId, f32>,
    /// Bidirectional node mapping: EgoNodeId to u32.
    pub node_id_to_u32: HashMap<EgoNodeId, u32>,
    /// Bidirectional node mapping: u32 to EgoNodeId.
    pub u32_to_node_id: HashMap<u32, EgoNodeId>,
    /// Next u32 allocator for the bidirectional node mapping.
    pub next_u32_id: u32,
    /// Generation of this tab's node mapping, bumped by
    /// [`Self::rebuild_node_mappings`] and used to keep accessibility node ids
    /// from colliding across documents and tabs — see
    /// [`crate::render::accessibility::access_id`].
    pub a11y_epoch: u32,
    /// Inverted dependency index mapping global variables to the DOM nodes
    /// that depend on them.
    pub dependency_index: HashMap<Symbol, Vec<EgoNodeId>>,
    /// Cache of Parley text layouts.
    pub text_layouts: HashMap<EgoNodeId, parley::Layout<vello::peniko::Color>>,
    /// Cache of text dimensions.
    pub text_dimensions: HashMap<EgoNodeId, (f32, f32)>,
    /// Set of DOM nodes that have dirty visual text state.
    pub dirty_nodes: std::collections::HashSet<EgoNodeId>,
    /// Flag indicating that layout recalculation was deferred due to typing.
    pub typing_layout_dirty: bool,
    /// Current values of locally typed text fields.
    /// NOT sent to the worker during typing — only collected on Submit.
    pub local_inputs: FxHashMap<u32, String>,
    /// File selections for `type "file"` inputs, keyed by the input node's
    /// u32 id. Holds only path/filename metadata, never file bytes.
    pub local_file_selections: FxHashMap<u32, std::sync::Arc<crate::core::types::FileHandleData>>,
    /// URL registry — compile-time endpoint aliases from the `urls` block.
    pub url_registry: crate::parser::UrlRegistry,
    /// Expanded Taffy subtrees for every `Each` node in this document.
    pub each_expansion: EachExpansion,
    /// Consecutive redirects followed since this tab's last user-initiated
    /// navigation. **T1:** per-tab so one tab's redirect chain can neither
    /// consume nor disable another's loop protection.
    pub redirect_count: u32,
    /// Computed (derived) variable bindings in topological order.
    pub computed_bindings: Vec<ComputedBinding>,
    /// Whether this tab's most recent interaction was a qualifying gesture.
    /// Cleared after that tab's action batch is processed. **T1:** per-tab so
    /// a click in one tab can never authorise another tab's clipboard read or
    /// cross-origin navigation.
    pub has_user_gesture: bool,
    /// Per-origin capability budget (storage quota + rate limit) for this
    /// tab's current origin. **T1:** per-tab so a low-trust origin cannot
    /// consume — or benefit from — quota attributable to a different origin
    /// open in another tab. Reset on every navigation within this tab.
    pub capability_policy: CapabilityPolicy,
    /// Root-level `timer` declarations from the `logic` block.
    pub root_timers: Vec<RootTimer>,
    /// Priority queue of pending root-timer deadlines.
    pub root_timer_queue: BTreeMap<std::time::Instant, Vec<usize>>,
    /// Live inspector UI state for this tab (panel visibility, tab, selection,
    /// scroll). Per-tab, matching how browser devtools are scoped.
    pub inspector: crate::render::inspector::InspectorState,
    /// Bounded log of this tab's runtime events and network activity.
    pub inspector_log: crate::render::inspector::log::InspectorLog,
    /// Instant of the most recent mutation per variable, for the inspector's
    /// Logic tab flash.
    pub recent_mutations: FxHashMap<Symbol, std::time::Instant>,
    /// In-memory session history (Back/Forward stacks) for this tab.
    pub history: HistoryStack,
    /// Scroll offset to restore once this tab's in-flight history navigation
    /// finishes loading.
    pub pending_scroll_restore: Option<f32>,
}

/// Encapsulates the application state, DOM, and Layout definitions.
pub struct MizuWindowManager {
    /// The active winit window instance.
    pub window: Option<Arc<Window>>,
    /// Every open tab, in tab-strip order. Invariant: never empty, and
    /// `active_tab` is always a valid index into it. Both are maintained by
    /// `close_tab`, which refuses to remove the last tab.
    pub tabs: Vec<TabState>,
    /// Index into [`Self::tabs`] of the tab currently displayed and receiving
    /// input. An index rather than a [`TabId`] so the common path (paint,
    /// input dispatch) is a direct slice index instead of a map lookup;
    /// `close_tab` is responsible for keeping it in range.
    pub active_tab: usize,
    /// Monotonic allocator for [`TabId`]s. Never decreases and ids are never
    /// recycled — see [`TabId`]'s doc comment for why reuse would be unsound.
    /// Consumed by `open_tab` (Stage 3); allocated here so Stage 1 already
    /// establishes the never-reuse invariant.
    #[allow(dead_code)]
    pub(super) next_tab_id: u64,
    /// The raw window content size in logical pixels (window size minus the
    /// chrome bar), before any per-tab inspector-panel adjustment. Each tab
    /// derives its own `viewport_size` from this.
    /// Read by `switch_to_tab` (Stage 3) to relayout a stale background tab.
    #[allow(dead_code)]
    pub window_logical_size: (f32, f32),
    /// Parley font context. Window-level: holds font/shaping caches only, no
    /// document state, so sharing it across tabs is both safe and desirable
    /// (system-font enumeration is expensive and would otherwise be repaid
    /// per tab).
    pub font_cx: parley::FontContext,
    /// Parley layout context. Window-level for the same reason as `font_cx`.
    pub layout_cx: parley::LayoutContext<vello::peniko::Color>,
    /// Async-compatible sender for commands to the background network thread.
    /// One shared worker serves every tab; messages are tagged with a
    /// [`TabId`] so responses route back to the tab that issued them.
    pub network_tx: tokio::sync::mpsc::UnboundedSender<crate::network::NetworkCmd>,
    /// Bounded tokio receiver for [`crate::network::NetworkResult`] messages
    /// from the network worker. Drained each frame via `try_recv()` so the UI
    /// thread never blocks on network I/O.
    pub network_rx: tokio::sync::mpsc::Receiver<crate::network::NetworkResult>,
    /// Sender to the dedicated logic worker thread — one shared worker for
    /// every tab, which keeps thread count constant as tabs are opened.
    pub logic_tx: std::sync::mpsc::Sender<(TabId, UiEvent)>,
    /// Receiver for updates from the logic worker thread.
    pub logic_rx: std::sync::mpsc::Receiver<(TabId, Result<WorkerResponse, MizuError>)>,
    /// Keyboard modifiers state. A property of physical input, not of any
    /// document, so it is window-level.
    pub modifiers: winit::keyboard::ModifiersState,
    /// Cache for decoded images used in `background-image` and `image` tags.
    /// Shared across tabs and keyed by URL (already a global namespace); the
    /// cached `AssetSlot` holds decoded pixels only — never document state or
    /// tainted values — so sharing leaks nothing across origins. Bounded by
    /// [`IMAGE_CACHE_CAPACITY`], which would otherwise be paid per tab.
    pub image_cache: lru::LruCache<String, AssetSlot>,
    /// Track currently fetching images to avoid duplicate requests.
    pub fetching_images: FxHashMap<String, Vec<TabId>>,
    /// Global start time of the engine for animations (e.g. the URL bar's
    /// cursor blink). Window-level: root timers schedule against
    /// `Instant::now()`, not against this, so it carries no per-document
    /// meaning.
    pub start_time: std::time::Instant,
    /// Last layout calculation time — a resize throttle, window-level.
    pub last_layout_time: std::time::Instant,
    /// Pending resize dimensions.
    pub pending_resize: Option<(f32, f32)>,
    /// Detected OS appearance/accessibility preferences (ux-5). A user-level
    /// setting shared by every tab.
    pub preferences: UserPreferences,
    /// Window-level persistent history log: receives a push on every fresh
    /// top-level navigation (not on Back/Forward steps). Loaded from disk on
    /// startup and saved on exit. Data source for the history sidebar panel.
    pub history_log: HistoryLog,
    /// UI state for the history sidebar panel (visibility, scroll, hover).
    pub history_sidebar: HistorySidebarState,
}

/// A freshly parsed document to replace the currently loaded one, passed to
/// [`MizuWindowManager::reload_document`]. Replaces that method's prior
/// 7-parameter positional argument list; shaped like `event_loop`'s
/// `InitialDocument` (the initial-load equivalent) minus the two fields
/// (`url_registry`, `initial_url`) the navigation caller already applies to
/// the manager separately before calling `reload_document`.
pub struct ReloadedDocument {
    /// The newly parsed DOM tree.
    pub dom: Tree<MizuNode>,
    /// Tag/class style rules from the new document's `style` block.
    pub style_rules: HashMap<String, StyleRules>,
    /// Breakpoint/color-scheme style variants (ux-6) for the new document.
    pub style_variants: Vec<StyleVariant>,
    /// The new document's declared `logic` functions.
    pub logic_fns: FxHashMap<Symbol, MizuFunction>,
    /// The string interner for the new document (replaces the old one).
    pub interner: StringInterner,
    /// The new document's `comp`-declared computed/derived bindings.
    pub computed_bindings: Vec<ComputedBinding>,
    /// The new document's declared root-scope `timer` blocks.
    pub root_timers: Vec<RootTimer>,
}

/// Keeps the far ends of a headless manager's dummy channels alive.
///
/// Without this the `Sender`s on the manager would see a disconnected peer the
/// instant [`MizuWindowManager::new_headless`] returns, and every `send` would
/// fail — masking the very dispatches tests want to observe. Bind it to a
/// `let _keepalive = ...` for the duration of the test.
#[cfg(test)]
pub(crate) struct TestChannelKeepAlive {
    _network_cmd_rx: tokio::sync::mpsc::UnboundedReceiver<crate::network::NetworkCmd>,
    _network_result_tx: tokio::sync::mpsc::Sender<crate::network::NetworkResult>,
    _logic_event_rx: std::sync::mpsc::Receiver<(TabId, UiEvent)>,
    _logic_response_tx: std::sync::mpsc::Sender<(TabId, Result<WorkerResponse, MizuError>)>,
}

/// Borrowed window-level state that a per-tab operation may need.
///
/// Exists to solve a borrow-checker problem, not as an abstraction: functions
/// like [`resize_tab_viewport`] legitimately touch ~25 fields spanning both
/// halves of the tab/window split. Rust's NLL sees field-level disjointness
/// only through a *direct destructure of a place*, so
/// [`MizuWindowManager::split_active`] destructures `self` once and hands back
/// a `&mut TabState` plus this bundle of independent borrows of the remaining
/// fields. Constructed on demand; never stored.
pub(super) struct WindowCtx<'a> {
    /// Id of the tab currently on screen.
    ///
    /// Lets a per-tab operation tell whether it is running for the visible
    /// document — the gate on window-level side effects such as the OS window
    /// title, which a background load must never change.
    pub active_tab_id: TabId,
    /// Shared Parley font context.
    pub font_cx: &'a mut parley::FontContext,
    /// Shared Parley layout context.
    pub layout_cx: &'a mut parley::LayoutContext<vello::peniko::Color>,
    /// Shared, URL-keyed decoded-image cache.
    pub image_cache: &'a mut lru::LruCache<String, AssetSlot>,
    /// In-flight image URLs (dedupe set).
    pub fetching_images: &'a mut FxHashMap<String, Vec<TabId>>,
    /// Sender to the shared network worker.
    pub network_tx: &'a tokio::sync::mpsc::UnboundedSender<crate::network::NetworkCmd>,
    /// Sender to the shared logic worker.
    pub logic_tx: &'a std::sync::mpsc::Sender<(TabId, UiEvent)>,
    /// User appearance/accessibility preferences.
    pub preferences: &'a UserPreferences,
    /// The OS window, when one exists yet.
    pub window: Option<&'a Arc<Window>>,
    /// Raw window content size before per-tab inspector adjustment.
    /// Read by `switch_to_tab` (Stage 3).
    #[allow(dead_code)]
    pub window_logical_size: (f32, f32),
    /// Current keyboard modifier state (a property of physical input, not of
    /// any document). Copied rather than borrowed since it is `Copy`.
    pub modifiers: winit::keyboard::ModifiersState,
    /// Engine start time, for animation phase. `Copy`.
    pub start_time: std::time::Instant,
    /// Mutable reference to the window-level persistent history log, so
    /// navigation helpers can push entries without needing a second split.
    pub history_log: &'a mut HistoryLog,
}

impl MizuWindowManager {
    /// Splits `self` into the active tab plus the window-level state it may
    /// need, as two independent borrows.
    ///
    /// # Panics
    ///
    /// Panics if the `active_tab < tabs.len()` invariant has been broken. That
    /// is deliberately loud rather than silently clamping: a
    /// wrong-but-in-range index would silently deliver one tab's input to
    /// another, which is exactly the class of bug invariant T1 exists to
    /// prevent.
    pub(super) fn split_active(&mut self) -> (&mut TabState, WindowCtx<'_>) {
        let Self {
            tabs,
            active_tab,
            font_cx,
            layout_cx,
            image_cache,
            fetching_images,
            network_tx,
            logic_tx,
            preferences,
            window,
            window_logical_size,
            modifiers,
            start_time,
            history_log,
            ..
        } = self;
        // Direct index, matching `active()`/`active_mut()`: the
        // `active_tab < tabs.len()` invariant is maintained by `close_tab`,
        // and violating it must fail loudly rather than silently redirecting
        // one tab's input to another (see this method's doc comment).
        let tab = &mut tabs[*active_tab];
        let active_tab_id = tab.id;
        (
            tab,
            WindowCtx {
                active_tab_id,
                font_cx,
                layout_cx,
                image_cache,
                fetching_images,
                network_tx,
                logic_tx,
                preferences,
                window: window.as_ref(),
                window_logical_size: *window_logical_size,
                modifiers: *modifiers,
                start_time: *start_time,
                history_log,
            },
        )
    }

    /// Like [`Self::split_active`] but selects a tab by id.
    ///
    /// Returns `None` when `id` names no live tab — a message arriving for a
    /// tab the user has since closed. Callers must treat that as a silent
    /// drop and never fall back to the active tab: a `WorkerResponse` carries
    /// bare [`Symbol`]s that are only meaningful against *its own* tab's
    /// frozen interner, so resolving one against a different tab would write
    /// a value under whatever unrelated name that id happens to mean there.
    pub(super) fn split_tab(&mut self, id: TabId) -> Option<(&mut TabState, WindowCtx<'_>)> {
        let Self {
            tabs,
            active_tab,
            font_cx,
            layout_cx,
            image_cache,
            fetching_images,
            network_tx,
            logic_tx,
            preferences,
            window,
            window_logical_size,
            modifiers,
            start_time,
            history_log,
            ..
        } = self;
        let active_tab_id = tabs[*active_tab].id;
        let tab = tabs.iter_mut().find(|t| t.id == id)?;
        Some((
            tab,
            WindowCtx {
                active_tab_id,
                font_cx,
                layout_cx,
                image_cache,
                fetching_images,
                network_tx,
                logic_tx,
                preferences,
                window: window.as_ref(),
                window_logical_size: *window_logical_size,
                modifiers: *modifiers,
                start_time: *start_time,
                history_log,
            },
        ))
    }

    /// Index of the active tab within [`Self::tabs`]. Always in range.
    pub fn active_tab_index(&self) -> usize {
        self.active_tab
    }

    /// The currently displayed tab.
    pub fn active(&self) -> &TabState {
        &self.tabs[self.active_tab]
    }

    /// Opens a blank tab on `url` and returns its id, or `None` when
    /// [`MAX_OPEN_TABS`] is already reached.
    ///
    /// Does **not** switch to it — the caller decides, because Ctrl+T switches
    /// while a background open (should one ever be added) must not.
    pub fn open_tab(&mut self, url: &str) -> Option<TabId> {
        if self.tabs.len() >= MAX_OPEN_TABS {
            tracing::warn!(open = self.tabs.len(), "refusing to open tab: limit reached");
            return None;
        }
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let doc = TabDocument {
            // An empty `doc` root: a blank page until the first navigation
            // replaces the whole document via `reload_tab_document`.
            dom: Tree::new(MizuNode {
                primitive: crate::parser::Primitive::Doc,
                attributes: FxHashMap::default(),
                events: FxHashMap::default(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            }),
            style_rules: HashMap::new(),
            style_variants: Vec::new(),
            logic_fns: FxHashMap::default(),
        };
        let env = RenderEnvironment {
            viewport: ViewportSize {
                width: self.window_logical_size.0,
                height: (self.window_logical_size.1 - CHROME_HEIGHT).max(0.0),
            },
            color_scheme: self.preferences.color_scheme,
        };
        let tab = match TabState::new(id, doc, env, url, &mut self.image_cache) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = ?e, "failed to build new tab");
                return None;
            }
        };
        self.tabs.push(tab);
        Some(id)
    }

    /// Closes `id`, returning `false` when it was the last tab — the caller
    /// decides what that means (the event loop exits).
    ///
    /// Order matters: the worker is told first so it drops the document's
    /// store and interner, then the tab is unregistered from every in-flight
    /// image's waiter list, and only then removed. A late worker or network
    /// response tagged with `id` afterwards finds no tab and is dropped, which
    /// is exactly the intended behaviour — ids are never reused, so it can
    /// never be misrouted to a different document.
    pub fn close_tab(&mut self, id: TabId) -> bool {
        let Some(pos) = self.tabs.iter().position(|t| t.id == id) else {
            return true;
        };
        if self.tabs.len() == 1 {
            return false;
        }
        let _ = self.logic_tx.send((id, UiEvent::CloseTab));
        for waiters in self.fetching_images.values_mut() {
            waiters.retain(|w| *w != id);
        }
        self.fetching_images.retain(|_, waiters| !waiters.is_empty());
        self.tabs.remove(pos);
        // Browser convention: focus moves to the tab on the right, falling
        // back to the left when the closed tab was last.
        if self.active_tab > pos || self.active_tab >= self.tabs.len() {
            self.active_tab = self.active_tab.saturating_sub(1);
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        true
    }

    /// Makes `id` the visible tab. No-op for an unknown id.
    ///
    /// Returns `true` when the caller should rebuild window-level views of the
    /// tab (title, accessibility tree, redraw) — i.e. when the active tab
    /// actually changed.
    pub fn switch_to_tab(&mut self, id: TabId) -> bool {
        let Some(pos) = self.tabs.iter().position(|t| t.id == id) else {
            return false;
        };
        if pos == self.active_tab {
            return false;
        }
        self.active_tab = pos;
        if self.tabs[pos].layout_stale {
            let (width, height) = self.window_logical_size;
            let (tab, mut ctx) = self.split_active();
            if let Err(e) = resize_tab_viewport(tab, &mut ctx, width, height, None) {
                tracing::error!(error = ?e, "relayout on tab switch failed");
            }
        }
        true
    }

    /// The currently displayed tab, mutably.
    pub fn active_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active_tab]
    }
}

/// A parsed document, ready to be installed into a tab.
///
/// Groups the four artefacts the parser produces so [`TabState::new`] takes one
/// argument instead of four positional ones that are trivially swappable at a
/// call site (`style_rules` and `logic_fns` are both maps keyed by name-ish
/// types).
pub struct TabDocument {
    /// The document tree.
    pub dom: Tree<MizuNode>,
    /// Style rules, keyed by selector.
    pub style_rules: HashMap<String, StyleRules>,
    /// Media/scheme variants, applied over `style_rules` per render env.
    pub style_variants: Vec<StyleVariant>,
    /// Top-level logic functions, keyed by interned name.
    pub logic_fns: FxHashMap<Symbol, MizuFunction>,
}

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
            url: initial_url.to_string(),
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
            store: VariableStore::new(),
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
            has_user_gesture: false,
            capability_policy: CapabilityPolicy::new(initial_url),
            root_timers: Vec::new(),
            root_timer_queue: BTreeMap::new(),
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
            crate::parser::logic_worker::LogicWorker::spawn(logic_worker_rx, logic_worker_tx)?;
        }

        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();

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
        let mut image_cache =
            lru::LruCache::new(IMAGE_CACHE_CAPACITY);

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
    #[cfg(test)]
    pub(crate) fn new_headless(tabs: Vec<TabState>) -> (Self, TestChannelKeepAlive) {
        assert!(!tabs.is_empty(), "a manager must always own at least one tab");
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
                    let sym = self.store.interner.get_or_intern(&var);
                    self.dependency_index.entry(sym).or_default().push(id);
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

        let _ = logic_tx.send((self.id, UiEvent::Reload(Box::new(ReloadPayload {
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
        }))));
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

/// Reloads `tab`'s document completely, resetting its layout and logic state.
///
/// `is_active` gates the OS window retitle: a background tab finishing a load
/// must not rename the window out from under the tab the user is looking at.
pub(super) fn reload_tab_document(
    tab: &mut TabState,
    ctx: &mut WindowCtx<'_>,
    doc: ReloadedDocument,
    is_active: bool,
) -> Result<(), MizuError> {
    let ReloadedDocument {
        dom,
        style_rules,
        style_variants,
        logic_fns,
        interner,
        computed_bindings,
        root_timers,
    } = doc;

    tab.root_timers = root_timers;
    // Old node ids die with the old tree — drop inspector selection state.
    tab.inspector.reset_document_state();
    tab.recent_mutations.clear();
    let mut taffy = TaffyTree::new();
    let mut node_to_taffy_id = HashMap::new();

    let env = RenderEnvironment {
        viewport: tab.viewport_size,
        color_scheme: ctx.preferences.color_scheme,
    };
    let root_taffy_id = crate::render::layout_bridge::build_taffy_tree(
        dom.root(),
        &mut crate::render::layout_bridge::TaffyBuildContext {
            style_rules_map: &style_rules,
            taffy: &mut taffy,
            node_to_taffy_id: &mut node_to_taffy_id,
            image_cache: ctx.image_cache,
            chrome_url: &tab.chrome_state.url,
            variants: &style_variants,
            env: &env,
        },
    )?;

    tab.dom = dom;
    // Keep the OS window title in sync with the newly loaded document's
    // `doc "..."` title attribute (falls back to the same default used at
    // startup, matching `render::window::event_loop`) — but only for the tab
    // actually on screen.
    if is_active && let Some(window) = ctx.window {
        let title = tab
            .dom
            .root()
            .value()
            .attributes
            .get("title")
            .cloned()
            .unwrap_or_else(|| "Mizu Navigator".to_string());
        window.set_title(&title);
    }
    tab.style_rules = style_rules;
    tab.style_variants = style_variants;
    tab.logic_fns = logic_fns;
    tab.computed_bindings = computed_bindings;
    tab.taffy = taffy;
    tab.node_to_taffy_id = node_to_taffy_id;
    tab.root_taffy_id = root_taffy_id;

    tab.scroll_offsets.clear();
    tab.root_timer_queue.clear();
    tab.focused_node = None;
    tab.root_scroll_offset_y = 0.0;
    tab.chrome_state.focused = false;
    tab.chrome_state.selection = None;
    tab.text_layouts.clear();
    tab.text_dimensions.clear();
    tab.dirty_nodes.clear();
    tab.local_inputs.clear();
    tab.local_file_selections.clear();
    // The new Taffy tree has fresh node IDs; the old synthetic IDs are invalid.
    tab.each_expansion = EachExpansion::default();
    // Both are keyed by `EgoNodeId` from the *previous* DOM. Left behind they
    // grow by one document's worth of entries per navigation, and — worse than
    // the leak — an id reused by the new tree would seed virtualization with
    // another document's measured row height.
    tab.each_row_height_estimate.clear();
    tab.each_container_offset_y.clear();

    tab.rebuild_node_mappings();
    tab.store = VariableStore::with_interner(interner);
    tab.store
        .set("window_url", Value::from(tab.chrome_state.url.clone()));
    tab.rebuild_dependency_index();

    tab.trigger_logic_reload(ctx.logic_tx);
    // Freeze the UI interner so any runtime symbol additions (network results,
    // form fields not declared in logic) are flagged in logs — the logic worker
    // already holds a pre-freeze clone, so post-freeze symbols would diverge
    // between threads if they were ever used as raw IDs in inter-thread messages.
    tab.store.interner.freeze();

    tab.setup_timers();
    Ok(())
}

/// Recomputes `tab`'s Taffy layout against a new viewport boundary.
///
/// `dirty_list_names` controls whether `expand_each_nodes` does a full rebuild
/// (`None`) or only rebuilds the `Each` blocks whose backing list variables are
/// in the provided set (`Some(set)`). See [`expand_each_nodes`] for the contract.
pub(super) fn resize_tab_viewport(
    tab: &mut TabState,
    ctx: &mut WindowCtx<'_>,
    width: f32,
    height: f32,
    dirty_list_names: Option<std::collections::HashSet<String>>,
) -> Result<(), MizuError> {
    if width <= 0.0 || height <= 0.0 {
        return Ok(());
    }

    // The docked inspector panel reduces the document's usable width.
    // Centralised here so every call site (resize, F12 toggle, timers)
    // automatically lays the document out in the remaining space. Per-tab,
    // because the inspector's open state is per-tab.
    let width = if tab.inspector.open {
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
    // state) — the same construction `reload_tab_document` uses, so a
    // resize's responsive re-styling is exactly as correct as a fresh
    // document load, just without re-parsing anything. Bounded by the
    // same ≥16ms debounce this function is already only called behind
    // (see `window::event_loop`'s `WindowEvent::Resized` handler) — "not
    // on every resize pixel", per the design memo.
    tab.viewport_size = ViewportSize {
        width,
        height: content_height,
    };
    tab.layout_stale = false;
    let env = RenderEnvironment {
        viewport: tab.viewport_size,
        color_scheme: ctx.preferences.color_scheme,
    };
    let mut new_taffy = TaffyTree::new();
    let mut new_node_to_taffy_id = HashMap::new();
    let new_root_taffy_id = crate::render::layout_bridge::build_taffy_tree(
        tab.dom.root(),
        &mut crate::render::layout_bridge::TaffyBuildContext {
            style_rules_map: &tab.style_rules,
            taffy: &mut new_taffy,
            node_to_taffy_id: &mut new_node_to_taffy_id,
            image_cache: ctx.image_cache,
            chrome_url: &tab.chrome_state.url,
            variants: &tab.style_variants,
            env: &env,
        },
    )?;
    tab.taffy = new_taffy;
    tab.node_to_taffy_id = new_node_to_taffy_id;
    tab.root_taffy_id = new_root_taffy_id;
    // The rebuilt tree has fresh synthetic-node bookkeeping — the old
    // each-expansion's `groups`/`original_children`/`all_synthetic_ids`
    // reference taffy node ids that no longer exist in `tab.taffy`, so
    // they must not be reused (`expand_each_nodes`'s "restore the
    // previous expansion" step would otherwise operate on stale/
    // possibly-reused ids). `truncated` is keyed by `EgoNodeId`, which
    // *is* still meaningful, and is kept so the budget-change log below
    // compares against the real previous count instead of always
    // reading 0 (which would log a spurious "budget exceeded" on every
    // resize of a document with any truncated list).
    let prev_truncated = std::mem::take(&mut tab.each_expansion.truncated);
    tab.each_expansion = EachExpansion::default();

    if let Ok(mut style) = tab.taffy.style(tab.root_taffy_id).cloned() {
        style.min_size.height = taffy::style::Dimension::Length(content_height);
        style.size.height = taffy::style::Dimension::Auto;
        let _ = tab.taffy.set_style(tab.root_taffy_id, style);
    }

    // Expand `Each` nodes in Taffy to match the current list lengths.
    // Must run before `compute_layout_with_measure` so Taffy sees the
    // full N-row tree and produces correct per-item positions.
    let new_expansion = expand_each_nodes(
        &tab.dom,
        &tab.store,
        &mut tab.taffy,
        &tab.node_to_taffy_id,
        &tab.each_expansion,
        dirty_list_names.as_ref(), // None = full rebuild; Some(set) = granular
        tab.root_scroll_offset_y,
        content_height,
        &tab.each_container_offset_y,
        &tab.each_row_height_estimate,
    )?;

    for (node_id, &new_count) in &new_expansion.truncated {
        let old_count = prev_truncated.get(node_id).copied().unwrap_or(0);
        if new_count != old_count {
            let msg = format!("budget exceeded: clamped list to hide {} items", new_count);
            tab.inspector_log
                .push_event(crate::render::inspector::log::EventKind::Layout, msg.clone());
            tracing::warn!("{}", msg);
        }
    }
    for (node_id, &old_count) in &prev_truncated {
        if !new_expansion.truncated.contains_key(node_id) {
            let msg = format!(
                "budget restored: previously clamped {} items now visible",
                old_count
            );
            tab.inspector_log
                .push_event(crate::render::inspector::log::EventKind::Layout, msg.clone());
            tracing::warn!("{}", msg);
        }
    }

    tab.each_expansion = new_expansion;

    let dom = &tab.dom;
    let style_rules = &tab.style_rules;
    let style_variants = &tab.style_variants;
    let render_env = RenderEnvironment {
        viewport: tab.viewport_size,
        color_scheme: ctx.preferences.color_scheme,
    };
    let font_cx = &mut *ctx.font_cx;
    let layout_cx = &mut *ctx.layout_cx;
    let store = &tab.store;
    let text_layouts = &mut tab.text_layouts;
    let text_dimensions = &mut tab.text_dimensions;
    let dirty_nodes = &mut tab.dirty_nodes;
    let local_inputs = &tab.local_inputs;
    let node_id_to_u32 = &tab.node_id_to_u32;
    let focused_input = tab.focused_node;

    tab.taffy
        .compute_layout_with_measure(
            tab.root_taffy_id,
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

                    if let Some((dims, layout)) = crate::render::text_engine::calculate_node_text(
                        node_id,
                        available_width,
                        &mut crate::render::text_engine::TextLayoutContext {
                            dom,
                            style_rules,
                            font_cx: &mut *font_cx,
                            layout_cx: &mut *layout_cx,
                            store,
                            local_inputs,
                            node_id_to_u32,
                            focused_input,
                            style_variants,
                            render_env: &render_env,
                        },
                    ) {
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

    // Refresh each virtualized `Each` block's row-height estimate from
    // this frame's real Taffy measurements, so the *next* layout pass
    // (which rebuilds `tab.taffy` from scratch and can't see these
    // synthetic row ids anymore) windows against real data instead of
    // `DEFAULT_ROW_HEIGHT_ESTIMATE_PX`. Cheap: one `layout()` lookup per
    // currently-visible row, not per list element.
    let mut estimates: Vec<(EgoNodeId, f32)> = Vec::new();
    for (each_dom_id, groups) in &tab.each_expansion.groups {
        if groups.is_empty() {
            continue;
        }
        let mut total = 0.0f32;
        let mut count = 0usize;
        for (row_id, _) in groups {
            if let Ok(layout) = tab.taffy.layout(*row_id) {
                total += layout.size.height;
                count += 1;
            }
        }
        if count > 0 {
            estimates.push((*each_dom_id, total / count as f32));
        }
    }
    for (each_dom_id, estimate) in estimates {
        tab.each_row_height_estimate.insert(each_dom_id, estimate);
    }

    Ok(())
}

/// Cheaply checks whether the current scroll position still falls inside
/// each virtualized `Each` block's already-expanded row window (plus a
/// small slack margin), and only pays for a real re-expansion when it
/// doesn't. Returns `true` when a re-layout actually happened.
pub(super) fn refresh_tab_virtualized_windows(
    tab: &mut TabState,
    ctx: &mut WindowCtx<'_>,
    viewport_height: f32,
) -> Result<bool, MizuError> {
    let scroll_y = tab.root_scroll_offset_y;
    let slack_rows = (*crate::render::layout_bridge::VIRTUALIZATION_BUFFER_ROWS / 2).max(1) as isize;

    let mut dirty: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (&each_dom_id, &window_start) in &tab.each_expansion.window_start {
        let Some(groups) = tab.each_expansion.groups.get(&each_dom_id) else {
            continue;
        };
        let Some(list_name) = tab
            .dom
            .get(each_dom_id)
            .and_then(|n| n.value().iterator_context.as_ref())
            .map(|(_, name)| name.clone())
        else {
            continue;
        };
        let n = match tab.store.get(&list_name).ok() {
            Some(Value::List(arc)) => arc.len(),
            _ => continue,
        };

        let y0 = tab
            .each_container_offset_y
            .get(&each_dom_id)
            .copied()
            .unwrap_or(0.0);
        let row_h = tab
            .each_row_height_estimate
            .get(&each_dom_id)
            .copied()
            .unwrap_or(*crate::render::layout_bridge::DEFAULT_ROW_HEIGHT_ESTIMATE_PX)
            .max(1.0);
        let buffer = *crate::render::layout_bridge::VIRTUALIZATION_BUFFER_ROWS as f32 * row_h;

        let needed_first = (((scroll_y - y0 - buffer) / row_h).floor().max(0.0) as usize).min(n);
        let needed_last = (((scroll_y + viewport_height - y0 + buffer) / row_h)
            .ceil()
            .max(0.0) as usize)
            .clamp(needed_first, n);

        let window_end = window_start + groups.len();
        let still_covered = needed_first as isize >= window_start as isize - slack_rows
            && needed_last as isize <= window_end as isize + slack_rows;

        if !still_covered {
            dirty.insert(list_name);
        }
    }

    if dirty.is_empty() {
        return Ok(false);
    }

    // Reuse the last-known viewport size — this is a scroll-driven
    // refresh, not a real resize, so there is no new width/height to
    // query. `tab.viewport_size` already has the inspector panel width
    // subtracted (and chrome height), so undo both before passing back
    // into `resize_tab_viewport`, which subtracts them again itself.
    let width = tab.viewport_size.width
        + if tab.inspector.open {
            crate::render::inspector::PANEL_WIDTH
        } else {
            0.0
        };
    let height = tab.viewport_size.height + CHROME_HEIGHT;
    resize_tab_viewport(tab, ctx, width, height, Some(dirty))?;
    Ok(true)
}

/// Executes a declarative capability action for `tab`, recording
/// network-visible dispatches (and policy blocks) in that tab's inspector log.
///
/// The origin and the capability budget both come from `tab` — never from the
/// active tab — so a background tab's action is always judged against its own
/// origin's quota (invariant T1).
pub(super) fn execute_tab_capability_action(
    tab: &mut TabState,
    ctx: &WindowCtx<'_>,
    action: RuntimeAction,
) {
    use crate::render::inspector::log::NetOutcome;
    use crate::render::security::CapabilityOutcome;

    // Describe network-visible actions before the action is moved.
    let described: Option<(String, String, Option<String>)> = match &action {
        RuntimeAction::ResolvedCall {
            method,
            url,
            target_variable,
            ..
        } => Some((
            method.clone(),
            url.clone(),
            Some(target_variable.0.to_string()),
        )),
        RuntimeAction::StoreLocal { key, .. } => Some(("STORE".to_string(), key.clone(), None)),
        RuntimeAction::DownloadMedia { url } => Some(("MEDIA".to_string(), url.clone(), None)),
        _ => None,
    };

    let outcome = crate::render::security::execute_capability_action(
        &mut tab.store,
        ctx.network_tx,
        ctx.logic_tx,
        tab.id,
        &tab.chrome_state.url,
        &mut tab.capability_policy,
        action,
    );

    if let Some((verb, target, correlation)) = described {
        match outcome {
            CapabilityOutcome::Blocked(reason) => {
                tab.inspector_log.push_net_blocked(&verb, &target, reason);
            }
            CapabilityOutcome::Dispatched => {
                if verb == "STORE" {
                    // Fire-and-forget: no completion message flows back.
                    tab.inspector_log
                        .push_net_done(&verb, &target, NetOutcome::Ok);
                } else {
                    tab.inspector_log
                        .push_net_start(&verb, &target, correlation);
                }
            }
        }
    }
}
