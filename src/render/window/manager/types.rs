//! Core data types: `TabState`, `MizuWindowManager`, `ReloadedDocument`,
//! `TabDocument`, `WindowCtx` (the per-tab-operation borrow bundle), and the
//! test-only `TestChannelKeepAlive`.

use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};

use crate::render::chrome_vello::ChromeState;
use ego_tree::{NodeId as EgoNodeId, Tree};
use taffy::TaffyTree;
use winit::window::Window;

use super::super::AssetSlot;
use super::super::history::{HistoryLog, HistorySidebarState, HistoryStack};
use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol, VariableStore};
use crate::network::{TabId, UiEvent, WorkerResponse};
use crate::parser::logic::{ComputedBinding, MizuFunction, RootTimer};
use crate::parser::style::StyleVariant;
use crate::parser::{MizuNode, StyleRules};
use crate::render::layout_bridge::EachExpansion;
use crate::render::preferences::UserPreferences;
use crate::render::responsive::ViewportSize;
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
pub(super) fn next_a11y_epoch() -> u32 {
    let epoch = A11Y_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if epoch == 0 {
        A11Y_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    } else {
        epoch
    }
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
    SPAWN_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Maximum number of consecutive server redirects honoured for a single
/// user-initiated navigation before the chain is aborted.  Prevents a hostile
/// or misconfigured server from trapping the client in an infinite redirect
/// loop.
pub(crate) static MAX_REDIRECTS: LazyLock<u32> =
    LazyLock::new(|| crate::core::config::CONFIG.max_redirects);

/// Maximum root-timer ticks a single tab may have outstanding with the logic
/// worker at once — see [`TabState::may_dispatch_timer_tick`].
///
/// **T1:** counted per tab, so one document's backlog can neither consume nor
/// suppress another's timers.
///
/// Sized to give a healthy worker room to pipeline (a document's timers all
/// coming due in the same tick must not throttle each other) while keeping the
/// queue depth a small constant when the worker falls behind. Lower is safer,
/// not better: too low and legitimate documents lose ticks under momentary
/// load; the value only has to be small enough that the backlog stays
/// negligible next to the per-tab state already resident.
pub const MAX_INFLIGHT_TIMER_TICKS: u32 = 128;

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
pub(super) const IMAGE_CACHE_CAPACITY: std::num::NonZeroUsize =
    match std::num::NonZeroUsize::new(200) {
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
/// Two fields are security-load-bearing and must never be read or written
/// across a tab boundary (invariant **T1**, `SECURITY-INVARIANTS.md`):
/// `capability_policy` (per-origin storage quota + write rate limit) and
/// `redirect_count` (a per-navigation-chain budget). Hoisting either back to
/// window level is a security regression, not a simplification.
///
/// User-gesture agency is deliberately *not* a field here. It gates N3's
/// cross-origin navigation check and the clipboard, and it is carried
/// per-action-batch on [`crate::network::WorkerResponse::gesture`] instead:
/// a flag stored on the tab would be set when an input is dispatched and read
/// when some later, unrelated worker response is drained, letting a timer- or
/// network-driven batch inherit a gesture it never received. T1 still holds —
/// a response is routed to the tab that produced it, so one tab's click
/// cannot authorise another's — but it now holds per action rather than
/// per tab.
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
    /// Per-origin capability budget (storage quota + rate limit) for this
    /// tab's current origin. **T1:** per-tab so a low-trust origin cannot
    /// consume — or benefit from — quota attributable to a different origin
    /// open in another tab. Rebuilt on every navigation within this tab.
    ///
    /// Rebuilding is a *rate-limit* reset only: the byte quota is accumulated
    /// per origin in the window's shared `StorageUsageLedger`, precisely so
    /// that navigating (which a document can do to itself, ungated, whenever
    /// it likes) cannot hand it a fresh budget. See
    /// [`mizu_core::security::quota::StorageUsageLedger`].
    pub capability_policy: CapabilityPolicy,
    /// Root-level `timer` declarations from the `logic` block.
    pub root_timers: Vec<RootTimer>,
    /// Priority queue of pending root-timer deadlines.
    pub root_timer_queue: BTreeMap<std::time::Instant, Vec<usize>>,
    /// Timer ticks dispatched to the logic worker for this tab that have not
    /// yet been answered — the admission counter behind
    /// [`Self::may_dispatch_timer_tick`].
    pub inflight_timer_ticks: u32,
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
    /// Storage bytes charged per origin, for the life of the process.
    ///
    /// Window-level rather than per-tab on purpose: the quota bounds bytes at
    /// rest, and two tabs on one origin write to a single encrypted store, so
    /// they must draw on a single budget. This is not a T1 exception — no
    /// origin can read, or spend, another's entry — it is what makes the quota
    /// per-*origin* rather than per-tab-per-page-load.
    pub storage_usage: crate::render::security::StorageUsageLedger,
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
    pub(super) _network_cmd_rx: tokio::sync::mpsc::UnboundedReceiver<crate::network::NetworkCmd>,
    pub(super) _network_result_tx: tokio::sync::mpsc::Sender<crate::network::NetworkResult>,
    pub(super) _logic_event_rx: std::sync::mpsc::Receiver<(TabId, UiEvent)>,
    pub(super) _logic_response_tx:
        std::sync::mpsc::Sender<(TabId, Result<WorkerResponse, MizuError>)>,
}

#[cfg(test)]
impl TestChannelKeepAlive {
    /// Takes every [`crate::network::NetworkCmd`] the manager has dispatched
    /// since the last drain.
    ///
    /// A `mizu://` navigation is asynchronous: the choke point emits a command
    /// and nothing else changes until a document actually commits. The command
    /// is therefore the only observable proof that a navigation was authorised,
    /// which is exactly what the N2/N3 tests need to assert on.
    pub(crate) fn drain_network_cmds(&mut self) -> Vec<crate::network::NetworkCmd> {
        let mut out = Vec::new();
        while let Ok(cmd) = self._network_cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
    }
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
pub(crate) struct WindowCtx<'a> {
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
    /// Window-level per-origin storage byte accounting, so `navigate_to_url`
    /// can rebuild the tab's `CapabilityPolicy` against the new origin without
    /// discarding what that origin has already spent.
    pub storage_usage: &'a crate::render::security::StorageUsageLedger,
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
