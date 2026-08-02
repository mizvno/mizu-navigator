//! The redraw path: builds one frame's Vello scene (document, chrome, inspector, history sidebar) and presents it.

use vello::{
    AaConfig, Renderer, Scene,
    kurbo::Affine,
    util::{RenderContext, RenderSurface},
};
use winit::window::Window;

use crate::render::chrome_vello::{CHROME_HEIGHT, paint_chrome};
use crate::render::preferences::ChromePalette;
use crate::render::vello_pipeline::{PaintContext, paint_node};

use crate::render::accessibility::build_a11y_tree;

use super::super::manager::MizuWindowManager;
use super::mouse::tab_strip_entries;

/// Handles `WindowEvent::RedrawRequested`: paints the DOM, chrome bar, and
/// inspector panel into a fresh `Scene`, then presents it to the surface.
/// Left as a single function — like `paint_node`'s own per-primitive paint
/// steps, this is one paint pass with a fixed, legitimately linear layer
/// order (DOM content, then chrome, then inspector), not several tangled
/// concerns.
pub(super) fn dispatch_redraw_requested(
    manager: &mut MizuWindowManager,
    render_cx: &mut RenderContext,
    surface: &mut RenderSurface<'_>,
    renderer: &mut Renderer,
    a11y_adapter: &mut accesskit_winit::Adapter,
    window: &Window,
) {
    // Built before the split borrow: the strip is a view over *all* tabs,
    // while everything below paints exactly one.
    let strip = tab_strip_entries(manager);
    // Snapshot window-level sidebar state before the split borrow so we can
    // read these values during paint without conflicting with `ctx.history_log`.
    let sidebar_open = manager.history_sidebar.open;
    let sidebar_scroll = manager.history_sidebar.scroll_offset;
    let sidebar_hovered = manager.history_sidebar.hovered;
    let (tab, mut ctx) = manager.split_active();
    let physical_size = window.inner_size();
    let scale = window.scale_factor();
    let width = physical_size.width;
    let height = physical_size.height;
    if width == 0 || height == 0 {
        return;
    }

    // Piggyback on the same frame coalescing the renderer
    // already has — one accessibility tree rebuild per
    // actual redraw, not per state change, so a per-frame
    // timer document can't spam the AT.
    a11y_adapter.update_if_active(|| {
        build_a11y_tree(
            tab.a11y_epoch,
            &tab.dom,
            &tab.node_id_to_u32,
            tab.focused_node,
            &tab.store,
        )
    });

    let device = &render_cx.devices[surface.dev_id].device;
    let queue = &render_cx.devices[surface.dev_id].queue;

    // Resolve background color from the root `doc` style rule
    let mut bg_color = vello::peniko::Color::rgba8(255, 255, 255, 255);
    if let Some(rules) = tab.style_rules.get("doc")
        && let Some(crate::parser::style::MizuBackground::Solid(c)) = &rules.background
    {
        bg_color = vello::peniko::Color::rgba8(c.r, c.g, c.b, c.a);
    }

    let elapsed_ms = ctx.start_time.elapsed().as_millis() as u64;
    let logical_width = width as f32 / scale as f32;

    let mut scene = Scene::new();

    // ── Layer 1: DOM content, clipped below the chrome bar ────
    let chrome_phys = CHROME_HEIGHT as f64 * scale;
    let content_clip = vello::kurbo::Rect::new(0.0, chrome_phys, width as f64, height as f64);
    scene.push_layer(
        vello::peniko::BlendMode::new(vello::peniko::Mix::Normal, vello::peniko::Compose::SrcOver),
        1.0,
        Affine::IDENTITY,
        &content_clip,
    );

    let dom_transform = Affine::scale(scale)
        * Affine::translate((0.0, (CHROME_HEIGHT - tab.root_scroll_offset_y) as f64));

    let has_animations;
    {
        // Subresource URLs (images, media) resolve against the origin of the
        // document being painted, never the URL-bar buffer.
        let chrome_url_snapshot = tab.chrome_state.committed_url.clone();
        let mut ctx = PaintContext {
            tab: tab.id,
            tree: &tab.dom,
            taffy: &tab.taffy,
            node_to_taffy_id: &tab.node_to_taffy_id,
            style_rules: &tab.style_rules,
            style_variants: &tab.style_variants,
            render_env: crate::render::responsive::RenderEnvironment {
                viewport: tab.viewport_size,
                color_scheme: ctx.preferences.color_scheme,
            },
            font_cx: ctx.font_cx,
            layout_cx: ctx.layout_cx,
            transform: dom_transform,
            store: &mut tab.store,
            scroll_offsets: &tab.scroll_offsets,
            focused_node: tab.focused_node,
            image_cache: ctx.image_cache,
            fetching_images: ctx.fetching_images,
            elapsed_ms,
            network_tx: ctx.network_tx,
            chrome_url: &chrome_url_snapshot,
            has_animations: false,
            text_layouts: &tab.text_layouts,
            item_bindings: std::collections::HashMap::new(),
            each_groups: &tab.each_expansion.groups,
            each_window_start: &tab.each_expansion.window_start,
            each_container_offset_y: &mut tab.each_container_offset_y,
            taffy_id_overrides: std::collections::HashMap::new(),
        };
        paint_node(tab.dom.root().id(), &mut ctx, &mut scene, (0.0, 0.0));
        has_animations = ctx.has_animations;
    } // font_cx / layout_cx borrows released here

    scene.pop_layer();

    // ── Layer 2: Chrome bar (always on top) ──────────────────
    {
        let cs = &tab.chrome_state;
        let can_go_back = tab.history.can_go_back();
        let can_go_forward = tab.history.can_go_forward();
        let palette = ChromePalette::for_preferences(ctx.preferences);
        let fc = &mut ctx.font_cx;
        let lc = &mut ctx.layout_cx;
        paint_chrome(
            &mut scene,
            &mut crate::render::chrome_vello::ChromePaintContext {
                state: cs,
                window_width: logical_width,
                transform: Affine::scale(scale),
                elapsed_ms,
                font_cx: fc,
                layout_cx: lc,
                can_go_back,
                can_go_forward,
                palette: &palette,
                tabs: &strip,
                history_sidebar_open: sidebar_open,
            },
        );
    }

    // ── Layer 3: Inspector panel + selection highlight ───────
    if tab.inspector.open {
        let logical_height = height as f32 / scale as f32;
        // While picking, highlight the node under the cursor;
        // otherwise the committed selection.
        let highlight_target = if tab.inspector.picker {
            tab.inspector.picker_hover
        } else {
            tab.inspector.selected
        };
        if let Some(sel) = highlight_target
            && let Some(rect) = crate::render::inspector::node_screen_rect(
                &tab.dom,
                &tab.taffy,
                &tab.node_to_taffy_id,
                &tab.scroll_offsets,
                tab.root_scroll_offset_y,
                CHROME_HEIGHT,
                sel,
            )
        {
            crate::render::inspector::paint::paint_node_highlight(
                &mut scene,
                rect,
                scale as f32,
                &ChromePalette::for_preferences(ctx.preferences),
            );
        }
        let rows = {
            let src = tab.inspector_sources();
            crate::render::inspector::model::build_rows(&src, &tab.inspector)
        };
        crate::render::inspector::paint::paint_panel(
            &mut scene,
            &mut crate::render::inspector::paint::PanelPaintContext {
                state: &mut tab.inspector,
                rows: &rows,
                window_width: logical_width,
                window_height: logical_height,
                scale: scale as f32,
                font_cx: ctx.font_cx,
                layout_cx: ctx.layout_cx,
                palette: &ChromePalette::for_preferences(ctx.preferences),
            },
        );
    }

    // ── Layer 4: History sidebar ─────────────────────────────
    // Painted last so it overlays both the page and the inspector.
    if sidebar_open {
        let palette = ChromePalette::for_preferences(ctx.preferences);
        crate::render::history_sidebar::paint_history_sidebar(
            &mut scene,
            &mut crate::render::history_sidebar::SidebarPaintContext {
                log: ctx.history_log,
                scroll_offset: sidebar_scroll,
                hovered: sidebar_hovered,
                palette: &palette,
                font_cx: ctx.font_cx,
                layout_cx: ctx.layout_cx,
                transform: Affine::scale(scale),
                window_height: height as f32 / scale as f32,
                chrome_height: CHROME_HEIGHT,
            },
        );
    }

    if has_animations || tab.chrome_state.loading {
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

    if let Err(e) =
        renderer.render_to_surface(device, queue, &scene, &surface_texture, &render_params)
    {
        tracing::error!("render_to_surface failed: {e}");
        return;
    }

    surface_texture.present();

    // Expose scroll state to the logic store
    tab.store.set_runtime(
        "root_scroll_y",
        crate::core::types::Value::Decimal(
            (tab.root_scroll_offset_y as f64 * crate::core::types::DECIMAL_SCALE as f64).round()
                as i64,
        ),
    );
}
