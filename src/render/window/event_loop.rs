//! `run_window_loop`, the Winit event loop.

use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::sync::Arc;

use ego_tree::Tree;
use vello::{
    AaConfig, Renderer, RendererOptions, Scene,
    kurbo::Affine,
    util::{RenderContext, RenderSurface},
};
use winit::{
    dpi::PhysicalSize,
    event::{Event, MouseScrollDelta, WindowEvent},
    keyboard::{Key, NamedKey},
    window::{Window, WindowBuilder},
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
use crate::render::preferences::ChromePalette;
use crate::render::vello_pipeline::{PaintContext, paint_node};

use crate::render::accessibility::{MizuUserEvent, build_a11y_tree, resolve_ego_id};

use super::focus::find_click_and_submit;
use super::input::{
    apply_clipboard_action, dispatch_click_gesture, dispatch_form_submit, find_form_submitter,
    push_input_text,
};
use super::manager::MizuWindowManager;
use super::navigate::{navigate_back, navigate_forward, navigate_to_url, process_network_result};

/// Everything needed to construct the initial window/manager state: the
/// parsed document (DOM, styles, logic) plus the URL it was loaded from.
/// Replaces `run_window_loop`'s prior 9-parameter positional argument list.
pub struct InitialDocument {
    /// The parsed DOM tree.
    pub dom: Tree<MizuNode>,
    /// Tag/class style rules from the `style` block.
    pub style_rules: HashMap<String, StyleRules>,
    /// Breakpoint/color-scheme style variants (ux-6).
    pub style_variants: Vec<crate::parser::style::StyleVariant>,
    /// Declared `logic` functions, keyed by interned name.
    pub logic_fns: FxHashMap<Symbol, MizuFunction>,
    /// The string interner shared by every symbol in this document.
    pub interner: StringInterner,
    /// Compile-time endpoint alias registry from the `urls` block.
    pub url_registry: crate::parser::UrlRegistry,
    /// The URL this document was loaded from, shown in the chrome bar.
    pub initial_url: String,
    /// `comp`-declared computed/derived bindings.
    pub computed_bindings: Vec<ComputedBinding>,
    /// Declared `timer` blocks at the root scope.
    pub root_timers: Vec<RootTimer>,
}

/// Mouse-position/drag state that persists across window events within a
/// single event-loop instance. Kept separate from [`MizuWindowManager`]
/// since it's display-server-adjacent input state, not document state.
#[derive(Default)]
struct MouseState {
    last_logical_x: f32,
    last_logical_y: f32,
    dragging_url_bar: bool,
}

/// Connects the rendering manager to the Winit event loop.
///
/// `allow_insecure`: when `true`, TLS certificate verification is skipped on
/// QUIC connections (development only).  When `false` (the default), every
/// `mizu://` connection must present a valid TLS certificate; the client drops
/// connections that fail verification.
pub fn run_window_loop(
    doc: InitialDocument,
    #[cfg(feature = "insecure-dev")] allow_insecure: bool,
) -> Result<(), MizuError> {
    let InitialDocument {
        dom,
        style_rules,
        style_variants,
        logic_fns,
        interner,
        url_registry,
        initial_url,
        computed_bindings,
        root_timers,
    } = doc;

    let event_loop = winit::event_loop::EventLoopBuilder::<MizuUserEvent>::with_user_event()
        .build()
        .map_err(|e| MizuError::ParseError(e.to_string()))?;
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
    // Must be created before `event_loop.run(...)` consumes `event_loop` by value.
    let accesskit_proxy = event_loop.create_proxy();

    let mut manager = MizuWindowManager::new(
        dom,
        style_rules,
        style_variants,
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
                func.body.root(),
                &func.body.arena,
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

    // `window "..."`'s inline text is stored here as the `title` attribute
    // (parser::layout::parse_primitive_and_attrs) — it sets the OS window
    // title only and is never rendered as page content.
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

    // ux-5: real light/dark detection. `None` (platform doesn't report a
    // theme) keeps the `UserPreferences::default()` scheme already set.
    if let Some(theme) = window.theme() {
        manager.preferences.color_scheme = theme.into();
    }

    // Delivers accesskit's initial-tree/action/deactivation events through
    // the same `Event::UserEvent` channel as everything else in this loop —
    // no separate thread, no separate handler wiring.
    let mut a11y_adapter = accesskit_winit::Adapter::with_event_loop_proxy(&window, accesskit_proxy);

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

    let mut mouse = MouseState::default();

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
            // Must run before this window event is handled below (accesskit
            // needs to observe focus/IME/etc. changes as they happen).
            a11y_adapter.process_event(&window, window_event);
            match window_event {
                WindowEvent::CloseRequested => {
                    elwt.exit();
                }
                WindowEvent::ThemeChanged(theme) => {
                    // ux-5: chrome re-themes live when the OS scheme changes.
                    manager.preferences.color_scheme = (*theme).into();
                    window.request_redraw();
                }
                WindowEvent::Resized(physical_size) => {
                    dispatch_resized(&mut manager, &mut render_cx, &mut surface, &window, elwt, *physical_size);
                }
                WindowEvent::CursorMoved { position, .. } => {
                    dispatch_cursor_moved(&mut manager, &window, *position, &mut mouse);
                }
                WindowEvent::MouseInput {
                    state: winit::event::ElementState::Released,
                    button: winit::event::MouseButton::Left,
                    ..
                } => {
                    mouse.dragging_url_bar = false;
                }
                WindowEvent::MouseInput {
                    state: winit::event::ElementState::Pressed,
                    button: winit::event::MouseButton::Left,
                    ..
                } => {
                    dispatch_mouse_pressed(&mut manager, &window, &mut mouse);
                }
                WindowEvent::KeyboardInput {
                    event: key_event, ..
                } => {
                    dispatch_keyboard_input(&mut manager, &window, elwt, key_event);
                }
                WindowEvent::ModifiersChanged(modifiers) => {
                    manager.modifiers = modifiers.state();
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    dispatch_mouse_wheel(&mut manager, &window, *delta, &mouse);
                }
                WindowEvent::RedrawRequested => {
                    dispatch_redraw_requested(
                        &mut manager,
                        &mut render_cx,
                        &mut surface,
                        &mut renderer,
                        &mut a11y_adapter,
                        &window,
                    );
                }
                _ => {}
            }
        } else if let Event::AboutToWait = event {
            dispatch_about_to_wait(&mut manager, &window, elwt);
        } else if let Event::UserEvent(MizuUserEvent::Accesskit(ak_event)) = event {
            dispatch_accesskit_event(&mut manager, &mut a11y_adapter, ak_event);
        }
    });

    res.map_err(|e| MizuError::ParseError(format!("Event loop error: {e}")))?;
    Ok(())
}

// ── WindowEvent dispatch ───────────────────────────────────────────────────

/// Handles `WindowEvent::Resized`: resizes the render surface and, throttled
/// to once per 16ms, recomputes layout at the new logical size.
fn dispatch_resized(
    manager: &mut MizuWindowManager,
    render_cx: &mut RenderContext,
    surface: &mut RenderSurface<'_>,
    window: &Window,
    elwt: &winit::event_loop::EventLoopWindowTarget<MizuUserEvent>,
    physical_size: PhysicalSize<u32>,
) {
    if physical_size.width == 0 || physical_size.height == 0 {
        return;
    }
    render_cx.resize_surface(surface, physical_size.width, physical_size.height);
    let scale_factor = window.scale_factor();
    let logical_width = physical_size.width as f64 / scale_factor;
    let logical_height = physical_size.height as f64 / scale_factor;

    let now = std::time::Instant::now();
    if now.duration_since(manager.last_layout_time) >= std::time::Duration::from_millis(16) {
        if let Err(e) = manager.resize_viewport(logical_width as f32, logical_height as f32) {
            tracing::error!("layout recalculation failed: {e}");
            elwt.exit();
        } else {
            manager.last_layout_time = now;
            manager.pending_resize = None;
            window.request_redraw();
        }
    } else {
        manager.pending_resize = Some((logical_width as f32, logical_height as f32));
    }
}

/// Handles `WindowEvent::CursorMoved`: updates the tracked mouse position,
/// continues an in-progress URL-bar drag-selection, updates the picker hover
/// highlight, and sets the button/default cursor icon over DOM content.
fn dispatch_cursor_moved(
    manager: &mut MizuWindowManager,
    window: &Window,
    position: winit::dpi::PhysicalPosition<f64>,
    mouse: &mut MouseState,
) {
    let scale_factor = window.scale_factor();
    mouse.last_logical_x = position.x as f32 / scale_factor as f32;
    mouse.last_logical_y = position.y as f32 / scale_factor as f32;

    // URL bar drag-selection
    if mouse.dragging_url_bar && manager.chrome_state.focused {
        let bar_left = url_text_left(window.inner_size().width as f32 / scale_factor as f32);
        let cs = &mut manager.chrome_state;
        let fc = &mut manager.font_cx;
        let lc = &mut manager.layout_cx;
        cs.extend_selection_to_x(mouse.last_logical_x, bar_left, fc, lc);
        window.request_redraw();
        return;
    }

    let mut hit_node_id = None;
    if mouse.last_logical_y >= CHROME_HEIGHT {
        hit_node_id = hit_test(
            &manager.dom,
            &manager.taffy,
            &manager.node_to_taffy_id,
            &manager.scroll_offsets,
            mouse.last_logical_x,
            mouse.last_logical_y - CHROME_HEIGHT + manager.root_scroll_offset_y,
        );
    }
    // ── Picker hover: live-highlight the node under the cursor ─
    if manager.inspector.open && manager.inspector.picker {
        window.set_cursor_icon(winit::window::CursorIcon::Crosshair);
        let logical_width = window.inner_size().width as f32 / scale_factor as f32;
        let over_page = mouse.last_logical_x < crate::render::inspector::panel_left(logical_width);
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

/// Handles a left-click on the chrome bar: routes to the hit chrome zone
/// (back/forward/reload/URL-bar/background) and always redraws.
fn dispatch_chrome_click(
    manager: &mut MizuWindowManager,
    window: &Window,
    mouse: &mut MouseState,
    logical_width: f32,
) {
    let zone = chrome_hit_zone(mouse.last_logical_x, mouse.last_logical_y, logical_width);
    match zone {
        ChromeHitZone::BackButton => {
            navigate_back(manager);
        }
        ChromeHitZone::ForwardButton => {
            navigate_forward(manager);
        }
        ChromeHitZone::ReloadButton => {
            tracing::debug!("reload button clicked");
            manager.chrome_state.loading = true;
            let url = manager.chrome_state.url.clone();
            navigate_to_url(manager, url, NavigationInitiator::UserGesture);
        }
        ChromeHitZone::UrlBar => {
            let bar_left = url_text_left(logical_width);
            manager.chrome_state.focused = true;
            mouse.dragging_url_bar = true;
            {
                let cs = &mut manager.chrome_state;
                let fc = &mut manager.font_cx;
                let lc = &mut manager.layout_cx;
                cs.set_cursor_from_click(mouse.last_logical_x, bar_left, fc, lc);
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
}

/// Handles a left-click inside the open inspector panel. Returns `true` if
/// the click was inside the panel (and therefore fully handled here).
fn dispatch_inspector_panel_click(
    manager: &mut MizuWindowManager,
    window: &Window,
    mouse: &MouseState,
    logical_width: f32,
) -> bool {
    if !(manager.inspector.open
        && mouse.last_logical_x >= crate::render::inspector::panel_left(logical_width))
    {
        return false;
    }
    let rows = {
        let src = manager.inspector_sources();
        crate::render::inspector::model::build_rows(&src, &manager.inspector)
    };
    let x = mouse.last_logical_x - crate::render::inspector::panel_left(logical_width);
    let y = mouse.last_logical_y - CHROME_HEIGHT;
    if crate::render::inspector::handle_panel_click(&mut manager.inspector, &rows, x, y) {
        window.request_redraw();
    }
    true
}

/// Handles a left-click while the element picker is active: selects the
/// node under the cursor instead of triggering its action. Returns `true`
/// when the picker was active (and therefore consumed the click).
fn dispatch_picker_click(manager: &mut MizuWindowManager, window: &Window, mouse: &MouseState) -> bool {
    if !(manager.inspector.open && manager.inspector.picker) {
        return false;
    }
    let hit = hit_test(
        &manager.dom,
        &manager.taffy,
        &manager.node_to_taffy_id,
        &manager.scroll_offsets,
        mouse.last_logical_x,
        mouse.last_logical_y - CHROME_HEIGHT + manager.root_scroll_offset_y,
    );
    if let Some(hit_id) = hit {
        manager.inspector.select_with_ancestors(&manager.dom, hit_id);
        // Bring the selection into view in the Elements tree.
        let rows = {
            let src = manager.inspector_sources();
            crate::render::inspector::model::build_rows(&src, &manager.inspector)
        };
        if let Some(idx) = rows.iter().position(|r| r.node == Some(hit_id)) {
            let logical_height = window.inner_size().height as f32 / window.scale_factor() as f32;
            let viewport_h =
                (logical_height - CHROME_HEIGHT - crate::render::inspector::TAB_BAR_HEIGHT).max(0.0);
            manager.inspector.scroll_to_row(idx, viewport_h);
        }
    }
    manager.inspector.set_picker(false);
    window.set_cursor_icon(winit::window::CursorIcon::Default);
    window.request_redraw();
    true
}

/// Handles a left-click on DOM content: updates focus, and dispatches
/// click/submit actions for the nearest ancestor carrying them.
fn dispatch_dom_click(manager: &mut MizuWindowManager, window: &Window, mouse: &MouseState) {
    manager.chrome_state.focused = false;

    let hit_node_id = hit_test(
        &manager.dom,
        &manager.taffy,
        &manager.node_to_taffy_id,
        &manager.scroll_offsets,
        mouse.last_logical_x,
        mouse.last_logical_y - CHROME_HEIGHT + manager.root_scroll_offset_y,
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
        && dispatch_click_gesture(manager, node_id)
    {
        window.request_redraw();
    }

    // A click on a submit button gathers the enclosing form's
    // fields and forwards them to the logic worker.
    if let Some(submit_id) = submit_node_id
        && dispatch_form_submit(manager, submit_id)
    {
        window.request_redraw();
    }
}

/// Handles `WindowEvent::MouseInput` (left button pressed): routes to
/// exactly one of the chrome bar, the inspector panel, the element picker,
/// or DOM content, in that priority order.
fn dispatch_mouse_pressed(manager: &mut MizuWindowManager, window: &Window, mouse: &mut MouseState) {
    let logical_width = window.inner_size().width as f32 / window.scale_factor() as f32;

    if mouse.last_logical_y < CHROME_HEIGHT {
        dispatch_chrome_click(manager, window, mouse, logical_width);
        return;
    }
    if dispatch_inspector_panel_click(manager, window, mouse, logical_width) {
        return;
    }
    if dispatch_picker_click(manager, window, mouse) {
        return;
    }
    dispatch_dom_click(manager, window, mouse);
}

/// Handles the global keyboard shortcuts that apply regardless of focus:
/// F12 (toggle inspector) and Alt+Left/Right (history back/forward). Returns
/// `true` if the key was one of these (caller should stop processing).
fn handle_global_key_shortcuts(
    manager: &mut MizuWindowManager,
    window: &Window,
    key_event: &winit::event::KeyEvent,
) -> bool {
    if let Key::Named(NamedKey::F12) = key_event.logical_key {
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
        return true;
    }

    // Alt+Left / Alt+Right: history Back/Forward, regardless of focus (the
    // platform-standard shortcuts).
    if manager.modifiers.alt_key() {
        match key_event.logical_key {
            Key::Named(NamedKey::ArrowLeft) => {
                navigate_back(manager);
                window.request_redraw();
                return true;
            }
            Key::Named(NamedKey::ArrowRight) => {
                navigate_forward(manager);
                window.request_redraw();
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Routes a key to the chrome URL bar when it has focus. Returns `true` when
/// the bar was focused (and therefore consumed the key).
fn handle_chrome_key(
    manager: &mut MizuWindowManager,
    window: &Window,
    key_event: &winit::event::KeyEvent,
) -> bool {
    if !manager.chrome_state.focused {
        return false;
    }
    let action = {
        let cs = &mut manager.chrome_state;
        cs.handle_key(&key_event.logical_key, key_event.text.as_deref(), manager.modifiers)
    };
    match action {
        ChromeKeyAction::Navigate(url) => {
            manager.chrome_state.loading = true;
            navigate_to_url(manager, url, NavigationInitiator::UserGesture);
        }
        ChromeKeyAction::Reload => {
            let url = manager.chrome_state.url.clone();
            manager.chrome_state.loading = true;
            navigate_to_url(manager, url, NavigationInitiator::UserGesture);
        }
        ChromeKeyAction::Back => {
            navigate_back(manager);
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
    true
}

/// Handles Tab/Shift-Tab keyboard focus advancement through the DOM.
/// Document order is the tab order; Mizu has no `tabindex`. Returns `true`
/// when the key was Tab (and therefore consumed).
fn handle_tab_focus(
    manager: &mut MizuWindowManager,
    window: &Window,
    key_event: &winit::event::KeyEvent,
) -> bool {
    if !matches!(key_event.logical_key, Key::Named(NamedKey::Tab)) {
        return false;
    }
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
    true
}

/// Routes editing/activation keys to the currently DOM-focused node
/// (Escape/Backspace/Enter/Space, or plain text input). Returns `true` when
/// a node was focused (and therefore consumed the key).
fn handle_focused_node_key(
    manager: &mut MizuWindowManager,
    window: &Window,
    key_event: &winit::event::KeyEvent,
) -> bool {
    let Some(focus_id) = manager.focused_node else {
        return false;
    };
    let Some(&input_u32) = manager.node_id_to_u32.get(&focus_id) else {
        return false;
    };
    let is_input = manager
        .dom
        .get(focus_id)
        .map(|n| n.value().primitive == crate::parser::Primitive::Input)
        .unwrap_or(false);

    match &key_event.logical_key {
        Key::Named(NamedKey::Escape) => {
            // Blur the focused node; Escape only exits the
            // app / closes pickers when nothing is focused.
            manager.focused_node = None;
            manager.mark_text_dirty(focus_id);
            window.request_redraw();
        }
        Key::Named(NamedKey::Backspace) if is_input => {
            if let Some(buf) = manager.local_inputs.get_mut(&input_u32)
                && buf.pop().is_some()
            {
                manager.mark_text_dirty(focus_id);
                window.request_redraw();
            }
        }
        Key::Named(NamedKey::Enter) if is_input => {
            // Enter submits the enclosing form, exactly
            // like clicking its submit button.
            if let Some(submitter) = find_form_submitter(&manager.dom, focus_id)
                && dispatch_form_submit(manager, submitter)
            {
                window.request_redraw();
            }
        }
        Key::Named(NamedKey::Enter | NamedKey::Space) if !is_input => {
            // Activate the focused button/clickable node —
            // the same ancestor walk and gesture dispatch
            // the mouse click handler uses (SECURITY: this
            // is not a second gesture path, it is the same
            // `dispatch_click_gesture`/`dispatch_form_submit`
            // helpers the click handler calls, anchored at
            // the focused node instead of a hit-test result).
            let (action_node_id, submit_node_id) = find_click_and_submit(&manager.dom, focus_id);
            let mut redraw = false;
            if let Some(node_id) = action_node_id
                && dispatch_click_gesture(manager, node_id)
            {
                redraw = true;
            }
            if let Some(submit_id) = submit_node_id
                && dispatch_form_submit(manager, submit_id)
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
                    Key::Character(c) if c.eq_ignore_ascii_case("v")
                );
            if is_paste {
                if let Ok(mut cb) = arboard::Clipboard::new()
                    && let Ok(text) = cb.get_text()
                {
                    let buf = manager.local_inputs.entry(input_u32).or_default();
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
                let buf = manager.local_inputs.entry(input_u32).or_default();
                if push_input_text(buf, text) {
                    manager.mark_text_dirty(focus_id);
                    window.request_redraw();
                }
            }
        }
        _ => {}
    }
    true
}

/// Final fallback for Escape when nothing else consumed it: closes the
/// picker, then the inspector, then exits the app — in that order.
fn handle_escape_fallback(
    manager: &mut MizuWindowManager,
    window: &Window,
    elwt: &winit::event_loop::EventLoopWindowTarget<MizuUserEvent>,
    key_event: &winit::event::KeyEvent,
) {
    if !matches!(key_event.logical_key, Key::Named(NamedKey::Escape)) {
        return;
    }
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
            tracing::error!("layout recalculation failed on inspector close: {e}");
        }
        window.request_redraw();
    } else {
        elwt.exit();
    }
}

/// Handles `WindowEvent::KeyboardInput`, routing a pressed key through (in
/// order) global shortcuts, the chrome URL bar, Tab focus advancement, the
/// DOM-focused node, and finally the Escape fallback.
fn dispatch_keyboard_input(
    manager: &mut MizuWindowManager,
    window: &Window,
    elwt: &winit::event_loop::EventLoopWindowTarget<MizuUserEvent>,
    key_event: &winit::event::KeyEvent,
) {
    if !key_event.state.is_pressed() {
        return;
    }
    if handle_global_key_shortcuts(manager, window, key_event) {
        return;
    }
    if handle_chrome_key(manager, window, key_event) {
        return;
    }
    if handle_tab_focus(manager, window, key_event) {
        return;
    }
    if handle_focused_node_key(manager, window, key_event) {
        return;
    }
    handle_escape_fallback(manager, window, elwt, key_event);
}

/// Handles `WindowEvent::MouseWheel`: scrolls the inspector panel, the
/// nearest scrollable DOM ancestor under the cursor, or the root document,
/// in that priority order.
fn dispatch_mouse_wheel(
    manager: &mut MizuWindowManager,
    window: &Window,
    delta: MouseScrollDelta,
    mouse: &MouseState,
) {
    let scale = window.scale_factor() as f32;
    let delta_y = match delta {
        MouseScrollDelta::LineDelta(_dx, dy) => -dy * 20.0,
        MouseScrollDelta::PixelDelta(physical) => -(physical.y as f32) / scale,
    };

    // ── Wheel over the inspector panel scrolls its content ────
    if manager.inspector.open {
        let logical_width = window.inner_size().width as f32 / scale;
        if mouse.last_logical_x >= crate::render::inspector::panel_left(logical_width)
            && mouse.last_logical_y >= CHROME_HEIGHT
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
        mouse.last_logical_x,
        mouse.last_logical_y - CHROME_HEIGHT + manager.root_scroll_offset_y,
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
            let max_scroll = if let Some(&t_id) = manager.node_to_taffy_id.get(&node_id) {
                if let Ok(container_layout) = manager.taffy.layout(t_id) {
                    let container_h = container_layout.size.height;
                    let mut content_h: f32 = 0.0;
                    if let Some(node_ref) = manager.dom.get(node_id) {
                        for child in node_ref.children() {
                            if let Some(&c_t_id) = manager.node_to_taffy_id.get(&child.id())
                                && let Ok(child_layout) = manager.taffy.layout(c_t_id)
                            {
                                let bottom = child_layout.location.y + child_layout.size.height;
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

            let current = manager.scroll_offsets.get(&node_id).copied().unwrap_or(0.0);
            let new_offset = (current + delta_y).clamp(0.0, max_scroll);
            manager.scroll_offsets.insert(node_id, new_offset);
            scrolled = true;
            break;
        }

        candidate = manager.dom.get(node_id).and_then(|n| n.parent().map(|p| p.id()));
    }

    if scrolled {
        window.request_redraw();
    } else if mouse.last_logical_y >= CHROME_HEIGHT {
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
        manager.root_scroll_offset_y = (manager.root_scroll_offset_y + delta_y).clamp(0.0, max_scroll);
        window.request_redraw();
    }
}

/// Handles `WindowEvent::RedrawRequested`: paints the DOM, chrome bar, and
/// inspector panel into a fresh `Scene`, then presents it to the surface.
/// Left as a single function — like `paint_node`'s own per-primitive paint
/// steps, this is one paint pass with a fixed, legitimately linear layer
/// order (DOM content, then chrome, then inspector), not several tangled
/// concerns.
fn dispatch_redraw_requested(
    manager: &mut MizuWindowManager,
    render_cx: &mut RenderContext,
    surface: &mut RenderSurface<'_>,
    renderer: &mut Renderer,
    a11y_adapter: &mut accesskit_winit::Adapter,
    window: &Window,
) {
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
        build_a11y_tree(&manager.dom, &manager.node_id_to_u32, manager.focused_node, &manager.store)
    });

    let device = &render_cx.devices[surface.dev_id].device;
    let queue = &render_cx.devices[surface.dev_id].queue;

    // Resolve background color from window style rule
    let mut bg_color = vello::peniko::Color::rgba8(255, 255, 255, 255);
    if let Some(rules) = manager.style_rules.get("window")
        && let Some(crate::parser::style::MizuBackground::Solid(c)) = &rules.background
    {
        bg_color = vello::peniko::Color::rgba8(c.r, c.g, c.b, c.a);
    }

    let elapsed_ms = manager.start_time.elapsed().as_millis() as u64;
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

    let dom_transform =
        Affine::scale(scale) * Affine::translate((0.0, (CHROME_HEIGHT - manager.root_scroll_offset_y) as f64));

    let has_animations;
    {
        let chrome_url_snapshot = manager.chrome_state.url.clone();
        let mut ctx = PaintContext {
            tree: &manager.dom,
            taffy: &manager.taffy,
            node_to_taffy_id: &manager.node_to_taffy_id,
            style_rules: &manager.style_rules,
            style_variants: &manager.style_variants,
            render_env: crate::render::responsive::RenderEnvironment {
                viewport: manager.viewport_size,
                color_scheme: manager.preferences.color_scheme,
            },
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
        let can_go_back = manager.history.can_go_back();
        let can_go_forward = manager.history.can_go_forward();
        let palette = ChromePalette::for_preferences(&manager.preferences);
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
            can_go_back,
            can_go_forward,
            &palette,
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
            crate::render::inspector::paint::paint_node_highlight(&mut scene, rect, scale as f32);
        }
        let rows = {
            let src = manager.inspector_sources();
            crate::render::inspector::model::build_rows(&src, &manager.inspector)
        };
        crate::render::inspector::paint::paint_panel(
            &mut scene,
            &mut crate::render::inspector::paint::PanelPaintContext {
                state: &mut manager.inspector,
                rows: &rows,
                window_width: logical_width,
                window_height: logical_height,
                scale: scale as f32,
                font_cx: &mut manager.font_cx,
                layout_cx: &mut manager.layout_cx,
            },
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

    if let Err(e) = renderer.render_to_surface(device, queue, &scene, &surface_texture, &render_params) {
        tracing::error!("render_to_surface failed: {e}");
        return;
    }

    surface_texture.present();

    // Expose scroll state to the logic store
    manager.store.set(
        "root_scroll_y",
        crate::core::types::Value::Int(
            (manager.root_scroll_offset_y as f64 * crate::core::types::DECIMAL_SCALE as f64).round() as i64,
        ),
    );
}

// ── AboutToWait dispatch ────────────────────────────────────────────────────

/// Drains all pending network results without blocking. Collecting into a
/// `Vec` first avoids a split-borrow conflict between `manager.network_rx`
/// (needs `&mut`) and the rest of `manager` (needed by
/// `process_network_result`).
fn drain_network_results(manager: &mut MizuWindowManager) {
    let network_msgs: Vec<_> = std::iter::from_fn(|| manager.network_rx.try_recv().ok()).collect();
    for res in network_msgs {
        process_network_result(manager, res);
    }
}

/// Drains the logic worker's response channel, applying mutated variables
/// and dispatching runtime actions (navigate/clipboard/capability actions).
/// Returns whether any variable changed and which symbols changed, so the
/// caller can decide whether a layout recompute is needed.
fn drain_logic_worker_results(manager: &mut MizuWindowManager) -> (bool, Vec<Symbol>) {
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
                    manager.recent_mutations.insert(sym, std::time::Instant::now());
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
                        navigate_to_url(manager, url, initiator);
                    } else if let crate::network::RuntimeAction::CopyToClipboard { node_id } = &action {
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
    (state_changed, mutated_symbols)
}

/// Recomputes text layout for every node depending on a mutated symbol (or
/// carrying dirty typing state), marking Taffy nodes dirty and triggering a
/// viewport re-layout if any dimensions actually changed.
fn recompute_dirty_layout(manager: &mut MizuWindowManager, window: &Window, mutated_symbols: Vec<Symbol>) {
    manager.setup_timers();

    let mut layout_dirty = manager.typing_layout_dirty;
    manager.typing_layout_dirty = false;

    for sym in mutated_symbols {
        if let Some(nodes) = manager.dependency_index.get(&sym) {
            for &node_id in nodes {
                manager.dirty_nodes.insert(node_id);

                let current_width = if let Some(&taffy_node) = manager.node_to_taffy_id.get(&node_id) {
                    if let Ok(layout) = manager.taffy.layout(taffy_node) {
                        Some(layout.size.width)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let old_dims = manager.text_dimensions.get(&node_id).copied();

                let render_env = crate::render::responsive::RenderEnvironment {
                    viewport: manager.viewport_size,
                    color_scheme: manager.preferences.color_scheme,
                };
                if let Some((new_dims, layout)) = crate::render::text_engine::calculate_node_text(
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
                    &manager.style_variants,
                    &render_env,
                ) {
                    manager.text_layouts.insert(node_id, layout);
                    manager.text_dimensions.insert(node_id, new_dims);
                    manager.dirty_nodes.remove(&node_id);

                    let dimensions_changed = match old_dims {
                        Some(old) => {
                            (old.0 - new_dims.0).abs() > f32::EPSILON || (old.1 - new_dims.1).abs() > f32::EPSILON
                        }
                        None => true,
                    };

                    if dimensions_changed
                        && let Some(&taffy_node) = manager.node_to_taffy_id.get(&node_id)
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

/// Computes and applies throttled resize / root-timer firing / inspector
/// refresh / network-poll scheduling, then sets the event loop's next wake
/// deadline (or `Wait` if nothing is pending).
fn schedule_next_wakeup(
    manager: &mut MizuWindowManager,
    window: &Window,
    elwt: &winit::event_loop::EventLoopWindowTarget<MizuUserEvent>,
) {
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
                    let _ = manager.logic_tx.send(UiEvent::RootTimer { index: idx as u32 });
                    timers_fired = true;
                    if manager.inspector.open {
                        manager.inspector_log.push_event(
                            crate::render::inspector::log::EventKind::Timer,
                            format!("root timer #{idx}"),
                        );
                    }
                    if let Some(interval_ms) = interval {
                        let next_deadline = now + std::time::Duration::from_millis(interval_ms);
                        manager.root_timer_queue.entry(next_deadline).or_default().push(idx);
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
            crate::render::inspector::InspectorTab::Events | crate::render::inspector::InspectorTab::Logic
        )
    {
        if now.duration_since(manager.inspector.last_events_refresh) >= std::time::Duration::from_millis(500) {
            manager.inspector.last_events_refresh = now;
            window.request_redraw();
        }
        let tick = manager.inspector.last_events_refresh + std::time::Duration::from_millis(500);
        next_wakeup = Some(next_wakeup.map(|w| w.min(tick)).unwrap_or(tick));
    }

    // While a network fetch is in flight, poll every 16 ms so the
    // try_recv drain fires regularly and the UI stays responsive.
    if manager.chrome_state.loading {
        let poll_deadline = std::time::Instant::now() + std::time::Duration::from_millis(16);
        next_wakeup = Some(next_wakeup.map(|d: std::time::Instant| d.min(poll_deadline)).unwrap_or(poll_deadline));
    }

    if let Some(deadline) = next_wakeup {
        elwt.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(deadline));
    } else {
        elwt.set_control_flow(winit::event_loop::ControlFlow::Wait);
    }
}

/// Handles `Event::AboutToWait`: drains network/logic worker results,
/// recomputes dirty layout if anything changed, then schedules the next
/// wakeup.
fn dispatch_about_to_wait(
    manager: &mut MizuWindowManager,
    window: &Window,
    elwt: &winit::event_loop::EventLoopWindowTarget<MizuUserEvent>,
) {
    drain_network_results(manager);
    let (state_changed, mutated_symbols) = drain_logic_worker_results(manager);
    if state_changed || manager.typing_layout_dirty {
        recompute_dirty_layout(manager, window, mutated_symbols);
    }
    schedule_next_wakeup(manager, window, elwt);
}

// ── Accesskit UserEvent dispatch ────────────────────────────────────────────

/// Handles an `accesskit_winit::Event` delivered via `Event::UserEvent`:
/// serves the initial accessibility tree on request, and routes an
/// AT-initiated action through the same gesture-gated dispatch helpers
/// keyboard activation uses.
fn dispatch_accesskit_event(
    manager: &mut MizuWindowManager,
    a11y_adapter: &mut accesskit_winit::Adapter,
    ak_event: accesskit_winit::Event,
) {
    match ak_event.window_event {
        accesskit_winit::WindowEvent::InitialTreeRequested => {
            a11y_adapter.update_if_active(|| {
                build_a11y_tree(&manager.dom, &manager.node_id_to_u32, manager.focused_node, &manager.store)
            });
        }
        accesskit_winit::WindowEvent::ActionRequested(request) => {
            // SECURITY (ux-2 guardrail): an AT-initiated action is a
            // real user gesture — route it through the *same*
            // gesture-gated dispatch keyboard activation (ux-1) uses,
            // never a second path into the evaluator.
            let Some(ego_id) = resolve_ego_id(&manager.u32_to_node_id, request.target) else {
                return;
            };
            let mut redraw = false;
            match request.action {
                accesskit::Action::Focus => {
                    if manager.focused_node != Some(ego_id) {
                        if let Some(prev) = manager.focused_node {
                            manager.mark_text_dirty(prev);
                        }
                        manager.mark_text_dirty(ego_id);
                        manager.focused_node = Some(ego_id);
                        redraw = true;
                    }
                }
                accesskit::Action::Default => {
                    let (action_node_id, submit_node_id) = find_click_and_submit(&manager.dom, ego_id);
                    if let Some(node_id) = action_node_id
                        && dispatch_click_gesture(manager, node_id)
                    {
                        redraw = true;
                    }
                    if let Some(submit_id) = submit_node_id
                        && dispatch_form_submit(manager, submit_id)
                    {
                        redraw = true;
                    }
                }
                _ => {}
            }
            if redraw && let Some(window) = manager.window.as_ref() {
                window.request_redraw();
            }
        }
        accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
    }
}
