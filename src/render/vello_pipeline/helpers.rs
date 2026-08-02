//! Shared paint-pass helpers: color conversion, image-waiter registration,
//! the `PaintContext` scratch state, media URL resolution, and conditional
//! class evaluation.

use std::collections::HashMap;

use ego_tree::{NodeId as EgoNodeId, Tree};
use taffy::TaffyTree;
use vello::kurbo::Affine;
use vello::peniko::Color;

use rustc_hash::FxHashMap;

use crate::core::types::{Symbol, Value, VariableStore};
use crate::parser::logic::MizuFunction;
use crate::parser::{ConditionalClass, MizuNode, StyleRules};
use crate::render::layout_bridge::{EachGroupEntries, EachIterationOverrides};

/// Converts a `MizuColor` into a `vello::peniko::Color`.
pub fn to_vello_color(color: &crate::parser::MizuColor) -> Color {
    Color::rgba8(color.r, color.g, color.b, color.a)
}

/// Records `tab` as waiting on `url`'s in-flight fetch, if it isn't already.
///
/// Linear scan: the list is one or two entries in every realistic case (it is
/// the number of open tabs showing the same image at the same moment).
pub(super) fn register_image_waiter(
    waiters: &mut rustc_hash::FxHashMap<String, Vec<crate::network::TabId>>,
    url: &str,
    tab: crate::network::TabId,
) {
    let list = waiters.entry(url.to_owned()).or_default();
    if !list.contains(&tab) {
        list.push(tab);
    }
}

/// Context holding references required for painting.
pub struct PaintContext<'a> {
    /// Tab being painted. Stamped on the `FetchImage` commands this pass
    /// issues so the completion is routed back to the tab that needs the
    /// relayout, not to whichever tab happens to be active when it lands.
    pub tab: crate::network::TabId,
    /// Reference to the DOM tree.
    pub tree: &'a Tree<MizuNode>,
    /// Reference to the computed Taffy layout tree.
    pub taffy: &'a TaffyTree<EgoNodeId>,
    /// Mapping of DOM Node IDs to Taffy Node IDs.
    pub node_to_taffy_id: &'a HashMap<EgoNodeId, taffy::prelude::NodeId>,
    /// Active CSS styles.
    pub style_rules: &'a HashMap<String, StyleRules>,
    /// Breakpoint/color-scheme style variants (ux-6). Empty for callers that
    /// don't need responsive behavior (e.g. tests).
    pub style_variants: &'a [crate::parser::style::StyleVariant],
    /// Current window-width/color-scheme snapshot variants are resolved
    /// against (ux-6).
    pub render_env: crate::render::responsive::RenderEnvironment,
    /// Parley font context.
    pub font_cx: &'a mut parley::FontContext,
    /// Parley layout context.
    pub layout_cx: &'a mut parley::LayoutContext<vello::peniko::Color>,
    /// Global transformation applied to the scene (e.g. for high-DPI scaling).
    pub transform: Affine,
    /// The runtime variable store (mutable so `push_local`/`truncate_locals` can be
    /// used in the hot conditional-class loop without cloning the whole Evaluator).
    pub store: &'a mut VariableStore,
    /// Vertical scroll offsets (logical pixels) for nodes with `overflow scroll`.
    ///
    /// Borrowed from [`crate::render::window::MizuWindowManager::scroll_offsets`].
    pub scroll_offsets: &'a HashMap<EgoNodeId, f32>,
    /// Currently focused node for text input.
    pub focused_node: Option<EgoNodeId>,
    /// Cache for decoded images.
    pub image_cache: &'a mut lru::LruCache<String, crate::render::window::AssetSlot>,
    /// Tabs waiting on each in-flight image URL.
    ///
    /// Presence of a key means "in flight" (the dedupe the decoded-image cache's
    /// `Loading` slot also expresses); the value is every tab that must relayout
    /// when the bytes land. A bare set would let a second tab requesting the
    /// same URL be deduped and then never notified.
    pub fetching_images: &'a mut rustc_hash::FxHashMap<String, Vec<crate::network::TabId>>,
    /// Elapsed time in milliseconds.
    pub elapsed_ms: u64,
    /// MPSC sender for network requests.
    pub network_tx: &'a tokio::sync::mpsc::UnboundedSender<crate::network::NetworkCmd>,
    /// The current base URL.
    pub chrome_url: &'a str,
    /// Flag indicating if an animated image was drawn
    pub has_animations: bool,
    /// Cached text layouts.
    pub text_layouts: &'a HashMap<EgoNodeId, parley::Layout<vello::peniko::Color>>,
    /// Per-iteration variable bindings injected by `each` loops.
    ///
    /// Checked before the global store during text interpolation so that
    /// `{item.field}` resolves to the current element's field value.
    /// Empty outside of `each` loops.
    pub item_bindings: HashMap<String, Value>,
    /// Expanded Taffy groups for all `Each` nodes: built by
    /// [`crate::render::layout_bridge::expand_each_nodes`] before each layout
    /// pass and consumed read-only during painting.
    pub each_groups: &'a HashMap<EgoNodeId, EachGroupEntries>,
    /// Absolute list index of `each_groups`'s first entry per `Each` node —
    /// `groups[i]` corresponds to list index `each_window_start[node] + i`
    /// once virtualization windowing is active. Missing entry means 0 (the
    /// whole list fit inside the window).
    pub each_window_start: &'a HashMap<EgoNodeId, usize>,
    /// Absolute Y offset of each `Each` container's top edge, written here
    /// every frame so the *next* layout pass's virtualization windowing
    /// knows where each block starts. See
    /// `MizuWindowManager::each_container_offset_y`.
    pub each_container_offset_y: &'a mut HashMap<EgoNodeId, f32>,
    /// Temporary per-iteration Taffy ID overrides installed by `paint_each`
    /// so that `paint_node` reads positions from the correct synthetic Taffy
    /// node rather than from the stale single-template node.
    /// Cleared between iterations and after `paint_each` returns.
    pub taffy_id_overrides: EachIterationOverrides,
}

/// Resolves a `background-image`/`image src` path to the absolute URL used as
/// the image-cache/fetch key, or `None` if this document may not reference it.
///
/// * An absolute `mizu://` path passes through unchanged — that is what a
///   declared `media` alias resolves to, for local and remote documents alike.
/// * An absolute `file://` path is accepted **only from a `file://`
///   document**. A remote document naming a local file is refused here.
/// * Anything else is resolved relative to the document's own origin: a
///   `mizu://<domain>` path for a `mizu://`-hosted document, or a path against
///   the document's own directory for a `file://` one.
/// * A path that cannot be resolved against either kind of origin is refused
///   rather than returned verbatim, so it can never be handed to the fetcher
///   as an unresolved, origin-less string.
///
/// # Why the `file://` rule lives here too
///
/// Two other guards already stop a remote document from naming a local file:
/// `parser::layout` rejects `file://` in `image src` for remote origins at
/// parse time, and the worker's `handle_fetch_file` denies every read when no
/// sandbox base was supplied (which is the case for a `mizu://` origin). Both
/// are real, and neither is a reason for *this* function to be fail-open: it
/// is the one place that turns a document-controlled string into a fetch URL,
/// and it was previously willing to emit `file:///…` for any origin at all.
/// A guard that only holds because of what its callers happen to check is one
/// edit away from not holding.
///
/// Shared by the background-image and inline-`image` paint paths, which
/// previously duplicated this resolution byte-for-byte.
pub(super) fn resolve_media_url(path: &str, chrome_url: &str) -> Option<String> {
    let origin_is_file = chrome_url.starts_with("file://");

    if path.starts_with("mizu://") {
        if origin_is_file {
            // A `file://` document declaring `media logo mizu://evil.com/x.png`
            // must not be able to reach an attacker-controlled host merely by
            // rendering or downloading an image — the same SSRF guard the
            // outbound network-call and download paths already enforce
            // (`execute_capability_action`'s `ResolvedCall`/`DownloadMedia`
            // arms). Only a local target is allowed; a parse failure is
            // treated as remote (fail-secure).
            let is_local = crate::network::uri::MizuUri::parse(path)
                .map(|u| crate::network::worker::is_local_host(&u.domain))
                .unwrap_or(false);
            return is_local.then(|| path.to_string());
        }
        return Some(path.to_string());
    }
    if path.starts_with("file://") {
        return origin_is_file.then(|| path.to_string());
    }
    if let Ok(base_uri) = crate::network::uri::MizuUri::parse(chrome_url) {
        let full_path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        return Some(format!("mizu://{}{}", base_uri.domain, full_path));
    }
    if let Some(file_path) = chrome_url.strip_prefix("file:///") {
        let base = std::path::Path::new(file_path);
        if let Some(parent) = base.parent() {
            // Traversal out of the document's directory is still possible in
            // this string (`../../secret.png`); it is caught at read time by
            // `handle_fetch_file`'s sandbox check, which resolves symlinks —
            // something this pure, I/O-free function cannot do.
            let resolved = parent.join(path);
            return Some(format!(
                "file:///{}",
                resolved.to_string_lossy().replace('\\', "/")
            ));
        }
    }
    None
}

/// Evaluates every conditional class (`class X if <expr>`) on `mizu_node`
/// against the current evaluator state and returns the merged style rules of
/// every truthy one, in declaration order.
///
/// This is the *only* place `paint_node` invokes the logic evaluator. Kept
/// as its own function (rather than inlined in the paint walk) so that the
/// paint/evaluator integration seam — exactly the class of "individually
/// correct pieces, wired together wrong" mistake this project has already
/// paid for once at the navigation choke point — has one function's worth of
/// surface to read and test, not a few hundred lines of surrounding paint
/// logic to read around it.
///
/// Injects `ctx.item_bindings` (the current `each`-iteration bindings) as
/// *local* variables (`push_local`) rather than deep-cloning the global
/// store, which caused O(N×G) heap allocation per frame where N = conditional
/// classes and G = global variable count. Protocol: snapshot the local-stack
/// height, push bindings, reset the per-condition instruction budget,
/// evaluate, then truncate back to the snapshot — zero heap allocation.
pub(super) fn evaluate_conditional_classes(
    mizu_node: &MizuNode,
    ctx: &mut PaintContext<'_>,
) -> StyleRules {
    let mut extra = StyleRules::default();
    if mizu_node.conditional_classes.is_empty() {
        return extra;
    }

    let empty_fns: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
    // Collect item_binding (name → sym, val) pairs ahead of the loop so that
    // we can split-borrow `ctx.store.evaluator` (mut) from `ctx.store.interner`
    // (immutable) without the borrow checker seeing overlapping &mut / & on the
    // same struct through the ctx.item_bindings reference.
    let binding_pairs: Vec<(Symbol, Value)> = ctx
        .item_bindings
        .iter()
        .filter_map(|(name, val)| ctx.store.interner.get(name).map(|sym| (sym, val.clone())))
        .collect();

    for cc in &mizu_node.conditional_classes {
        let frame = ctx.store.evaluator.local_stack.len();
        ctx.store.evaluator.instruction_count = 0;

        for (sym, val) in &binding_pairs {
            ctx.store.evaluator.push_local(*sym, val.clone());
        }

        // Split-borrow: evaluator is mutably borrowed for evaluate();
        // interner is immutably borrowed as a separate field of VariableStore.
        // Rust allows this because they are distinct struct fields.
        let resolved_class_name: Option<std::sync::Arc<str>> = match cc {
            ConditionalClass::Toggle {
                class_name,
                condition,
            } => {
                let sm = &mut ctx.store.evaluator;
                let interner = &ctx.store.interner;
                let is_truthy = sm
                    .evaluate(condition.root(), 0, &empty_fns, interner, &condition.arena)
                    .map(|v| matches!(v, Value::Bool(true)))
                    .unwrap_or(false);
                is_truthy.then(|| std::sync::Arc::from(class_name.as_str()))
            }
            ConditionalClass::Ternary { expr } => {
                let sm = &mut ctx.store.evaluator;
                let interner = &ctx.store.interner;
                sm.evaluate(expr.root(), 0, &empty_fns, interner, &expr.arena)
                    .ok()
                    .and_then(|v| match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    })
            }
        };

        // Rewind — O(injected_bindings) pops, zero heap allocation.
        ctx.store.evaluator.truncate_locals(frame);

        if let Some(class_name) = resolved_class_name
            && let Some(rules) = ctx.style_rules.get(class_name.as_ref())
        {
            extra = extra.merge(rules.clone());
        }
    }
    extra
}
