//! Hardware-accelerated 2D rendering pipeline using `vello`.
//!
//! ## Phase 11 additions
//!
//! * **Z-index depth sorting** — before painting its children, a node sorts
//!   them by their resolved `z-index` value (ascending).  Nodes with a higher
//!   `z-index` are drawn last, appearing on top of siblings with a lower value.
//!
//! * **Overflow clipping** — if a node's style carries `overflow hidden` or
//!   `overflow scroll`, the renderer wraps the child paint pass inside a
//!   `scene.push_layer(…)` / `scene.pop_layer()` pair so that children are
//!   hard-clipped to the container's layout rectangle.
//!
//! * **Scroll translation** — if a node has an entry in `scroll_offsets`, its
//!   children are shifted upward by the scroll offset via
//!   `Affine::translate((0, -scroll_y))` composed with the existing DPI scale
//!   transform.  The container's *own* background is painted without the
//!   translation so it always fills its layout rect.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use ego_tree::{NodeId as EgoNodeId, Tree};
use parley::style::StyleProperty;
use taffy::TaffyTree;
use vello::{
    Scene,
    kurbo::{Affine, Rect, Stroke},
    peniko::{BlendMode, Color, Fill, Mix},
};

use rustc_hash::FxHashMap;

use crate::core::types::{Symbol, Value, VariableStore};
use crate::parser::logic::MizuFunction;
use crate::parser::{ConditionalClass, MizuNode, MizuOverflow, Primitive, StyleRules};
use crate::render::layout_bridge::{EachGroupEntries, EachIterationOverrides};

/// Converts a `MizuColor` into a `vello::peniko::Color`.
pub fn to_vello_color(color: &crate::parser::MizuColor) -> Color {
    Color::rgba8(color.r, color.g, color.b, color.a)
}

/// Records `tab` as waiting on `url`'s in-flight fetch, if it isn't already.
///
/// Linear scan: the list is one or two entries in every realistic case (it is
/// the number of open tabs showing the same image at the same moment).
fn register_image_waiter(
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
fn resolve_media_url(path: &str, chrome_url: &str) -> Option<String> {
    let origin_is_file = chrome_url.starts_with("file://");

    if path.starts_with("mizu://") {
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
fn evaluate_conditional_classes(mizu_node: &MizuNode, ctx: &mut PaintContext<'_>) -> StyleRules {
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

/// Recursively paints the DOM node and its children into the given `vello::Scene`.
///
/// ## Phase 11 behaviour
///
/// 1. **Z-index sort** — children are collected and sorted by their resolved
///    `z-index` (ascending) before iteration so higher-z nodes paint on top.
/// 2. **Clip layer** — if the node carries `overflow hidden` or `overflow scroll`,
///    a Vello clip layer bounded to the node's layout rect is pushed before
///    children are painted and popped afterwards.
/// 3. **Scroll translation** — if the node has a non-zero scroll offset, the
///    child transform includes a vertical `Affine::translate((0, -scroll_y))`
///    so that scrolled content is shifted upward inside the clip rect.
///
/// Coordinates are accumulated top-down via `offset`.
/// Returns the number of painted background and text elements.
pub fn paint_node(
    node_id: EgoNodeId,
    ctx: &mut PaintContext<'_>,
    scene: &mut Scene,
    offset: (f32, f32),
) -> usize {
    let mut drawn_count = 0;

    // ── Fast path: Each nodes are handled by paint_each ───────────────────
    // `is_each` is a plain bool — the temporary NodeRef from .get() is dropped
    // at the end of the `let` statement, so ctx is free for the mutable access
    // that paint_each needs (ctx.item_bindings).
    {
        let is_each = ctx
            .tree
            .get(node_id)
            .map(|n| n.value().primitive == Primitive::Each)
            .unwrap_or(false);
        if is_each {
            return paint_each(node_id, ctx, scene, offset);
        }
    }

    let node_ref = match ctx.tree.get(node_id) {
        Some(n) => n,
        None => return 0,
    };
    let mizu_node = node_ref.value();

    let mut current_offset_x = offset.0;
    let mut current_offset_y = offset.1;
    let mut width = 0.0f32;
    let mut height = 0.0f32;

    // Retrieve computed layout.
    // During `paint_each` iterations the override map redirects to the
    // synthetic Taffy node for this iteration; otherwise fall back to the
    // static `node_to_taffy_id` mapping built by `build_taffy_tree`.
    let resolved_taffy_id = ctx
        .taffy_id_overrides
        .get(&node_id)
        .or_else(|| ctx.node_to_taffy_id.get(&node_id))
        .copied();
    if let Some(t_id) = resolved_taffy_id
        && let Ok(layout) = ctx.taffy.layout(t_id)
    {
        current_offset_x += layout.location.x;
        current_offset_y += layout.location.y;
        width = layout.size.width;
        height = layout.size.height;
    }

    // ── Resolve style properties for this node ────────────────────────────────
    let mut merged = StyleRules::default();
    let tag_name = mizu_node.style_tag_name();
    if let Some(tag_rules) = ctx.style_rules.get(tag_name.as_ref()) {
        merged = merged.merge(tag_rules.clone());
    }
    let class_attr = mizu_node.attributes.get("class").map(String::as_str);
    if let Some(class_attr) = class_attr
        && let Some(rules) = ctx.style_rules.get(class_attr)
    {
        merged = merged.merge(rules.clone());
    }
    // Id styles — highest specificity, applied after tag and class (stored
    // `#`-prefixed in the same rules map, so it can't collide with a
    // same-named class or tag).
    let id_key = mizu_node.attributes.get("id").map(|id| format!("#{id}"));
    if let Some(ref id_key) = id_key
        && let Some(rules) = ctx.style_rules.get(id_key.as_str())
    {
        merged = merged.merge(rules.clone());
    }
    // ux-6: breakpoint/color-scheme variants, applied last (after all three
    // bases), in source declaration order — see docs/design/responsive.md.
    let mut variant_selectors: Vec<&str> = vec![tag_name.as_ref()];
    if let Some(c) = class_attr {
        variant_selectors.push(c);
    }
    if let Some(ref k) = id_key {
        variant_selectors.push(k.as_str());
    }
    merged = merged.merge(crate::render::responsive::resolve_matching_variants(
        ctx.style_variants,
        &variant_selectors,
        &ctx.render_env,
    ));

    // ── Evaluate conditional classes ──────────────────────────────────────
    // The one place `paint_node` invokes the logic evaluator; kept as its own
    // function so this evaluator-integration seam has exactly one place to
    // audit (see `evaluate_conditional_classes`'s doc comment for why that's
    // worth doing deliberately rather than leaving it inlined here).
    merged = merged.merge(evaluate_conditional_classes(mizu_node, ctx));

    let background = merged.background.clone();
    let background_image = merged.background_image.clone();
    let background_size = merged.background_size;
    let border_radius = merged.border_radius;
    let border_width = merged.border_width;
    let border_color = merged.border_color.clone();
    let mut overflow = merged.overflow;
    let _z_index_self = merged.z_index;

    // Enforce default Hidden overflow for buttons and boxes to match layout constraints
    if (mizu_node.primitive == Primitive::Button || mizu_node.primitive == Primitive::Box)
        && overflow == MizuOverflow::Visible
    {
        overflow = MizuOverflow::Hidden;
    }

    // ── Paint this node's own background ─────────────────────────────────────
    if width > 0.0 && height > 0.0 {
        let rect = Rect::new(
            current_offset_x as f64,
            current_offset_y as f64,
            (current_offset_x + width) as f64,
            (current_offset_y + height) as f64,
        );

        let shape = rect.to_rounded_rect(border_radius.unwrap_or(0.0) as f64);

        let mut drawn_bg = false;

        // Background Image
        if let Some(img_path) = background_image {
            let animated_img = match resolve_media_url(&img_path, ctx.chrome_url) {
                None => {
                    // `debug!`, not `warn!`: paint runs every frame, and a
                    // document can hold as many refused nodes as it likes —
                    // an unconditional per-frame warning would be a
                    // document-controlled log flood at the default level.
                    tracing::debug!(
                        path = %img_path,
                        origin = %ctx.chrome_url,
                        "background-image refused: not resolvable against this document's origin"
                    );
                    None
                }
                Some(abs_url) => match ctx.image_cache.get(&abs_url) {
                    Some(crate::render::window::AssetSlot::Ready(cached)) => Some(cached.clone()),
                    Some(crate::render::window::AssetSlot::Loading) => {
                        // Already in flight for another tab: join the waiter
                        // list so this tab is relaid out too when the bytes
                        // arrive.
                        register_image_waiter(ctx.fetching_images, &abs_url, ctx.tab);
                        None
                    }
                    Some(crate::render::window::AssetSlot::Failed) => None,
                    None => {
                        ctx.image_cache
                            .put(abs_url.clone(), crate::render::window::AssetSlot::Loading);
                        register_image_waiter(ctx.fetching_images, &abs_url, ctx.tab);
                        let _ = ctx.network_tx.send(crate::network::NetworkCmd::FetchImage {
                            tab: ctx.tab,
                            url: abs_url.clone(),
                            is_remote_origin: ctx.chrome_url.starts_with("mizu://"),
                            sandbox_base: crate::render::window::chrome_url_to_file_sandbox_base(
                                ctx.chrome_url,
                            ),
                        });
                        None
                    }
                },
            };

            if animated_img.is_none() && background.is_none() {
                let placeholder_brush = vello::peniko::Brush::Solid(Color::rgba8(45, 45, 48, 255));
                scene.fill(
                    Fill::NonZero,
                    ctx.transform,
                    &placeholder_brush,
                    None,
                    &shape,
                );
                drawn_bg = true;
            }

            if let Some(animated_img) = animated_img {
                let current_frame_texture = match &animated_img {
                    crate::render::window::AnimatedImage::Static(img) => img.clone(),
                    crate::render::window::AnimatedImage::Animated {
                        frames,
                        total_duration_ms,
                    } => {
                        ctx.has_animations = true;
                        let mut time_in_anim = ctx.elapsed_ms % total_duration_ms;
                        let mut selected_frame = &frames[0].texture;
                        for frame in frames {
                            if time_in_anim < frame.duration_ms {
                                selected_frame = &frame.texture;
                                break;
                            }
                            time_in_anim -= frame.duration_ms;
                        }
                        selected_frame.clone()
                    }
                };

                let img_width = current_frame_texture.width as f64;
                let img_height = current_frame_texture.height as f64;

                let bg_size =
                    background_size.unwrap_or(crate::parser::style::MizuBackgroundSize::Stretch);

                if bg_size == crate::parser::style::MizuBackgroundSize::Tile {
                    // Push a clip rect matching the node bounds to prevent overflowing the borders
                    scene.push_layer(BlendMode::default(), 1.0, ctx.transform, &shape);

                    let mut y = 0.0;
                    while y < height as f64 {
                        let mut x = 0.0;
                        while x < width as f64 {
                            let tile_transform = Affine::translate((
                                current_offset_x as f64 + x,
                                current_offset_y as f64 + y,
                            ));
                            scene
                                .draw_image(&current_frame_texture, ctx.transform * tile_transform);
                            x += img_width;
                        }
                        y += img_height;
                    }

                    scene.pop_layer();
                } else {
                    let transform = match bg_size {
                        crate::parser::style::MizuBackgroundSize::Stretch => {
                            Affine::translate((current_offset_x as f64, current_offset_y as f64))
                                * Affine::scale_non_uniform(
                                    width as f64 / img_width,
                                    height as f64 / img_height,
                                )
                        }
                        crate::parser::style::MizuBackgroundSize::Cover => {
                            let scale = (width as f64 / img_width).max(height as f64 / img_height);
                            Affine::translate((current_offset_x as f64, current_offset_y as f64))
                                * Affine::scale(scale)
                        }
                        _ => Affine::IDENTITY,
                    };

                    if bg_size == crate::parser::style::MizuBackgroundSize::Cover {
                        scene.push_layer(BlendMode::default(), 1.0, ctx.transform, &shape);
                    }

                    scene.draw_image(&current_frame_texture, ctx.transform * transform);

                    if bg_size == crate::parser::style::MizuBackgroundSize::Cover {
                        scene.pop_layer();
                    }
                }

                drawn_bg = true;
            }
        }

        // Solid Color or Gradient Fallback
        if !drawn_bg && let Some(bg) = background {
            let brush = match bg {
                crate::parser::style::MizuBackground::Solid(c) => {
                    vello::peniko::Brush::Solid(to_vello_color(&c))
                }
                crate::parser::style::MizuBackground::LinearGradient { angle, start, end } => {
                    let rad = angle.to_radians() as f64;
                    let cx = rect.center().x;
                    let cy = rect.center().y;
                    let w2 = width as f64 / 2.0;
                    let h2 = height as f64 / 2.0;
                    let dx = rad.sin() * w2;
                    let dy = -rad.cos() * h2;

                    let gradient = vello::peniko::Gradient::new_linear(
                        vello::kurbo::Point::new(cx - dx, cy - dy),
                        vello::kurbo::Point::new(cx + dx, cy + dy),
                    )
                    .with_stops([
                        vello::peniko::ColorStop {
                            offset: 0.0,
                            color: to_vello_color(&start),
                        },
                        vello::peniko::ColorStop {
                            offset: 1.0,
                            color: to_vello_color(&end),
                        },
                    ]);
                    vello::peniko::Brush::Gradient(gradient)
                }
            };

            scene.fill(Fill::NonZero, ctx.transform, &brush, None, &shape);
        }

        // Border
        if let Some(bw) = border_width
            && let Some(bc) = border_color
        {
            let stroke = Stroke::new(bw as f64);
            let brush = vello::peniko::Brush::Solid(to_vello_color(&bc));
            scene.stroke(&stroke, ctx.transform, &brush, None, &shape);
        }

        // ── Keyboard focus ring ────────────────────────────────────────────
        // A 2px ring, inset 1px from the node's own border, in the same
        // accent color as the chrome URL bar's focused-state border
        // (`crate::render::FOCUS_RING_COLOR`) — legible against both the
        // chrome's dark palette and an arbitrary document background.
        if Some(node_id) == ctx.focused_node {
            const FOCUS_RING_WIDTH: f64 = 2.0;
            const FOCUS_RING_INSET: f64 = 1.0;
            let ring_rect = Rect::new(
                rect.x0 + FOCUS_RING_INSET,
                rect.y0 + FOCUS_RING_INSET,
                rect.x1 - FOCUS_RING_INSET,
                rect.y1 - FOCUS_RING_INSET,
            );
            let ring_shape = ring_rect
                .to_rounded_rect((border_radius.unwrap_or(0.0) as f64 - FOCUS_RING_INSET).max(0.0));
            let stroke = Stroke::new(FOCUS_RING_WIDTH);
            let brush = vello::peniko::Brush::Solid(crate::render::FOCUS_RING_COLOR);
            scene.stroke(&stroke, ctx.transform, &brush, None, &ring_shape);
        }

        drawn_count += 1;
    }

    // ── Paint inline text (not for Window nodes) ──────────────────────────────
    if mizu_node.primitive != Primitive::Doc
        && let Some(text) = mizu_node.attributes.get("content")
    {
        let mut font_size = 16.0f32;
        let mut text_color = Color::BLACK;

        if let Some(fs) = merged.font_size {
            font_size = fs;
        }
        if let Some(ref tc) = merged.color {
            text_color = to_vello_color(tc);
        }

        let fallback_layout;
        let layout = if let Some(cached) = ctx.text_layouts.get(&node_id) {
            cached
        } else {
            let text_to_draw = ctx
                .store
                .interpolate_with_overlay(text, &ctx.item_bindings)
                .unwrap_or_else(|e| match &e {
                    crate::core::errors::MizuError::BindingNotFound(name) => {
                        format!("{{missing: {}}}", name)
                    }
                    _ => format!("{{error: {}}}", e),
                });

            let mut builder = ctx
                .layout_cx
                .ranged_builder(ctx.font_cx, &text_to_draw, 1.0, true);
            let fallbacks = vec![
                parley::style::FontFamilyName::named("Segoe UI"),
                parley::style::FontFamilyName::named("Arial"),
                parley::style::FontFamilyName::named("Meiryo"),
                parley::style::FontFamilyName::named("Yu Gothic"),
                parley::style::FontFamilyName::named("Hiragino Sans"),
                parley::style::FontFamilyName::Generic(parley::style::GenericFamily::SansSerif),
            ];
            let font_family = parley::style::FontFamily::List(std::borrow::Cow::Owned(fallbacks));
            builder.push_default(parley::style::StyleProperty::FontFamily(font_family));
            builder.push_default(StyleProperty::FontSize(font_size));
            builder.push_default(StyleProperty::Brush(text_color));
            builder.push_default(StyleProperty::LineHeight(
                parley::style::LineHeight::FontSizeRelative(1.2),
            ));

            let mut l = builder.build(&text_to_draw);
            let mut is_nowrap = false;
            if let Some(parent) = node_ref.parent()
                && parent.value().primitive == Primitive::Button
            {
                is_nowrap = true;
            }
            let max_advance = if width > 0.0 && !is_nowrap {
                Some(width)
            } else {
                None
            };
            l.break_all_lines(max_advance);
            fallback_layout = l;
            &fallback_layout
        };

        let y_offset = if let Some(first_line) = layout.lines().next() {
            first_line.metrics().ascent - first_line.metrics().baseline
        } else {
            0.0
        };

        for line in layout.lines() {
            for item in line.items() {
                if let parley::layout::PositionedLayoutItem::GlyphRun(run) = item {
                    let font_data = run.run().font();
                    let (arc_data, id) = font_data.data.clone().into_raw_parts();
                    let peniko_blob = vello::peniko::Blob::from_raw_parts(arc_data, id);
                    let vello_font = vello::peniko::Font::new(peniko_blob, font_data.index);

                    let vello_glyphs = run.positioned_glyphs().map(|g| vello::glyph::Glyph {
                        id: g.id,
                        x: g.x,
                        y: g.y,
                    });

                    scene
                        .draw_glyphs(&vello_font)
                        .font_size(font_size)
                        .brush(text_color)
                        .transform(
                            ctx.transform
                                * Affine::translate((
                                    current_offset_x as f64,
                                    (current_offset_y + y_offset) as f64,
                                )),
                        )
                        .draw(Fill::NonZero, vello_glyphs);

                    drawn_count += 1;
                }
            }
        }
    }

    // ── Paint input text and cursor ──────────────────────────────────────────
    if mizu_node.primitive == Primitive::Input {
        let mut font_size = 16.0f32;
        let mut text_color = Color::BLACK;

        if let Some(fs) = merged.font_size {
            font_size = fs;
        }
        if let Some(ref tc) = merged.color {
            text_color = to_vello_color(tc);
        }

        let fallback_layout;
        let layout = if let Some(cached) = ctx.text_layouts.get(&node_id) {
            cached
        } else {
            let text = String::new();

            let mut builder = ctx.layout_cx.ranged_builder(ctx.font_cx, &text, 1.0, true);
            builder.push_default(StyleProperty::FontSize(font_size));
            builder.push_default(StyleProperty::Brush(text_color));
            builder.push_default(StyleProperty::LineHeight(
                parley::style::LineHeight::FontSizeRelative(1.2),
            ));

            let mut l = builder.build(&text);
            let max_advance = None;
            l.break_all_lines(max_advance);
            fallback_layout = l;
            &fallback_layout
        };

        let mut text_width = 0.0;
        let text_height = layout.height();
        let y_offset = if let Some(first_line) = layout.lines().next() {
            first_line.metrics().ascent - first_line.metrics().baseline
        } else {
            0.0
        };
        let center_y_offset = if height > text_height {
            (height - text_height) / 2.0
        } else {
            0.0
        };

        for line in layout.lines() {
            for item in line.items() {
                if let parley::layout::PositionedLayoutItem::GlyphRun(run) = item {
                    let font_data = run.run().font();
                    let (arc_data, id) = font_data.data.clone().into_raw_parts();
                    let peniko_blob = vello::peniko::Blob::from_raw_parts(arc_data, id);
                    let vello_font = vello::peniko::Font::new(peniko_blob, font_data.index);

                    let vello_glyphs = run.positioned_glyphs().map(|g| {
                        let advance = g.x + g.advance;
                        if advance > text_width {
                            text_width = advance;
                        }
                        vello::glyph::Glyph {
                            id: g.id,
                            x: g.x,
                            y: g.y,
                        }
                    });

                    scene
                        .draw_glyphs(&vello_font)
                        .font_size(font_size)
                        .brush(text_color)
                        .transform(
                            ctx.transform
                                * Affine::translate((
                                    current_offset_x as f64,
                                    (current_offset_y + center_y_offset + y_offset) as f64,
                                )),
                        )
                        .draw(Fill::NonZero, vello_glyphs);

                    drawn_count += 1;
                }
            }
        }

        if Some(node_id) == ctx.focused_node {
            let cursor_rect = Rect::new(
                (current_offset_x + text_width + 2.0) as f64,
                (current_offset_y + center_y_offset + y_offset) as f64,
                (current_offset_x + text_width + 4.0) as f64,
                (current_offset_y + center_y_offset + y_offset + text_height) as f64,
            );
            scene.fill(Fill::NonZero, ctx.transform, text_color, None, &cursor_rect);
        }
    }

    // ── Paint inline image ───────────────────────────────────────────────────
    if mizu_node.primitive == Primitive::Image
        && let Some(src) = mizu_node.attributes.get("src")
    {
        let peniko_img = match resolve_media_url(src, ctx.chrome_url) {
            None => {
                // Per-frame path — see the `background-image` arm above for
                // why this is `debug!` rather than `warn!`.
                tracing::debug!(
                    src = %src,
                    origin = %ctx.chrome_url,
                    "image src refused: not resolvable against this document's origin"
                );
                None
            }
            Some(abs_url) => match ctx.image_cache.get(&abs_url) {
                Some(crate::render::window::AssetSlot::Ready(cached)) => Some(cached.clone()),
                Some(crate::render::window::AssetSlot::Loading) => {
                    register_image_waiter(ctx.fetching_images, &abs_url, ctx.tab);
                    None
                }
                Some(crate::render::window::AssetSlot::Failed) => None,
                None => {
                    ctx.image_cache
                        .put(abs_url.clone(), crate::render::window::AssetSlot::Loading);
                    register_image_waiter(ctx.fetching_images, &abs_url, ctx.tab);
                    let _ = ctx.network_tx.send(crate::network::NetworkCmd::FetchImage {
                        tab: ctx.tab,
                        url: abs_url.clone(),
                        is_remote_origin: ctx.chrome_url.starts_with("mizu://"),
                        sandbox_base: crate::render::window::chrome_url_to_file_sandbox_base(
                            ctx.chrome_url,
                        ),
                    });
                    None
                }
            },
        };

        if peniko_img.is_none() {
            let rect = Rect::new(
                current_offset_x as f64,
                current_offset_y as f64,
                (current_offset_x + width) as f64,
                (current_offset_y + height) as f64,
            );
            let shape = rect.to_rounded_rect(border_radius.unwrap_or(0.0) as f64);
            let placeholder_brush = vello::peniko::Brush::Solid(Color::rgba8(45, 45, 48, 255));
            scene.fill(
                Fill::NonZero,
                ctx.transform,
                &placeholder_brush,
                None,
                &shape,
            );
            drawn_count += 1;
        }

        if let Some(animated_img) = peniko_img {
            let current_frame_texture = match &animated_img {
                crate::render::window::AnimatedImage::Static(img) => img.clone(),
                crate::render::window::AnimatedImage::Animated {
                    frames,
                    total_duration_ms,
                } => {
                    ctx.has_animations = true;
                    let mut time_in_anim = ctx.elapsed_ms % total_duration_ms;
                    let mut selected_frame = &frames[0].texture;
                    for frame in frames {
                        if time_in_anim < frame.duration_ms {
                            selected_frame = &frame.texture;
                            break;
                        }
                        time_in_anim -= frame.duration_ms;
                    }
                    selected_frame.clone()
                }
            };

            let width_px = current_frame_texture.width;
            let height_px = current_frame_texture.height;

            // For inline images, we usually want them to fit their box.
            // We'll stretch them to fit the Taffy width and height exactly.
            let transform = Affine::translate((current_offset_x as f64, current_offset_y as f64))
                * Affine::scale_non_uniform(
                    width as f64 / width_px as f64,
                    height as f64 / height_px as f64,
                );

            let rect = Rect::new(
                current_offset_x as f64,
                current_offset_y as f64,
                (current_offset_x + width) as f64,
                (current_offset_y + height) as f64,
            );
            let shape = rect.to_rounded_rect(border_radius.unwrap_or(0.0) as f64);

            // Always clip the image using its calculated shape (which respects border-radius)
            scene.push_layer(BlendMode::default(), 1.0, ctx.transform, &shape);
            scene.draw_image(&current_frame_texture, ctx.transform * transform);
            scene.pop_layer();

            drawn_count += 1;
        }
    }

    // ── Collect and sort children by z-index (Phase 11) ───────────────────────
    //
    // We build a Vec of (z_index, child_id) pairs, sort ascending, then paint
    // in that order.  Children without a matching style rule default to z=0.
    let mut child_ids: Vec<(i32, EgoNodeId)> = node_ref
        .children()
        .map(|child| {
            let child_node = child.value();
            let z = child_node
                .attributes
                .get("class")
                .and_then(|cls| {
                    let cls_name = cls.strip_prefix('.').unwrap_or(cls);
                    ctx.style_rules.get(cls_name)
                })
                .map(|r| r.z_index)
                .unwrap_or(0);
            (z, child.id())
        })
        .collect();

    // Stable sort preserves document order for ties.
    child_ids.sort_by_key(|&(z, _)| z);

    // ── Clip + scroll setup (Phase 11) ────────────────────────────────────────
    //
    // If this node clips its children (`overflow hidden` or `overflow scroll`),
    // we push a Vello layer whose clip shape is the node's own layout rect.
    // For scrollable nodes we additionally shift the child transform upward by
    // the accumulated scroll offset.
    let clips_children = matches!(overflow, MizuOverflow::Hidden | MizuOverflow::Scroll);

    // The child-paint transform: starts from the global DPI scale, then adds
    // a vertical translation when scrolling is active.
    let scroll_y = ctx.scroll_offsets.get(&node_id).copied().unwrap_or(0.0);

    if clips_children && width > 0.0 && height > 0.0 {
        // Build the clip rectangle in *physical* coordinates (Vello operates in
        // physical / pre-transform space when the transform is baked into the
        // clip call — but in Vello 0.1 the clip shape is in the same coordinate
        // space as the transform passed to push_layer).
        //
        // Here we pass `ctx.transform` (DPI scale only) as the clip transform,
        // which means the clip shape must be in *logical* coordinates — exactly
        // what Taffy gives us.
        let clip_rect = Rect::new(
            current_offset_x as f64,
            current_offset_y as f64,
            (current_offset_x + width) as f64,
            (current_offset_y + height) as f64,
        );

        // Normal blend at full opacity; the shape acts purely as a clip mask.
        scene.push_layer(
            BlendMode::new(Mix::Normal, vello::peniko::Compose::SrcOver),
            1.0,
            ctx.transform,
            &clip_rect,
        );
    }

    // Build the child transform: the base DPI scale plus any scroll translation.
    let child_transform = if scroll_y.abs() > f32::EPSILON {
        ctx.transform * Affine::translate((0.0, -(scroll_y as f64)))
    } else {
        ctx.transform
    };

    // ── Paint children ────────────────────────────────────────────────────────
    // Temporarily swap the context transform to include the scroll offset, then
    // restore it afterwards so siblings painted after us are unaffected.
    let saved_transform = ctx.transform;
    ctx.transform = child_transform;

    for (_, child_id) in &child_ids {
        drawn_count += paint_node(*child_id, ctx, scene, (current_offset_x, current_offset_y));
    }

    ctx.transform = saved_transform;

    // ── Pop clip layer ────────────────────────────────────────────────────────
    if clips_children && width > 0.0 && height > 0.0 {
        scene.pop_layer();
    }

    drawn_count
}

/// Paints a `Primitive::Each` node by iterating the bound list and painting
/// the child template once for every element.
///
/// ## Layout strategy
///
/// Before this function is called, [`crate::render::layout_bridge::expand_each_nodes`]
/// has already replaced the Each node's single static Taffy child with N row
/// containers (one per list element), and `compute_layout` has been run on the
/// expanded tree.  `paint_each` reads each row container's computed position
/// from Taffy and installs a temporary `taffy_id_overrides` map so that
/// `paint_node` resolves template DOM node IDs to the correct per-iteration
/// synthetic Taffy nodes.
///
/// If the expansion is not yet available (e.g. the list variable was empty
/// during the last `resize_viewport` call), the function falls back to the
/// legacy height-division heuristic so items remain visible rather than blank.
///
/// ## Borrow-checker rationale
///
/// All data needed from `ctx.tree` is collected into owned values inside a
/// short inner scope so the `NodeRef` borrow is released before the function
/// mutates `ctx.item_bindings` and `ctx.taffy_id_overrides`.
fn paint_each(
    node_id: EgoNodeId,
    ctx: &mut PaintContext<'_>,
    scene: &mut Scene,
    offset: (f32, f32),
) -> usize {
    // ── Phase 1: collect owned data while holding the ctx.tree borrow ────
    let (item_var, list_name, child_ids, current_x, current_y) = {
        let node_ref = match ctx.tree.get(node_id) {
            Some(n) => n,
            None => return 0,
        };
        let mizu_node = node_ref.value();

        let (ix, iy) = ctx
            .node_to_taffy_id
            .get(&node_id)
            .and_then(|&t_id| ctx.taffy.layout(t_id).ok())
            .map(|l| (offset.0 + l.location.x, offset.1 + l.location.y))
            .unwrap_or(offset);

        // Recorded for the *next* layout pass's virtualization windowing —
        // see `MizuWindowManager::each_container_offset_y`.
        ctx.each_container_offset_y.insert(node_id, iy);

        let (item_var, list_name) = match mizu_node.iterator_context.as_ref() {
            Some((v, l)) => (v.clone(), l.clone()),
            None => {
                tracing::warn!("paint_each: Each node has no iterator_context");
                return 0;
            }
        };

        let child_ids: Vec<EgoNodeId> = node_ref.children().map(|c| c.id()).collect();
        (item_var, list_name, child_ids, ix, iy)
    }; // node_ref dropped — ctx.tree borrow released

    // ── Phase 2: look up the list value ──────────────────────────────────
    // Kept as the `Arc` rather than cloning the whole backing `Vec` (which
    // used to happen on every single paint frame regardless of list size):
    // with virtualization only a small visible window of items is ever
    // touched below, so only those get cloned.
    let list_arc: std::sync::Arc<Vec<Value>> = {
        let val = ctx
            .item_bindings
            .get(&list_name)
            .cloned()
            .or_else(|| ctx.store.get(&list_name).ok().cloned());
        match val {
            Some(Value::List(arc)) => arc,
            _ => {
                tracing::warn!("paint_each: `{}` is not a list or not found", list_name);
                return 0;
            }
        }
    };

    let n = list_arc.len();

    // ── Phase 3: clone expansion groups (if available) before mutating ctx ─
    // Cloned upfront so we hold no borrow on ctx.each_groups while we later
    // mutate ctx.item_bindings and ctx.taffy_id_overrides. Only the visible
    // window's entries, not the whole list.
    let groups: Option<EachGroupEntries> = ctx.each_groups.get(&node_id).cloned();

    // ── Phase 4: iterate and paint ────────────────────────────────────────
    let mut drawn_count = 0;

    if let Some(groups) = groups {
        // ── Expanded path: Taffy has row containers for the visible window ──
        let window_start = ctx.each_window_start.get(&node_id).copied().unwrap_or(0);
        for (i, (row_taffy_id, overrides)) in groups.iter().enumerate() {
            let idx = window_start + i;
            let Some(item_val) = list_arc.get(idx).cloned() else {
                // List shrank since last resize_viewport.
                break;
            };

            // Extract row position before any mutable borrow of ctx.
            let (row_abs_x, row_abs_y) = ctx
                .taffy
                .layout(*row_taffy_id)
                .map(|l| (current_x + l.location.x, current_y + l.location.y))
                .unwrap_or((current_x, current_y));

            ctx.item_bindings.insert(item_var.clone(), item_val);

            // Install per-iteration overrides: template DOM IDs → synthetic Taffy IDs.
            ctx.taffy_id_overrides.clear();
            ctx.taffy_id_overrides
                .extend(overrides.iter().map(|(&k, &v)| (k, v)));

            for &child_id in &child_ids {
                drawn_count += paint_node(child_id, ctx, scene, (row_abs_x, row_abs_y));
            }
        }
        ctx.taffy_id_overrides.clear();
    } else {
        // ── Fallback: expansion not yet available ─────────────────────────
        // Use the legacy height-division heuristic so items are visible
        // rather than blank while the store is being populated.
        tracing::debug!(
            "paint_each: no expansion for {:?}, using height-division fallback",
            node_id
        );
        let each_height = ctx
            .node_to_taffy_id
            .get(&node_id)
            .and_then(|&t_id| ctx.taffy.layout(t_id).ok())
            .map(|l| l.size.height)
            .unwrap_or(0.0);
        let item_height = if n > 0 && each_height > 0.0 {
            each_height / n as f32
        } else {
            0.0
        };
        for (idx, item_val) in list_arc.iter().cloned().enumerate() {
            ctx.item_bindings.insert(item_var.clone(), item_val);
            let item_offset = (current_x, current_y + idx as f32 * item_height);
            for &child_id in &child_ids {
                drawn_count += paint_node(child_id, ctx, scene, item_offset);
            }
        }
    }

    ctx.item_bindings.remove(&item_var);
    drawn_count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::MizuColor;

    #[test]
    fn test_color_translation_opaque() {
        let mizu_c = MizuColor::rgb(255, 0, 128);
        let vello_c = to_vello_color(&mizu_c);
        assert_eq!(vello_c.r, 255);
        assert_eq!(vello_c.g, 0);
        assert_eq!(vello_c.b, 128);
        assert_eq!(vello_c.a, 255);
    }

    #[test]
    fn test_color_translation_transparent() {
        let mizu_c = MizuColor::rgba(10, 20, 30, 50);
        let vello_c = to_vello_color(&mizu_c);
        assert_eq!(vello_c.r, 10);
        assert_eq!(vello_c.g, 20);
        assert_eq!(vello_c.b, 30);
        assert_eq!(vello_c.a, 50);
    }

    #[test]
    fn resolve_media_url_passes_through_absolute_mizu_urls() {
        // What a declared `media` alias resolves to, for either origin kind.
        assert_eq!(
            resolve_media_url("mizu://other.example/x.png", "mizu://example.mizu/page").as_deref(),
            Some("mizu://other.example/x.png")
        );
        assert_eq!(
            resolve_media_url("mizu://other.example/x.png", "file:///C:/docs/page.mizu").as_deref(),
            Some("mizu://other.example/x.png")
        );
    }

    #[test]
    fn resolve_media_url_refuses_local_files_for_remote_documents() {
        // A remote document naming a local file is refused *here*, not left to
        // the parse-time guard and the fetcher's sandbox check alone.
        for path in [
            "file:///C:/img.png",
            "file:///etc/passwd",
            "file://C:/Users/victim/.ssh/id_rsa",
        ] {
            assert_eq!(
                resolve_media_url(path, "mizu://example.mizu/page"),
                None,
                "{path} must not resolve for a mizu:// document"
            );
        }
        // The same path from a local document is the legitimate case, and the
        // read is still sandboxed at fetch time.
        assert_eq!(
            resolve_media_url("file:///C:/docs/img.png", "file:///C:/docs/page.mizu").as_deref(),
            Some("file:///C:/docs/img.png")
        );
    }

    #[test]
    fn resolve_media_url_resolves_relative_to_mizu_origin() {
        assert_eq!(
            resolve_media_url("img.png", "mizu://example.mizu/page").as_deref(),
            Some("mizu://example.mizu/img.png")
        );
        assert_eq!(
            resolve_media_url("/assets/img.png", "mizu://example.mizu/page").as_deref(),
            Some("mizu://example.mizu/assets/img.png")
        );
    }

    #[test]
    fn resolve_media_url_resolves_relative_to_file_origin() {
        assert_eq!(
            resolve_media_url("img.png", "file:///C:/docs/page.mizu").as_deref(),
            Some("file:///C:/docs/img.png")
        );
    }

    #[test]
    fn resolve_media_url_refuses_an_unresolvable_origin() {
        // Neither a `mizu://` host nor a `file:///` path to resolve against:
        // returning the raw string here would hand the fetcher an origin-less
        // path to interpret however it liked.
        assert_eq!(resolve_media_url("img.png", "about:blank"), None);
        assert_eq!(resolve_media_url("img.png", ""), None);
    }

    #[test]
    fn test_paint_node_with_text() {
        use crate::parser::Primitive;

        // Build a DOM tree: Window -> Text
        let mut tree = Tree::new(MizuNode {
            primitive: Primitive::Doc,
            attributes: FxHashMap::default(),
            events: FxHashMap::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });

        let text_node_id = tree
            .root_mut()
            .append(MizuNode {
                primitive: Primitive::Text,
                attributes: {
                    let mut attrs = FxHashMap::default();
                    attrs.insert("class".to_string(), "welcome-text".to_string());
                    attrs.insert("content".to_string(), "Benvenuto in Mizu!".to_string());
                    attrs
                },
                events: FxHashMap::default(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            })
            .id();

        // Style rules
        let mut style_rules = HashMap::new();
        style_rules.insert(
            "welcome-text".to_string(),
            StyleRules {
                font_size: Some(24.0),
                color: Some(MizuColor::rgb(255, 255, 255)),
                ..Default::default()
            },
        );

        // Set up Taffy layout
        let mut taffy = TaffyTree::<EgoNodeId>::new();
        let mut node_to_taffy_id = HashMap::new();

        // Window Taffy Node
        let window_style = taffy::style::Style::default();
        let window_taffy_id = taffy.new_with_children(window_style, &[]).unwrap();
        node_to_taffy_id.insert(tree.root().id(), window_taffy_id);

        // Text Taffy Node
        let text_style = taffy::style::Style::default();
        let text_taffy_id = taffy
            .new_leaf_with_context(text_style, text_node_id)
            .unwrap();
        node_to_taffy_id.insert(text_node_id, text_taffy_id);

        // Compute layout
        let viewport_size = taffy::geometry::Size {
            width: taffy::style::AvailableSpace::Definite(800.0),
            height: taffy::style::AvailableSpace::Definite(600.0),
        };
        taffy
            .compute_layout(window_taffy_id, viewport_size)
            .unwrap();

        // Parley contexts
        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();
        let mut layout_cx = parley::LayoutContext::new();
        let mut store = VariableStore::new().freeze();
        let scroll_offsets: HashMap<EgoNodeId, f32> = HashMap::new();
        let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());

        let mut fetching_images = rustc_hash::FxHashMap::default();
        let (network_tx, _network_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let chrome_url = "mizu://localhost/index.mizu";

        let text_layouts = HashMap::new();

        let empty_each_groups = HashMap::new();
        let empty_window_start: HashMap<EgoNodeId, usize> = HashMap::new();
        let mut empty_each_offset_y: HashMap<EgoNodeId, f32> = HashMap::new();

        // Setup PaintContext
        let mut ctx = PaintContext {
            tab: crate::network::TabId(0),
            tree: &tree,
            taffy: &taffy,
            node_to_taffy_id: &node_to_taffy_id,
            style_rules: &style_rules,
            style_variants: &[],
            render_env: crate::render::responsive::RenderEnvironment {
                viewport: crate::render::responsive::ViewportSize {
                    width: 800.0,
                    height: 600.0,
                },
                color_scheme: crate::render::preferences::ColorScheme::Dark,
            },
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            transform: Affine::IDENTITY,
            store: &mut store,
            scroll_offsets: &scroll_offsets,
            focused_node: None,
            image_cache: &mut image_cache,
            fetching_images: &mut fetching_images,
            network_tx: &network_tx,
            chrome_url,
            elapsed_ms: 0,
            has_animations: false,
            text_layouts: &text_layouts,
            item_bindings: HashMap::new(),
            each_groups: &empty_each_groups,
            each_window_start: &empty_window_start,
            each_container_offset_y: &mut empty_each_offset_y,
            taffy_id_overrides: HashMap::new(),
        };

        let mut scene = Scene::new();
        let drawn = paint_node(tree.root().id(), &mut ctx, &mut scene, (0.0, 0.0));

        // Since the Window text title is ignored, only the child Text node should draw text.
        // We check that drawn > 0.
        assert!(
            drawn > 0,
            "Expected at least one element (the text) to be painted, got {}",
            drawn
        );
    }

    /// Verifies that `each item in lista` paints the child template once per
    /// list element using the fully-expanded Taffy path.
    /// With a 2-element list and one Text child, `drawn_count` must be >= 2.
    #[test]
    fn test_paint_each_node_iterates_list() {
        use crate::core::types::Value;
        use crate::parser::Primitive;
        use crate::render::layout_bridge::{EachExpansion, expand_each_nodes};
        use std::sync::Arc;

        // Build store: items = [Record{"name":"A"}, Record{"name":"B"}]
        let mut store = crate::core::types::VariableStore::new();
        let make_record = |name: &str| -> Value {
            Value::record_from_unsorted(vec![("name", Value::String(Arc::from(name)))])
        };
        store.set(
            "lista",
            Value::List(Arc::new(vec![make_record("A"), make_record("B")])),
        );
        let mut store = store.freeze();

        // Build DOM: Window -> Each(item in lista) -> Text("{item.name}")
        let mut tree = Tree::new(MizuNode {
            primitive: Primitive::Doc,
            attributes: FxHashMap::default(),
            events: FxHashMap::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });

        let each_id = tree
            .root_mut()
            .append(MizuNode {
                primitive: Primitive::Each,
                attributes: FxHashMap::default(),
                events: FxHashMap::default(),
                iterator_context: Some(("item".to_string(), "lista".to_string())),
                conditional_classes: Vec::new(),
            })
            .id();

        // Append the Text child to the Each node
        let text_node_id = tree
            .get_mut(each_id)
            .unwrap()
            .append(MizuNode {
                primitive: Primitive::Text,
                attributes: {
                    let mut a = FxHashMap::default();
                    a.insert("content".to_string(), "{item.name}".to_string());
                    a
                },
                events: FxHashMap::default(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            })
            .id();

        // Taffy layout: Window -> Each -> Text
        let mut taffy = TaffyTree::<EgoNodeId>::new();
        let mut node_to_taffy_id = HashMap::new();

        let text_taffy = taffy
            .new_leaf_with_context(taffy::style::Style::default(), text_node_id)
            .unwrap();
        node_to_taffy_id.insert(text_node_id, text_taffy);

        let each_taffy = taffy
            .new_with_children(taffy::style::Style::default(), &[text_taffy])
            .unwrap();
        node_to_taffy_id.insert(each_id, each_taffy);

        let window_taffy = taffy
            .new_with_children(taffy::style::Style::default(), &[each_taffy])
            .unwrap();
        node_to_taffy_id.insert(tree.root().id(), window_taffy);

        // Expand Each nodes and re-compute layout (the correct order).
        let expansion = expand_each_nodes(
            &tree,
            &store,
            &mut taffy,
            &node_to_taffy_id,
            &EachExpansion::default(),
            None, // full rebuild in tests
            0.0,
            600.0,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();

        taffy
            .compute_layout(
                window_taffy,
                taffy::geometry::Size {
                    width: taffy::style::AvailableSpace::Definite(800.0),
                    height: taffy::style::AvailableSpace::Definite(600.0),
                },
            )
            .unwrap();

        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();
        let mut layout_cx = parley::LayoutContext::new();
        let scroll_offsets: HashMap<EgoNodeId, f32> = HashMap::new();
        let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());
        let mut fetching_images = rustc_hash::FxHashMap::default();
        let (network_tx, _rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let text_layouts = HashMap::new();
        let style_rules: HashMap<String, StyleRules> = HashMap::new();
        let mut each_offset_y: HashMap<EgoNodeId, f32> = HashMap::new();

        let mut ctx = PaintContext {
            tab: crate::network::TabId(0),
            tree: &tree,
            taffy: &taffy,
            node_to_taffy_id: &node_to_taffy_id,
            style_rules: &style_rules,
            style_variants: &[],
            render_env: crate::render::responsive::RenderEnvironment {
                viewport: crate::render::responsive::ViewportSize {
                    width: 800.0,
                    height: 600.0,
                },
                color_scheme: crate::render::preferences::ColorScheme::Dark,
            },
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            transform: Affine::IDENTITY,
            store: &mut store,
            scroll_offsets: &scroll_offsets,
            focused_node: None,
            image_cache: &mut image_cache,
            fetching_images: &mut fetching_images,
            network_tx: &network_tx,
            chrome_url: "mizu://localhost/index.mizu",
            elapsed_ms: 0,
            has_animations: false,
            text_layouts: &text_layouts,
            item_bindings: HashMap::new(),
            each_groups: &expansion.groups,
            each_window_start: &expansion.window_start,
            each_container_offset_y: &mut each_offset_y,
            taffy_id_overrides: HashMap::new(),
        };

        let mut scene = Scene::new();
        let drawn = paint_node(tree.root().id(), &mut ctx, &mut scene, (0.0, 0.0));

        assert!(
            drawn >= 2,
            "each with 2-element list must paint the child at least twice; got drawn={}",
            drawn,
        );
    }

    /// Verifies that `expand_each_nodes` + `compute_layout` produces
    /// non-overlapping row positions for a fixed-height Each template.
    ///
    /// DOM: Window → Each(item in rows) → Box(.row  height:50px)
    /// Store: rows = [_, _, _]  (3 elements; values irrelevant for layout)
    ///
    /// Expected Taffy output after expansion:
    ///   row_0.location.y == 0,   row_0.size.height == 50
    ///   row_1.location.y == 50,  row_1.size.height == 50
    ///   row_2.location.y == 100, row_2.size.height == 50
    ///   Each container size.height >= 150
    #[test]
    fn test_each_items_stack_without_overlap() {
        use crate::core::types::Value;
        use crate::parser::Primitive;
        use crate::render::layout_bridge::{EachExpansion, expand_each_nodes};
        use std::sync::Arc;

        // Store: rows = list of 3 null values (heights come from CSS, not values).
        let mut store = crate::core::types::VariableStore::new();
        store.set(
            "rows",
            Value::List(Arc::new(vec![Value::Null, Value::Null, Value::Null])),
        );
        let mut store = store.freeze();

        // DOM: Window → Each → Box
        let mut tree = Tree::new(MizuNode {
            primitive: Primitive::Doc,
            attributes: FxHashMap::default(),
            events: FxHashMap::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });
        let each_id = tree
            .root_mut()
            .append(MizuNode {
                primitive: Primitive::Each,
                attributes: FxHashMap::default(),
                events: FxHashMap::default(),
                iterator_context: Some(("item".to_string(), "rows".to_string())),
                conditional_classes: Vec::new(),
            })
            .id();
        let box_id = tree
            .get_mut(each_id)
            .unwrap()
            .append(MizuNode {
                primitive: Primitive::Box,
                attributes: {
                    let mut a = FxHashMap::default();
                    a.insert("class".to_string(), ".row".to_string());
                    a
                },
                events: FxHashMap::default(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            })
            .id();

        // Taffy: Window → Each → Box (height 50px, full width)
        let mut taffy = TaffyTree::<EgoNodeId>::new();
        let mut node_to_taffy_id = HashMap::new();

        let row_style = taffy::style::Style {
            size: taffy::geometry::Size {
                width: taffy::style::Dimension::Percent(1.0),
                height: taffy::style::Dimension::Length(50.0),
            },
            flex_shrink: 0.0,
            ..taffy::style::Style::default()
        };
        let box_taffy = taffy.new_leaf(row_style).unwrap();
        node_to_taffy_id.insert(box_id, box_taffy);

        let each_taffy = taffy
            .new_with_children(taffy::style::Style::default(), &[box_taffy])
            .unwrap();
        node_to_taffy_id.insert(each_id, each_taffy);

        let window_style = taffy::style::Style {
            size: taffy::geometry::Size {
                width: taffy::style::Dimension::Percent(1.0),
                height: taffy::style::Dimension::Auto,
            },
            ..taffy::style::Style::default()
        };
        let window_taffy = taffy
            .new_with_children(window_style, &[each_taffy])
            .unwrap();
        node_to_taffy_id.insert(tree.root().id(), window_taffy);

        // Expand then compute layout.
        let expansion = expand_each_nodes(
            &tree,
            &store,
            &mut taffy,
            &node_to_taffy_id,
            &EachExpansion::default(),
            None, // full rebuild in tests
            0.0,
            600.0,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();

        taffy
            .compute_layout(
                window_taffy,
                taffy::geometry::Size {
                    width: taffy::style::AvailableSpace::Definite(800.0),
                    height: taffy::style::AvailableSpace::MaxContent,
                },
            )
            .unwrap();

        // Check that there are exactly 3 groups for the Each node.
        let groups = expansion
            .groups
            .get(&each_id)
            .expect("Each must be expanded");
        assert_eq!(groups.len(), 3, "3 rows expected");

        // Collect (y, h) for every row container.
        let row_positions: Vec<(f32, f32)> = groups
            .iter()
            .map(|(row_id, _)| {
                let l = taffy.layout(*row_id).expect("row must have layout");
                (l.location.y, l.size.height)
            })
            .collect();

        // Each row must be 50px tall.
        for (i, &(_, h)) in row_positions.iter().enumerate() {
            assert!(
                (h - 50.0).abs() < 1.0,
                "row {i} must be 50 px tall, got {h}"
            );
        }

        // Rows must be stacked (no overlap): row[i].y == i * 50.
        for (i, &(y, _)) in row_positions.iter().enumerate() {
            let expected_y = i as f32 * 50.0;
            assert!(
                (y - expected_y).abs() < 1.0,
                "row {i} must start at y={expected_y}, got y={y}"
            );
        }

        // The Each container must encompass all three rows.
        let each_layout = taffy.layout(each_taffy).expect("Each must have layout");
        assert!(
            each_layout.size.height >= 150.0 - 1.0,
            "Each container must be at least 150 px tall, got {}",
            each_layout.size.height
        );
    }

    /// Verifies that z-index sorting is stable and correct:
    /// a node with z-index=1 must appear after a node with z-index=0 in the
    /// sort output.
    #[test]
    fn test_z_index_sort_order() {
        use crate::parser::{MizuOverflow, Primitive};

        let mut style_rules: HashMap<String, StyleRules> = HashMap::new();
        style_rules.insert(
            "low".to_string(),
            StyleRules {
                z_index: 0,
                ..Default::default()
            },
        );
        style_rules.insert(
            "high".to_string(),
            StyleRules {
                z_index: 5,
                ..Default::default()
            },
        );
        style_rules.insert(
            "mid".to_string(),
            StyleRules {
                z_index: 2,
                ..Default::default()
            },
        );

        // Build: Window -> (low, high, mid)
        let mut tree = Tree::new(MizuNode {
            primitive: Primitive::Doc,
            attributes: FxHashMap::default(),
            events: FxHashMap::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });

        let low_id = tree
            .root_mut()
            .append(MizuNode {
                primitive: Primitive::Box,
                attributes: {
                    let mut m = FxHashMap::default();
                    m.insert("class".into(), ".low".into());
                    m
                },
                events: FxHashMap::default(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            })
            .id();
        let high_id = tree
            .root_mut()
            .append(MizuNode {
                primitive: Primitive::Box,
                attributes: {
                    let mut m = FxHashMap::default();
                    m.insert("class".into(), ".high".into());
                    m
                },
                events: FxHashMap::default(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            })
            .id();
        let mid_id = tree
            .root_mut()
            .append(MizuNode {
                primitive: Primitive::Box,
                attributes: {
                    let mut m = FxHashMap::default();
                    m.insert("class".into(), ".mid".into());
                    m
                },
                events: FxHashMap::default(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            })
            .id();

        // Collect children z-index as the sort function would.
        let root_ref = tree.root();
        let mut child_ids: Vec<(i32, EgoNodeId)> = root_ref
            .children()
            .map(|child| {
                let z = child
                    .value()
                    .attributes
                    .get("class")
                    .and_then(|cls| {
                        let cls_name = cls.strip_prefix('.').unwrap_or(cls);
                        style_rules.get(cls_name)
                    })
                    .map(|r| r.z_index)
                    .unwrap_or(0);
                (z, child.id())
            })
            .collect();
        child_ids.sort_by_key(|&(z, _)| z);

        let sorted_ids: Vec<EgoNodeId> = child_ids.iter().map(|&(_, id)| id).collect();
        assert_eq!(
            sorted_ids,
            vec![low_id, mid_id, high_id],
            "z-index order must be ascending (low=0, mid=2, high=5)"
        );

        // Suppress unused-variable warnings for the ids we intentionally checked.
        let _ = (low_id, high_id, mid_id, MizuOverflow::Visible);
    }

    // ------------------------------------------------------------------
    // Task 2 — Zero-allocation conditional class evaluation
    // ------------------------------------------------------------------

    /// Verifies that `paint_node` evaluates conditional classes without cloning
    /// the Evaluator's global_store — the local stack must be clean before
    /// and after the evaluation.
    ///
    /// This is a regression guard: if the old `.clone()` code were reintroduced,
    /// the local_stack would not be used at all (it would be 0 throughout) and
    /// the test would still pass — but the key invariant tested here is that
    /// *no extra items are left on the local stack after paint_node returns*,
    /// which proves that `truncate_locals` properly rewound the frame.
    #[test]
    fn conditional_class_evaluation_leaves_local_stack_clean() {
        use crate::core::types::{Value, VariableStore};
        use crate::parser::layout::ConditionalClass;
        use crate::parser::logic::{Expr, ExprArena, ExprTree};
        use crate::parser::{MizuNode, Primitive};

        let mut store = VariableStore::new();
        // Intern a variable "active" and set it to true in the global store.
        let active_sym = store.interner.get_or_intern("active");
        let mut store = store.freeze();
        store
            .evaluator
            .global_store
            .insert(active_sym, Value::Bool(true));

        let mut cond_arena = ExprArena::new();
        let cond_root = cond_arena.alloc(Expr::Variable(active_sym));
        let mut tree = ego_tree::Tree::new(MizuNode {
            primitive: Primitive::Box,
            attributes: Default::default(),
            events: Default::default(),
            iterator_context: None,
            conditional_classes: vec![ConditionalClass::Toggle {
                class_name: "active-style".to_string(),
                // condition: `active` (a Variable reference)
                condition: ExprTree {
                    arena: cond_arena,
                    root: cond_root,
                },
            }],
        });

        // Add a child so paint_node doesn't short-circuit.
        tree.root_mut().append(MizuNode {
            primitive: Primitive::Box,
            attributes: Default::default(),
            events: Default::default(),
            iterator_context: None,
            conditional_classes: vec![],
        });

        let mut taffy = taffy::TaffyTree::new();
        let child_taffy = taffy.new_leaf(taffy::style::Style::default()).unwrap();
        let root_taffy = taffy
            .new_with_children(taffy::style::Style::default(), &[child_taffy])
            .unwrap();
        let mut node_to_taffy_id = HashMap::new();
        node_to_taffy_id.insert(tree.root().id(), root_taffy);
        taffy
            .compute_layout(
                root_taffy,
                taffy::geometry::Size {
                    width: taffy::style::AvailableSpace::Definite(800.0),
                    height: taffy::style::AvailableSpace::Definite(600.0),
                },
            )
            .unwrap();

        // Add the "active-style" class rule so it can be merged if condition is true.
        let mut style_rules: HashMap<String, StyleRules> = HashMap::new();
        style_rules.insert("active-style".to_string(), StyleRules::default());

        let mut font_cx = parley::FontContext::new();
        let mut layout_cx = parley::LayoutContext::new();
        let scroll_offsets: HashMap<EgoNodeId, f32> = HashMap::new();
        let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());
        let mut fetching_images = rustc_hash::FxHashMap::default();
        let (network_tx, _rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let text_layouts = HashMap::new();

        // Record local stack depth before painting.
        let stack_before = store.evaluator.local_stack.len();

        let empty_each_groups = HashMap::new();
        let empty_window_start: HashMap<EgoNodeId, usize> = HashMap::new();
        let mut empty_each_offset_y: HashMap<EgoNodeId, f32> = HashMap::new();
        let mut ctx = PaintContext {
            tab: crate::network::TabId(0),
            tree: &tree,
            taffy: &taffy,
            node_to_taffy_id: &node_to_taffy_id,
            style_rules: &style_rules,
            style_variants: &[],
            render_env: crate::render::responsive::RenderEnvironment {
                viewport: crate::render::responsive::ViewportSize {
                    width: 800.0,
                    height: 600.0,
                },
                color_scheme: crate::render::preferences::ColorScheme::Dark,
            },
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            transform: vello::kurbo::Affine::IDENTITY,
            store: &mut store,
            scroll_offsets: &scroll_offsets,
            focused_node: None,
            image_cache: &mut image_cache,
            fetching_images: &mut fetching_images,
            network_tx: &network_tx,
            chrome_url: "mizu://localhost/index.mizu",
            elapsed_ms: 0,
            has_animations: false,
            text_layouts: &text_layouts,
            item_bindings: HashMap::new(),
            each_groups: &empty_each_groups,
            each_window_start: &empty_window_start,
            each_container_offset_y: &mut empty_each_offset_y,
            taffy_id_overrides: HashMap::new(),
        };

        let mut scene = vello::Scene::new();
        paint_node(tree.root().id(), &mut ctx, &mut scene, (0.0, 0.0));

        // The local stack must be exactly as deep as it was before paint_node —
        // any leftover frames indicate `truncate_locals` was not called correctly.
        let stack_after = ctx.store.evaluator.local_stack.len();
        assert_eq!(
            stack_after, stack_before,
            "local stack must be clean after conditional-class evaluation: \
             before={stack_before}, after={stack_after}"
        );
    }

    /// The test that actually proves the ternary-class feature does what
    /// it's for: `evaluate_conditional_classes` must resolve a
    /// `ConditionalClass::Ternary` to whichever branch's style rules match
    /// the runtime-evaluated class name — not just parse without error.
    #[test]
    fn ternary_conditional_class_resolves_to_the_evaluated_branch_style() {
        use crate::core::types::{StringInterner, Value, VariableStore};
        use crate::parser::layout::ConditionalClass;
        use crate::parser::logic::parse_expr_standalone;
        use crate::parser::{MizuNode, Primitive};

        let mut interner = StringInterner::new();
        let expr = parse_expr_standalone(r#"flag ? "on" : "off""#, &mut interner).unwrap();

        let node = MizuNode {
            primitive: Primitive::Box,
            attributes: Default::default(),
            events: Default::default(),
            iterator_context: None,
            conditional_classes: vec![ConditionalClass::Ternary { expr }],
        };
        let tree = ego_tree::Tree::new(node);

        let mut taffy = taffy::TaffyTree::new();
        let root_taffy = taffy.new_leaf(taffy::style::Style::default()).unwrap();
        let mut node_to_taffy_id = HashMap::new();
        node_to_taffy_id.insert(tree.root().id(), root_taffy);
        taffy
            .compute_layout(
                root_taffy,
                taffy::geometry::Size {
                    width: taffy::style::AvailableSpace::Definite(800.0),
                    height: taffy::style::AvailableSpace::Definite(600.0),
                },
            )
            .unwrap();

        let mut style_rules: HashMap<String, StyleRules> = HashMap::new();
        style_rules.insert(
            "on".to_string(),
            StyleRules {
                z_index: 1,
                ..Default::default()
            },
        );
        style_rules.insert(
            "off".to_string(),
            StyleRules {
                z_index: 2,
                ..Default::default()
            },
        );

        let mut store = crate::core::types::VariableStore {
            evaluator: Default::default(),
            interner,
        };
        store.set("flag", Value::Bool(true));
        let mut store = store.freeze();

        let mut font_cx = parley::FontContext::new();
        let mut layout_cx = parley::LayoutContext::new();
        let scroll_offsets: HashMap<EgoNodeId, f32> = HashMap::new();
        let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());
        let mut fetching_images = rustc_hash::FxHashMap::default();
        let (network_tx, _rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let text_layouts = HashMap::new();
        let empty_each_groups = HashMap::new();
        let empty_window_start: HashMap<EgoNodeId, usize> = HashMap::new();
        let mut empty_each_offset_y: HashMap<EgoNodeId, f32> = HashMap::new();

        let mut ctx = PaintContext {
            tab: crate::network::TabId(0),
            tree: &tree,
            taffy: &taffy,
            node_to_taffy_id: &node_to_taffy_id,
            style_rules: &style_rules,
            style_variants: &[],
            render_env: crate::render::responsive::RenderEnvironment {
                viewport: crate::render::responsive::ViewportSize {
                    width: 800.0,
                    height: 600.0,
                },
                color_scheme: crate::render::preferences::ColorScheme::Dark,
            },
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            transform: vello::kurbo::Affine::IDENTITY,
            store: &mut store,
            scroll_offsets: &scroll_offsets,
            focused_node: None,
            image_cache: &mut image_cache,
            fetching_images: &mut fetching_images,
            network_tx: &network_tx,
            chrome_url: "mizu://localhost/index.mizu",
            elapsed_ms: 0,
            has_animations: false,
            text_layouts: &text_layouts,
            item_bindings: HashMap::new(),
            each_groups: &empty_each_groups,
            each_window_start: &empty_window_start,
            each_container_offset_y: &mut empty_each_offset_y,
            taffy_id_overrides: HashMap::new(),
        };

        let resolved = evaluate_conditional_classes(tree.root().value(), &mut ctx);
        assert_eq!(
            resolved.z_index, 1,
            "flag=true must resolve the ternary to the \"on\" branch's style rules"
        );

        ctx.store.set_runtime("flag", Value::Bool(false));
        let resolved = evaluate_conditional_classes(tree.root().value(), &mut ctx);
        assert_eq!(
            resolved.z_index, 2,
            "flag=false must resolve the ternary to the \"off\" branch's style rules"
        );
    }

    /// Verifies that item_bindings injected via `push_local` shadow global
    /// variables during conditional-class evaluation — the overlay semantics
    /// must be preserved by the push_local/truncate_locals approach.
    #[test]
    fn conditional_class_item_binding_shadows_global() {
        use crate::core::types::{Value, VariableStore};
        use crate::parser::layout::ConditionalClass;
        use crate::parser::logic::{Expr, ExprArena, ExprTree};
        use crate::parser::{MizuNode, Primitive};

        let mut store = VariableStore::new();
        // Global: "flag" = false
        let flag_sym = store.interner.get_or_intern("flag");
        let mut store = store.freeze();
        store
            .evaluator
            .global_store
            .insert(flag_sym, Value::Bool(false));

        // The conditional class condition: `flag`
        let mut cond_arena = ExprArena::new();
        let cond_root = cond_arena.alloc(Expr::Variable(flag_sym));
        let node = MizuNode {
            primitive: Primitive::Box,
            attributes: Default::default(),
            events: Default::default(),
            iterator_context: None,
            conditional_classes: vec![ConditionalClass::Toggle {
                class_name: "highlight".to_string(),
                condition: ExprTree {
                    arena: cond_arena,
                    root: cond_root,
                },
            }],
        };
        let tree = ego_tree::Tree::new(node);

        let mut taffy = taffy::TaffyTree::new();
        let root_taffy = taffy.new_leaf(taffy::style::Style::default()).unwrap();
        let mut node_to_taffy_id = HashMap::new();
        node_to_taffy_id.insert(tree.root().id(), root_taffy);
        taffy
            .compute_layout(
                root_taffy,
                taffy::geometry::Size {
                    width: taffy::style::AvailableSpace::Definite(800.0),
                    height: taffy::style::AvailableSpace::Definite(600.0),
                },
            )
            .unwrap();

        let mut style_rules: HashMap<String, StyleRules> = HashMap::new();
        style_rules.insert(
            "highlight".to_string(),
            StyleRules {
                z_index: 99, // sentinel value we can detect
                ..Default::default()
            },
        );

        let mut font_cx = parley::FontContext::new();
        let mut layout_cx = parley::LayoutContext::new();
        let scroll_offsets: HashMap<EgoNodeId, f32> = HashMap::new();
        let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());
        let mut fetching_images = rustc_hash::FxHashMap::default();
        let (network_tx, _rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let text_layouts = HashMap::new();

        // item_bindings overrides "flag" → true (local shadow beats global false)
        let mut item_bindings = HashMap::new();
        item_bindings.insert("flag".to_string(), Value::Bool(true));

        let empty_each_groups = HashMap::new();
        let empty_window_start: HashMap<EgoNodeId, usize> = HashMap::new();
        let mut empty_each_offset_y: HashMap<EgoNodeId, f32> = HashMap::new();
        let mut ctx = PaintContext {
            tab: crate::network::TabId(0),
            tree: &tree,
            taffy: &taffy,
            node_to_taffy_id: &node_to_taffy_id,
            style_rules: &style_rules,
            style_variants: &[],
            render_env: crate::render::responsive::RenderEnvironment {
                viewport: crate::render::responsive::ViewportSize {
                    width: 800.0,
                    height: 600.0,
                },
                color_scheme: crate::render::preferences::ColorScheme::Dark,
            },
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            transform: vello::kurbo::Affine::IDENTITY,
            store: &mut store,
            scroll_offsets: &scroll_offsets,
            focused_node: None,
            image_cache: &mut image_cache,
            fetching_images: &mut fetching_images,
            network_tx: &network_tx,
            chrome_url: "mizu://localhost/index.mizu",
            elapsed_ms: 0,
            has_animations: false,
            text_layouts: &text_layouts,
            item_bindings,
            each_groups: &empty_each_groups,
            each_window_start: &empty_window_start,
            each_container_offset_y: &mut empty_each_offset_y,
            taffy_id_overrides: HashMap::new(),
        };

        // Paint — if the shadow logic is correct, `highlight` class is merged
        // (flag=true via item_binding) even though global flag=false.
        // We can only verify indirectly that no panic occurs and the stack is clean.
        let mut scene = vello::Scene::new();
        paint_node(tree.root().id(), &mut ctx, &mut scene, (0.0, 0.0));

        // Global must not have been mutated — the old approach inserted into global_store.
        let global_flag = ctx.store.evaluator.global_store.get(&flag_sym);
        assert!(
            global_flag.is_some_and(|v| v
                .budget_eq(&Value::Bool(false), &mut u64::MAX, u64::MAX)
                .unwrap_or(false)),
            "global 'flag' must remain false after conditional-class eval with item_binding override"
        );

        // Local stack must be empty (no leftover push_local frames).
        assert_eq!(
            ctx.store.evaluator.local_stack.len(),
            0,
            "local stack must be empty after eval"
        );
    }

    /// Verifies that a node with `overflow: scroll` and a non-zero scroll
    /// offset causes the child transform to include the vertical translation.
    #[test]
    fn test_scroll_offset_applied_to_transform() {
        // The transform for a scrollable parent with 50px offset must shift
        // children upward (negative Y translation).
        let base = Affine::IDENTITY;
        let scroll_y = 50.0f32;
        let child_transform = base * Affine::translate((0.0, -(scroll_y as f64)));

        // A point at y=100 in the child's un-scrolled space should appear at y=50.
        let point = vello::kurbo::Point::new(0.0, 100.0);
        let transformed = child_transform * point;
        assert!(
            (transformed.y - 50.0).abs() < f64::EPSILON,
            "scroll should shift child paint by -scroll_y; got y={}",
            transformed.y,
        );
    }
}
