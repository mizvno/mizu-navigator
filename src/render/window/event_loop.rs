//! `run_window_loop`, the Winit event loop.

use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::sync::Arc;

use ego_tree::Tree;
use vello::{AaConfig, Renderer, RendererOptions, Scene, kurbo::Affine, util::RenderContext};
use winit::{
    event::{Event, MouseScrollDelta, WindowEvent},
    keyboard::NamedKey,
    window::WindowBuilder,
};

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol, VariableStore};
use crate::network::UiEvent;
use crate::parser::logic::{ComputedBinding, MizuFunction, RootTimer};
use crate::parser::{MizuNode, MizuOverflow, Primitive, StyleRules};
use crate::render::chrome_vello::{
    CHROME_HEIGHT, ChromeHitZone, ChromeKeyAction, chrome_hit_zone, paint_chrome, url_text_left,
};
use crate::render::hit_test::hit_test;
use crate::render::navigation::NavigationInitiator;
use crate::render::vello_pipeline::{PaintContext, paint_node};

use super::focus::find_click_and_submit;
use super::input::{
    apply_clipboard_action, dispatch_click_gesture, dispatch_form_submit, find_form_submitter,
    push_input_text,
};
use super::manager::MizuWindowManager;
use super::navigate::{navigate_to_url, process_network_result};

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
    root_timers: Vec<RootTimer>,
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
    manager.root_timers = root_timers;

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
        let reverse_index = crate::parser::logic::build_comp_reverse_index(&computed);
        crate::parser::logic::recompute_computed_bindings(
            &mut manager.store,
            &computed,
            &fns,
            &all_syms,
            &reverse_index,
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
                    // ── Picker hover: live-highlight the node under the cursor ─
                    if manager.inspector.open && manager.inspector.picker {
                        window.set_cursor_icon(winit::window::CursorIcon::Crosshair);
                        let logical_width =
                            window.inner_size().width as f32 / scale_factor as f32;
                        let over_page = last_mouse_logical_x
                            < crate::render::inspector::panel_left(logical_width);
                        let hover = if over_page { hit_node_id } else { None };
                        if manager.inspector.picker_hover != hover {
                            manager.inspector.picker_hover = hover;
                            window.request_redraw();
                        }
                        return;
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
                                navigate_to_url(
                                    &mut manager,
                                    url,
                                    NavigationInitiator::UserGesture,
                                );
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
                                if let Some(prev) = manager.focused_node.take() {
                                    // Re-render the blurred input (placeholder returns).
                                    manager.mark_text_dirty(prev);
                                }
                            }
                            ChromeHitZone::Background => {
                                // Chrome background — just blur URL bar and DOM focus
                                if manager.chrome_state.focused {
                                    manager.chrome_state.focused = false;
                                }
                                if let Some(prev) = manager.focused_node.take() {
                                    // Re-render the blurred input (placeholder returns).
                                    manager.mark_text_dirty(prev);
                                }
                            }
                        }
                        window.request_redraw();
                        return;
                    }

                    // ── Click inside the inspector panel ─────────────────────
                    if manager.inspector.open
                        && last_mouse_logical_x
                            >= crate::render::inspector::panel_left(logical_width)
                    {
                        let rows = {
                            let src = manager.inspector_sources();
                            crate::render::inspector::model::build_rows(&src, &manager.inspector)
                        };
                        let x = last_mouse_logical_x
                            - crate::render::inspector::panel_left(logical_width);
                        let y = last_mouse_logical_y - CHROME_HEIGHT;
                        if crate::render::inspector::handle_panel_click(
                            &mut manager.inspector,
                            &rows,
                            x,
                            y,
                        ) {
                            window.request_redraw();
                        }
                        return;
                    }

                    // ── Picker mode: the click selects instead of interacting ─
                    if manager.inspector.open && manager.inspector.picker {
                        let hit = hit_test(
                            &manager.dom,
                            &manager.taffy,
                            &manager.node_to_taffy_id,
                            &manager.scroll_offsets,
                            last_mouse_logical_x,
                            last_mouse_logical_y - CHROME_HEIGHT + manager.root_scroll_offset_y,
                        );
                        if let Some(hit_id) = hit {
                            manager
                                .inspector
                                .select_with_ancestors(&manager.dom, hit_id);
                            // Bring the selection into view in the Elements tree.
                            let rows = {
                                let src = manager.inspector_sources();
                                crate::render::inspector::model::build_rows(
                                    &src,
                                    &manager.inspector,
                                )
                            };
                            if let Some(idx) = rows.iter().position(|r| r.node == Some(hit_id)) {
                                let logical_height = window.inner_size().height as f32
                                    / window.scale_factor() as f32;
                                let viewport_h = (logical_height
                                    - CHROME_HEIGHT
                                    - crate::render::inspector::TAB_BAR_HEIGHT)
                                    .max(0.0);
                                manager.inspector.scroll_to_row(idx, viewport_h);
                            }
                        }
                        manager.inspector.set_picker(false);
                        window.set_cursor_icon(winit::window::CursorIcon::Default);
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
                    let mut submit_node_id = None;
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
                            if node_ref.value().events.contains_key("submit") {
                                submit_node_id = Some(id);
                            }
                            if action_node_id.is_some() || submit_node_id.is_some() {
                                break;
                            }
                            current_hit = node_ref.parent().map(|p| p.id());
                        } else {
                            break;
                        }
                    }

                    if manager.focused_node != new_focus {
                        // Re-render both inputs: the old one regains its
                        // placeholder, the new one shows the caret.
                        if let Some(prev) = manager.focused_node {
                            manager.mark_text_dirty(prev);
                        }
                        if let Some(next) = new_focus {
                            manager.mark_text_dirty(next);
                        }
                        manager.focused_node = new_focus;
                        window.request_redraw();
                    }

                    if let Some(node_id) = action_node_id
                        && dispatch_click_gesture(&mut manager, node_id)
                    {
                        window.request_redraw();
                    }

                    // A click on a submit button gathers the enclosing form's
                    // fields and forwards them to the logic worker.
                    if let Some(submit_id) = submit_node_id
                        && dispatch_form_submit(&mut manager, submit_id)
                    {
                        window.request_redraw();
                    }
                }
                WindowEvent::KeyboardInput {
                    event: key_event, ..
                } => {
                    if !key_event.state.is_pressed() {
                        return;
                    }

                    // ── F12 toggles the inspector, regardless of focus ────────
                    if let winit::keyboard::Key::Named(NamedKey::F12) = key_event.logical_key {
                        manager.inspector.toggle();
                        let physical_size = window.inner_size();
                        let scale = window.scale_factor() as f32;
                        if let Err(e) = manager.resize_viewport(
                            physical_size.width as f32 / scale,
                            physical_size.height as f32 / scale,
                        ) {
                            tracing::error!("layout recalculation failed on inspector toggle: {e}");
                        }
                        window.request_redraw();
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
                                navigate_to_url(
                                    &mut manager,
                                    url,
                                    NavigationInitiator::UserGesture,
                                );
                            }
                            ChromeKeyAction::Reload => {
                                let url = manager.chrome_state.url.clone();
                                manager.chrome_state.loading = true;
                                navigate_to_url(
                                    &mut manager,
                                    url,
                                    NavigationInitiator::UserGesture,
                                );
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

                    // ── Tab / Shift-Tab: advance keyboard focus through the DOM ──
                    // Document order is the tab order; Mizu has no `tabindex`.
                    if let winit::keyboard::Key::Named(NamedKey::Tab) = key_event.logical_key {
                        let backward = manager.modifiers.shift_key();
                        if let Some(next) = manager.next_focus_target(backward)
                            && manager.focused_node != Some(next)
                        {
                            if let Some(prev) = manager.focused_node {
                                manager.mark_text_dirty(prev);
                            }
                            manager.mark_text_dirty(next);
                            manager.focused_node = Some(next);
                            window.request_redraw();
                        }
                        return;
                    }

                    // ── A DOM node has focus — route editing/activation keys to it ──
                    if let Some(focus_id) = manager.focused_node
                        && let Some(&input_u32) = manager.node_id_to_u32.get(&focus_id)
                    {
                        let is_input = manager
                            .dom
                            .get(focus_id)
                            .map(|n| n.value().primitive == crate::parser::Primitive::Input)
                            .unwrap_or(false);

                        match &key_event.logical_key {
                            winit::keyboard::Key::Named(NamedKey::Escape) => {
                                // Blur the focused node; Escape only exits the
                                // app / closes pickers when nothing is focused.
                                manager.focused_node = None;
                                manager.mark_text_dirty(focus_id);
                                window.request_redraw();
                            }
                            winit::keyboard::Key::Named(NamedKey::Backspace) if is_input => {
                                if let Some(buf) = manager.local_inputs.get_mut(&input_u32)
                                    && buf.pop().is_some()
                                {
                                    manager.mark_text_dirty(focus_id);
                                    window.request_redraw();
                                }
                            }
                            winit::keyboard::Key::Named(NamedKey::Enter) if is_input => {
                                // Enter submits the enclosing form, exactly
                                // like clicking its submit button.
                                if let Some(submitter) =
                                    find_form_submitter(&manager.dom, focus_id)
                                    && dispatch_form_submit(&mut manager, submitter)
                                {
                                    window.request_redraw();
                                }
                            }
                            winit::keyboard::Key::Named(NamedKey::Enter | NamedKey::Space)
                                if !is_input =>
                            {
                                // Activate the focused button/clickable node —
                                // the same ancestor walk and gesture dispatch
                                // the mouse click handler uses (SECURITY: this
                                // is not a second gesture path, it is the same
                                // `dispatch_click_gesture`/`dispatch_form_submit`
                                // helpers the click handler calls, anchored at
                                // the focused node instead of a hit-test result).
                                let (action_node_id, submit_node_id) =
                                    find_click_and_submit(&manager.dom, focus_id);
                                let mut redraw = false;
                                if let Some(node_id) = action_node_id
                                    && dispatch_click_gesture(&mut manager, node_id)
                                {
                                    redraw = true;
                                }
                                if let Some(submit_id) = submit_node_id
                                    && dispatch_form_submit(&mut manager, submit_id)
                                {
                                    redraw = true;
                                }
                                if redraw {
                                    window.request_redraw();
                                }
                            }
                            _ if is_input => {
                                let is_paste = manager.modifiers.control_key()
                                    && matches!(
                                        &key_event.logical_key,
                                        winit::keyboard::Key::Character(c)
                                            if c.eq_ignore_ascii_case("v")
                                    );
                                if is_paste {
                                    if let Ok(mut cb) = arboard::Clipboard::new()
                                        && let Ok(text) = cb.get_text()
                                    {
                                        let buf = manager
                                            .local_inputs
                                            .entry(input_u32)
                                            .or_default();
                                        if push_input_text(buf, &text) {
                                            manager.mark_text_dirty(focus_id);
                                            window.request_redraw();
                                        }
                                    }
                                } else if !manager.modifiers.control_key()
                                    && !manager.modifiers.alt_key()
                                    && !manager.modifiers.super_key()
                                    && let Some(text) = key_event.text.as_deref()
                                {
                                    let buf =
                                        manager.local_inputs.entry(input_u32).or_default();
                                    if push_input_text(buf, text) {
                                        manager.mark_text_dirty(focus_id);
                                        window.request_redraw();
                                    }
                                }
                            }
                            _ => {}
                        }
                        return;
                    }

                    // ── Escape: picker → inspector → exit, in that order ─────
                    if let winit::keyboard::Key::Named(NamedKey::Escape) = key_event.logical_key {
                        if manager.inspector.picker {
                            manager.inspector.set_picker(false);
                            window.set_cursor_icon(winit::window::CursorIcon::Default);
                            window.request_redraw();
                        } else if manager.inspector.open {
                            manager.inspector.toggle();
                            let physical_size = window.inner_size();
                            let scale = window.scale_factor() as f32;
                            if let Err(e) = manager.resize_viewport(
                                physical_size.width as f32 / scale,
                                physical_size.height as f32 / scale,
                            ) {
                                tracing::error!(
                                    "layout recalculation failed on inspector close: {e}"
                                );
                            }
                            window.request_redraw();
                        } else {
                            elwt.exit();
                        }
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

                    // ── Wheel over the inspector panel scrolls its content ────
                    if manager.inspector.open {
                        let logical_width = window.inner_size().width as f32 / scale;
                        if last_mouse_logical_x
                            >= crate::render::inspector::panel_left(logical_width)
                            && last_mouse_logical_y >= CHROME_HEIGHT
                        {
                            manager.inspector.scroll_by(delta_y * 2.0);
                            window.request_redraw();
                            return;
                        }
                    }

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

                    // ── Layer 3: Inspector panel + selection highlight ───────
                    if manager.inspector.open {
                        let logical_height = height as f32 / scale as f32;
                        // While picking, highlight the node under the cursor;
                        // otherwise the committed selection.
                        let highlight_target = if manager.inspector.picker {
                            manager.inspector.picker_hover
                        } else {
                            manager.inspector.selected
                        };
                        if let Some(sel) = highlight_target
                            && let Some(rect) = crate::render::inspector::node_screen_rect(
                                &manager.dom,
                                &manager.taffy,
                                &manager.node_to_taffy_id,
                                &manager.scroll_offsets,
                                manager.root_scroll_offset_y,
                                CHROME_HEIGHT,
                                sel,
                            )
                        {
                            crate::render::inspector::paint::paint_node_highlight(
                                &mut scene,
                                rect,
                                scale as f32,
                            );
                        }
                        let rows = {
                            let src = manager.inspector_sources();
                            crate::render::inspector::model::build_rows(&src, &manager.inspector)
                        };
                        crate::render::inspector::paint::paint_panel(
                            &mut scene,
                            &mut manager.inspector,
                            &rows,
                            logical_width,
                            logical_height,
                            scale as f32,
                            &mut manager.font_cx,
                            &mut manager.layout_cx,
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
                        crate::core::types::Value::Int((manager.root_scroll_offset_y as f64 * crate::core::types::DECIMAL_SCALE as f64).round() as i64),
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
                        for (sym, val) in response.state_update.mutated_variables {
                            let name_str = manager.store.interner.resolve(sym).unwrap_or("<unknown>");
                            manager.inspector_log.push_event(
                                crate::render::inspector::log::EventKind::Mutation,
                                format!("{name_str} = {val}"),
                            );
                            manager.store.state_machine.set_global(sym, val);
                            manager
                                .recent_mutations
                                .insert(sym, std::time::Instant::now());
                            state_changed = true;
                            mutated_symbols.push(sym);
                        }
                        for action in response.runtime_actions {
                            if let crate::network::RuntimeAction::Navigate { url } = &action {
                                // N2+N3: Navigate actions go through the choke point;
                                // capture the current gesture flag so cross-origin
                                // logic-driven navigation is blocked without a click.
                                manager.chrome_state.loading = true;
                                let url = url.clone();
                                let initiator = if manager.has_user_gesture {
                                    NavigationInitiator::UserGesture
                                } else {
                                    NavigationInitiator::DocumentLogic
                                };
                                navigate_to_url(&mut manager, url, initiator);
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
                                    &manager.local_inputs,
                                    &manager.node_id_to_u32,
                                    manager.focused_node,
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
            let mut next_wakeup = manager.root_timer_queue.keys().next().copied();

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

            let mut timers_fired = false;

            // Root `timer` declarations fire on the same clock; the action is
            // dispatched to the logic worker by declaration index.
            while let Some(&deadline) = manager.root_timer_queue.keys().next() {
                if now >= deadline {
                    if let Some(indices) = manager.root_timer_queue.remove(&deadline) {
                        for idx in indices {
                            let interval = match manager.root_timers.get(idx) {
                                Some(rt) => manager.resolve_root_timer_interval(&rt.interval),
                                None => continue,
                            };
                            let _ = manager
                                .logic_tx
                                .send(UiEvent::RootTimer { index: idx as u32 });
                            timers_fired = true;
                            if manager.inspector.open {
                                manager.inspector_log.push_event(
                                    crate::render::inspector::log::EventKind::Timer,
                                    format!("root timer #{idx}"),
                                );
                            }
                            if let Some(interval_ms) = interval {
                                let next_deadline =
                                    now + std::time::Duration::from_millis(interval_ms);
                                manager
                                    .root_timer_queue
                                    .entry(next_deadline)
                                    .or_default()
                                    .push(idx);
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

            if let Some(&t) = manager.root_timer_queue.keys().next() {
                next_wakeup = Some(next_wakeup.map(|w| w.min(t)).unwrap_or(t));
            }

            // Timer actions execute asynchronously in the logic worker; wake
            // again shortly so their responses are drained without waiting a
            // full timer period.
            if timers_fired {
                let drain_at = now + std::time::Duration::from_millis(16);
                next_wakeup = Some(next_wakeup.map(|w| w.min(drain_at)).unwrap_or(drain_at));
            }

            // Inspector Events tab shows live countdowns and Logic flashes
            // recent mutations — refresh those views at ~2 Hz while visible.
            if manager.inspector.open
                && matches!(
                    manager.inspector.tab,
                    crate::render::inspector::InspectorTab::Events
                        | crate::render::inspector::InspectorTab::Logic
                )
            {
                if now.duration_since(manager.inspector.last_events_refresh)
                    >= std::time::Duration::from_millis(500)
                {
                    manager.inspector.last_events_refresh = now;
                    window.request_redraw();
                }
                let tick =
                    manager.inspector.last_events_refresh + std::time::Duration::from_millis(500);
                next_wakeup = Some(next_wakeup.map(|w| w.min(tick)).unwrap_or(tick));
            }

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
