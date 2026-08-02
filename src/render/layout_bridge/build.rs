//! `TaffyBuildContext` and `build_taffy_tree`: the recursive DOM→Taffy tree
//! builder, resolving styles/variants and threading `Each`-node expansion
//! metadata as it walks.

use std::collections::HashMap;

use ego_tree::{NodeId as EgoNodeId, NodeRef};
use taffy::{TaffyTree, geometry::Size};

use crate::core::errors::MizuError;
use crate::parser::style::StyleVariant;
use crate::parser::{MizuNode, Primitive, StyleRules};
use crate::render::bidi::resolve_direction;
use crate::render::image_codec::AssetSlot;
use crate::render::responsive::{RenderEnvironment, resolve_matching_variants};

use super::style::translate_style;

/// per node; everything else is read-only lookup state for the whole build
/// pass. Mirrors the `PaintContext` pattern already used for the analogous
/// recursive paint walk (`render::vello_pipeline`).
pub struct TaffyBuildContext<'a> {
    /// Active tag/class style rules.
    pub style_rules_map: &'a HashMap<String, StyleRules>,
    /// The Taffy tree being built.
    pub taffy: &'a mut TaffyTree<EgoNodeId>,
    /// Mapping of DOM node IDs to the Taffy node IDs created for them.
    pub node_to_taffy_id: &'a mut HashMap<EgoNodeId, taffy::prelude::NodeId>,
    /// Cache of decoded images, consulted for intrinsic aspect ratio.
    ///
    /// `&mut` because `LruCache::get` promotes the entry to
    /// most-recently-used, which requires mutable access.
    pub image_cache: &'a mut lru::LruCache<String, AssetSlot>,
    /// The current document's base URL, used to resolve relative `image src`.
    pub chrome_url: &'a str,
    /// ux-6 breakpoint/color-scheme style variants. Pass `&[]` for callers
    /// that don't need responsive behavior (e.g. tests).
    pub variants: &'a [StyleVariant],
    /// Current viewport size / color-scheme snapshot variants resolve against.
    pub env: &'a RenderEnvironment,
}

/// Recursively traverses the DOM tree bottom-up to build the Taffy tree layout.
pub fn build_taffy_tree(
    node: NodeRef<MizuNode>,
    ctx: &mut TaffyBuildContext<'_>,
) -> Result<taffy::prelude::NodeId, MizuError> {
    let mut children_ids = Vec::new();
    for child in node.children() {
        let child_id = build_taffy_tree(child, ctx)?;
        children_ids.push(child_id);
    }

    let mizu_node = node.value();
    let mut merged_rules = StyleRules::default();

    // 1. Tag styles
    let tag_name = mizu_node.style_tag_name();
    if let Some(tag_rules) = ctx.style_rules_map.get(tag_name.as_ref()) {
        // merge_from(&ref) clones only winning fields, not the whole struct.
        merged_rules.merge_from(tag_rules);
    }

    // 2. Class styles
    let class_attr = mizu_node.attributes.get("class").map(String::as_str);
    if let Some(class_attr) = class_attr
        && let Some(class_rules) = ctx.style_rules_map.get(class_attr)
    {
        merged_rules.merge_from(class_rules);
    }

    // 3. Id styles — highest specificity, applied after tag and class (an
    // id selector is stored `#`-prefixed in the same rules map, so it can
    // never collide with a same-named class or tag). Build the key on a
    // pre-sized String to avoid the alloc overhead of format!("#{id}").
    let id_key: Option<String> = mizu_node.attributes.get("id").map(|id| {
        let mut k = String::with_capacity(id.len() + 1);
        k.push('#');
        k.push_str(id);
        k
    });
    if let Some(ref id_key) = id_key
        && let Some(id_rules) = ctx.style_rules_map.get(id_key.as_str())
    {
        merged_rules.merge_from(id_rules);
    }

    // 4. Breakpoint / color-scheme variants (ux-6) — applied last, after all
    // three bases, in source declaration order (see docs/design/responsive.md).
    let mut selectors: Vec<&str> = vec![tag_name.as_ref()];
    if let Some(c) = class_attr {
        selectors.push(c);
    }
    if let Some(ref k) = id_key {
        selectors.push(k.as_str());
    }
    merged_rules.merge_from(&resolve_matching_variants(
        ctx.variants,
        &selectors,
        ctx.env,
    ));

    // ux-7: resolved once per node via `dir` attribute inheritance (an
    // O(depth) ancestor walk — see `render::bidi`'s doc for the cost class).
    let dir = resolve_direction(node);
    let mut style = translate_style(&merged_rules, ctx.env.viewport, dir);

    if mizu_node.primitive == Primitive::Doc {
        style.size = Size {
            width: taffy::style::Dimension::Percent(1.0),
            height: taffy::style::Dimension::Percent(1.0),
        };
    } else if mizu_node.primitive == Primitive::Button {
        style.flex_shrink = 0.0;
        style.overflow = taffy::geometry::Point {
            x: taffy::style::Overflow::Hidden,
            y: taffy::style::Overflow::Hidden,
        };
    } else if mizu_node.primitive == Primitive::Box {
        style.flex_shrink = 1.0;
        style.overflow = taffy::geometry::Point {
            x: taffy::style::Overflow::Hidden,
            y: taffy::style::Overflow::Hidden,
        };
    } else if mizu_node.primitive == Primitive::Image
        && let Some(src) = mizu_node.attributes.get("src")
    {
        let abs_url = if src.starts_with("mizu://") {
            src.clone()
        } else if let Ok(base_uri) = crate::network::uri::MizuUri::parse(ctx.chrome_url) {
            let path = if src.starts_with('/') {
                src.clone()
            } else {
                format!("/{}", src)
            };
            format!("mizu://{}{}", base_uri.domain, path)
        } else {
            src.clone()
        };

        let mut intr_width = None;
        let mut intr_height = None;

        if let Some(AssetSlot::Ready(cached)) = ctx.image_cache.get(&abs_url) {
            intr_width = Some(cached.width() as f32);
            intr_height = Some(cached.height() as f32);
        }

        if let (Some(w), Some(h)) = (intr_width, intr_height) {
            style.aspect_ratio = Some(w / h);
            // Only apply intrinsic pixel dimensions when *neither* axis has
            // been set by the stylesheet.  If the user specified one axis
            // (e.g. `width 400`) the aspect_ratio alone is sufficient for
            // Taffy to derive the other — overwriting it here would break
            // proportional scaling.
            if style.size.width == taffy::style::Dimension::Auto
                && style.size.height == taffy::style::Dimension::Auto
            {
                style.size.width = taffy::style::Dimension::Length(w);
                style.size.height = taffy::style::Dimension::Length(h);
            }
        }
    }

    let taffy_id = if children_ids.is_empty() {
        ctx.taffy
            .new_leaf_with_context(style, node.id())
            .map_err(|e| MizuError::ParseError(format!("Failed to create Taffy node: {e}")))?
    } else {
        ctx.taffy
            .new_with_children(style, &children_ids)
            .map_err(|e| MizuError::ParseError(format!("Failed to create Taffy node: {e}")))?
    };

    ctx.node_to_taffy_id.insert(node.id(), taffy_id);
    Ok(taffy_id)
}
