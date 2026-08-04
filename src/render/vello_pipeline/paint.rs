//! `paint_node` (the recursive per-node painter, with z-index sort, overflow
//! clipping, and scroll translation) and `paint_each` (the `each` iterator's
//! per-item repaint path).

use ego_tree::NodeId as EgoNodeId;
use parley::style::StyleProperty;
use vello::{
    Scene,
    kurbo::{Affine, Rect, Stroke},
    peniko::{BlendMode, Color, Fill, Mix},
};

use crate::core::types::Value;
use crate::parser::{MizuOverflow, Primitive, StyleRules};
use crate::render::layout_bridge::EachGroupEntries;

use super::helpers::*;

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
            // Embedded-only (see `render::embedded_fonts`): `SansSerif`
            // always resolves to IBM Plex Sans, and per-script fallback
            // (Han/Hangul/Arabic/...) is registered collection-wide there
            // too, independent of which family was explicitly requested —
            // so a named-font chain probing for OS-installed Latin/CJK
            // faces (Segoe UI/Meiryo/Yu Gothic/Hiragino Sans) would never
            // match anything and is unnecessary either way.
            let font_family = parley::style::FontFamily::Single(
                parley::style::FontFamilyName::Generic(parley::style::GenericFamily::SansSerif),
            );
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
