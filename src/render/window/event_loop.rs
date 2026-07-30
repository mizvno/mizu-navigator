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
use crate::core::types::{StringInterner, Symbol};
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
    apply_clipboard_action, dispatch_click_gesture, dispatch_file_input_click,
    dispatch_form_submit, find_form_submitter, is_file_input, push_input_text,
};
use super::manager::{
    MizuWindowManager, execute_tab_capability_action, refresh_tab_virtualized_windows,
    resize_tab_viewport,
};
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
    last_click_time: Option<std::time::Instant>,
    last_click_pos: Option<(f32, f32)>,
    click_count: u8,
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
    // Startup injection targets the single initial tab. `MizuWindowManager::new`
    // built it against a placeholder interner; everything below re-seeds it
    // with the real parsed document state.
    {
        let tab = manager.active_mut();
        let mut interner = interner;
        interner.get_or_intern("$form");
        for rt in &root_timers {
            if let crate::parser::Action::Assign { target, .. } = &rt.action {
                interner.get_or_intern(target);
            }
        }
        for node in tab.dom.nodes() {
            for event in node.value().events.values() {
                match event {
                    crate::parser::EventBlock::Click { action }
                    | crate::parser::EventBlock::Submit { action } => {
                        if let crate::parser::Action::Assign { target, .. } = action {
                            interner.get_or_intern(target);
                        }
                    }
                }
            }
            if let Some(text) = node.value().attributes.get("content") {
                let vars = crate::render::text_engine::extract_placeholders(text);
                for var in vars {
                    interner.get_or_intern(&var);
                }
            }
        }
        tab.store = crate::core::types::VariableStore {
            evaluator: Default::default(),
            interner,
        }
        .freeze();
        tab.url_registry = url_registry;
        tab.computed_bindings = computed_bindings;
        tab.root_timers = root_timers;

        // Inject the startup URL into the store
        tab.store.set_runtime(
            "window_url",
            crate::core::types::Value::from(initial_url.clone()),
        );
        tab.chrome_state.url = initial_url;

        // Pre-seed the state: evaluate all zero-arity functions and populate the store.
        let logic_fns = tab.logic_fns.clone();
        for (&sym, func) in &logic_fns {
            if func.params.is_empty()
                && let Ok(val) = crate::parser::logic::evaluate(
                    func.body.root(),
                    &func.body.arena,
                    &mut tab.store,
                    &logic_fns,
                    0,
                )
            {
                tab.store.evaluator.set_global(sym, val);
            }
        }

        // Pre-seed comp vars in the render store.
        let all_syms: rustc_hash::FxHashSet<Symbol> =
            tab.store.evaluator.global_store.keys().copied().collect();
        let computed = tab.computed_bindings.clone();
        let fns = tab.logic_fns.clone();
        let reverse_index = crate::parser::logic::build_comp_reverse_index(&computed);
        crate::parser::logic::recompute_computed_bindings(
            &mut tab.store,
            &computed,
            &fns,
            &all_syms,
            &reverse_index,
        );
        tab.store.evaluator.undo_log.clear();

        // Rebuild node mappings and dependency index using the correct, fully-populated interner.
        // This ensures that variable dependency tracking works correctly from startup.
        tab.rebuild_node_mappings();
        tab.rebuild_dependency_index();
    }
    manager.active().trigger_logic_reload(&manager.logic_tx);
    manager.active_mut().setup_timers();

    let root_node = manager.active().dom.root().value();
    if root_node.primitive != Primitive::Doc {
        return Err(MizuError::ParseError("Root element must be a `doc`".into()));
    }

    // `doc`'s explicit `title "..."` attribute sets the OS window title —
    // never rendered as page content (parser::layout::parse_primitive_and_attrs
    // rejects `title` on any other primitive, and no longer accepts it as
    // positional inline text on `doc` either).
    let title = root_node
        .attributes
        .get("title")
        .cloned()
        .unwrap_or_else(|| "Mizu Navigator".to_string());

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
    let mut a11y_adapter =
        accesskit_winit::Adapter::with_event_loop_proxy(&window, accesskit_proxy);

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
                    // ux-6 style variants resolve against the color scheme, so
                    // every tab's taffy styles are now stale. The visible one
                    // is rebuilt immediately; the rest on activation.
                    let size = manager.window_logical_size;
                    if let Err(e) = manager.resize_viewport(size.0, size.1) {
                        tracing::error!(error = ?e, "relayout after theme change failed");
                    }
                    window.request_redraw();
                }
                WindowEvent::Resized(physical_size) => {
                    dispatch_resized(
                        &mut manager,
                        &mut render_cx,
                        &mut surface,
                        &window,
                        elwt,
                        *physical_size,
                    );
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
                    dispatch_mouse_pressed(&mut manager, &window, elwt, &mut mouse);
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
        } else if let Event::LoopExiting = event {
            // The single exit path: every `elwt.exit()` in this file — the
            // close button, Ctrl+W on the last tab, Escape, a fatal layout
            // error — ends up here, so history is persisted no matter which
            // one the user took.
            manager.history_log.save_to_disk();
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
/// continues an in-progress URL-bar drag-selection, updates the history
/// sidebar and picker hover highlights, and sets the button/default cursor
/// icon over DOM content.
fn dispatch_cursor_moved(
    manager: &mut MizuWindowManager,
    window: &Window,
    position: winit::dpi::PhysicalPosition<f64>,
    mouse: &mut MouseState,
) {
    let scale_factor = window.scale_factor();
    mouse.last_logical_x = position.x as f32 / scale_factor as f32;
    mouse.last_logical_y = position.y as f32 / scale_factor as f32;

    // ── History sidebar hover ─────────────────────────────────
    // Before the split borrow, because the panel is window-level. Runs even
    // when the cursor has left the panel, so the highlight follows it out.
    if manager.history_sidebar.open {
        let hovered = crate::render::history_sidebar::hovered_entry(
            mouse.last_logical_x,
            mouse.last_logical_y,
            &manager.history_log,
            manager.history_sidebar.scroll_offset,
            CHROME_HEIGHT,
        );
        if manager.history_sidebar.hovered != hovered {
            manager.history_sidebar.hovered = hovered;
            window.request_redraw();
        }
        if hovered.is_some() {
            // A row is a link: say so, and skip the DOM hit test entirely —
            // the page under the panel is not what the cursor is over.
            window.set_cursor_icon(winit::window::CursorIcon::Pointer);
            return;
        }
    }

    // ── Autocomplete Dropdown Hover ───────────────────────────
    let tab_count = manager.tabs.len();
    let dropdown_count = {
        let tab = manager.active();
        if tab.chrome_state.focused {
            tab.chrome_state.suggestions.len()
        } else {
            0
        }
    };

    let mut dropdown_hovered = false;
    if dropdown_count > 0 {
        let layout = crate::render::chrome_vello::ChromeLayout {
            window_width: window.inner_size().width as f32 / scale_factor as f32,
            tab_count,
            dropdown_count,
        };
        let hit_zone = crate::render::chrome_vello::chrome_hit_zone(
            mouse.last_logical_x,
            mouse.last_logical_y,
            &layout,
        );

        let tab = manager.active_mut();
        if let crate::render::chrome_vello::ChromeHitZone::AutocompleteSuggestion(i) = hit_zone {
            if tab.chrome_state.hovered_suggestion != Some(i) {
                tab.chrome_state.hovered_suggestion = Some(i);
                window.request_redraw();
            }
            dropdown_hovered = true;
            window.set_cursor_icon(winit::window::CursorIcon::Pointer);
        } else if tab.chrome_state.hovered_suggestion.is_some() {
            tab.chrome_state.hovered_suggestion = None;
            window.request_redraw();
        }
    } else {
        let tab = manager.active_mut();
        if tab.chrome_state.hovered_suggestion.is_some() {
            tab.chrome_state.hovered_suggestion = None;
            window.request_redraw();
        }
    }

    if dropdown_hovered {
        return;
    }

    let (tab, mut ctx) = manager.split_active();

    // URL bar drag-selection
    if mouse.dragging_url_bar && tab.chrome_state.focused {
        let bar_left = url_text_left(window.inner_size().width as f32 / scale_factor as f32);
        let cs = &mut tab.chrome_state;
        let fc = &mut ctx.font_cx;
        let lc = &mut ctx.layout_cx;
        cs.extend_selection_to_x(mouse.last_logical_x, bar_left, fc, lc);
        window.request_redraw();
        return;
    }

    let mut hit_node_id = None;
    if mouse.last_logical_y >= CHROME_HEIGHT {
        hit_node_id = hit_test(
            &tab.dom,
            &tab.taffy,
            &tab.node_to_taffy_id,
            &tab.scroll_offsets,
            mouse.last_logical_x,
            mouse.last_logical_y - CHROME_HEIGHT + tab.root_scroll_offset_y,
        );
    }
    // ── Picker hover: live-highlight the node under the cursor ─
    if tab.inspector.open && tab.inspector.picker {
        window.set_cursor_icon(winit::window::CursorIcon::Crosshair);
        let logical_width = window.inner_size().width as f32 / scale_factor as f32;
        let over_page = mouse.last_logical_x < crate::render::inspector::panel_left(logical_width);
        let hover = if over_page { hit_node_id } else { None };
        if tab.inspector.picker_hover != hover {
            tab.inspector.picker_hover = hover;
            window.request_redraw();
        }
        return;
    }

    let mut is_button = false;
    if let Some(hit_id) = hit_node_id {
        let mut temp_hit = Some(hit_id);
        while let Some(id) = temp_hit {
            if let Some(node_ref) = tab.dom.get(id) {
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
    elwt: &winit::event_loop::EventLoopWindowTarget<MizuUserEvent>,
    mouse: &mut MouseState,
    logical_width: f32,
) {
    let dropdown_count = {
        let tab = manager.active();
        if tab.chrome_state.focused {
            tab.chrome_state.suggestions.len()
        } else {
            0
        }
    };
    let layout = crate::render::chrome_vello::ChromeLayout {
        window_width: logical_width,
        tab_count: manager.tabs.len(),
        dropdown_count,
    };
    let zone = chrome_hit_zone(mouse.last_logical_x, mouse.last_logical_y, &layout);
    // The strip arms mutate the tab *list*, so they run before the split
    // borrow narrows `manager` to a single tab.
    match zone {
        ChromeHitZone::TabItem(i) => {
            if let Some(id) = manager.tabs.get(i).map(|t| t.id) {
                manager.switch_to_tab(id);
                retitle_window(manager, window);
            }
            window.request_redraw();
            return;
        }
        ChromeHitZone::TabCloseButton(i) => {
            if let Some(id) = manager.tabs.get(i).map(|t| t.id)
                && !manager.close_tab(id)
            {
                elwt.exit();
                return;
            }
            retitle_window(manager, window);
            window.request_redraw();
            return;
        }
        ChromeHitZone::NewTabButton => {
            if let Some(id) = manager.open_tab(BLANK_TAB_URL) {
                manager.switch_to_tab(id);
                retitle_window(manager, window);
            }
            window.request_redraw();
            return;
        }
        _ => {}
    }
    let (tab, mut ctx) = manager.split_active();
    match zone {
        ChromeHitZone::BackButton => {
            navigate_back(tab, &mut ctx);
        }
        ChromeHitZone::ForwardButton => {
            navigate_forward(tab, &mut ctx);
        }
        ChromeHitZone::ReloadButton => {
            tracing::debug!("reload button clicked");
            tab.chrome_state.loading = true;
            let url = tab.chrome_state.url.clone();
            navigate_to_url(tab, &mut ctx, url, NavigationInitiator::UserGesture);
        }
        ChromeHitZone::UrlBar => {
            let bar_left = url_text_left(logical_width);
            tab.chrome_state.focused = true;
            mouse.dragging_url_bar = true;
            {
                let cs = &mut tab.chrome_state;
                let fc = &mut ctx.font_cx;
                let lc = &mut ctx.layout_cx;
                cs.set_cursor_from_click(mouse.last_logical_x, bar_left, fc, lc);

                if mouse.click_count == 2 {
                    cs.select_word_at_cursor();
                } else if mouse.click_count >= 3 {
                    cs.select_all();
                }
            }
            if let Some(prev) = tab.focused_node.take() {
                // Re-render the blurred input (placeholder returns).
                tab.mark_text_dirty(prev);
            }
        }
        ChromeHitZone::HistoryButton => {
            manager.history_sidebar.toggle();
            window.request_redraw();
            return;
        }
        ChromeHitZone::AutocompleteSuggestion(i) => {
            let target_url = if let Some(record) = tab.chrome_state.suggestions.get(i) {
                record.url.clone()
            } else {
                tab.chrome_state.url.trim().to_string()
            };

            let mut url = target_url;
            if !url.is_empty() && !url.contains("://") {
                url = format!("mizu://{url}");
            }

            tab.chrome_state.url = url.clone();
            tab.chrome_state.cursor = tab.chrome_state.url.len();
            tab.chrome_state.selection = None;
            tab.chrome_state.focused = false;
            tab.chrome_state.suggestions.clear();
            tab.chrome_state.selected_suggestion = None;
            tab.chrome_state.inline_completion = None;

            tab.chrome_state.loading = true;
            navigate_to_url(tab, &mut ctx, url, NavigationInitiator::UserGesture);
        }
        ChromeHitZone::TabItem(_)
        | ChromeHitZone::TabCloseButton(_)
        | ChromeHitZone::NewTabButton => unreachable!("handled above"),
        ChromeHitZone::Background => {
            // Chrome background — just blur URL bar and DOM focus
            if tab.chrome_state.focused {
                tab.chrome_state.focused = false;
            }
            if let Some(prev) = tab.focused_node.take() {
                // Re-render the blurred input (placeholder returns).
                tab.mark_text_dirty(prev);
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
    let (tab, _ctx) = manager.split_active();
    if !(tab.inspector.open
        && mouse.last_logical_x >= crate::render::inspector::panel_left(logical_width))
    {
        return false;
    }
    let rows = {
        let src = tab.inspector_sources();
        crate::render::inspector::model::build_rows(&src, &tab.inspector)
    };
    let x = mouse.last_logical_x - crate::render::inspector::panel_left(logical_width);
    let y = mouse.last_logical_y - CHROME_HEIGHT;
    if crate::render::inspector::handle_panel_click(&mut tab.inspector, &rows, x, y) {
        window.request_redraw();
    }
    true
}

/// Handles a left-click while the element picker is active: selects the
/// node under the cursor instead of triggering its action. Returns `true`
/// when the picker was active (and therefore consumed the click).
fn dispatch_picker_click(
    manager: &mut MizuWindowManager,
    window: &Window,
    mouse: &MouseState,
) -> bool {
    let (tab, _ctx) = manager.split_active();
    if !(tab.inspector.open && tab.inspector.picker) {
        return false;
    }
    let hit = hit_test(
        &tab.dom,
        &tab.taffy,
        &tab.node_to_taffy_id,
        &tab.scroll_offsets,
        mouse.last_logical_x,
        mouse.last_logical_y - CHROME_HEIGHT + tab.root_scroll_offset_y,
    );
    if let Some(hit_id) = hit {
        tab.inspector.select_with_ancestors(&tab.dom, hit_id);
        // Bring the selection into view in the Elements tree.
        let rows = {
            let src = tab.inspector_sources();
            crate::render::inspector::model::build_rows(&src, &tab.inspector)
        };
        if let Some(idx) = rows.iter().position(|r| r.node == Some(hit_id)) {
            let logical_height = window.inner_size().height as f32 / window.scale_factor() as f32;
            let viewport_h =
                (logical_height - CHROME_HEIGHT - crate::render::inspector::TAB_BAR_HEIGHT)
                    .max(0.0);
            tab.inspector.scroll_to_row(idx, viewport_h);
        }
    }
    tab.inspector.set_picker(false);
    window.set_cursor_icon(winit::window::CursorIcon::Default);
    window.request_redraw();
    true
}

/// Handles a left-click on DOM content: updates focus, and dispatches
/// click/submit actions for the nearest ancestor carrying them.
fn dispatch_dom_click(manager: &mut MizuWindowManager, window: &Window, mouse: &MouseState) {
    let (tab, ctx) = manager.split_active();
    tab.chrome_state.focused = false;

    let hit_node_id = hit_test(
        &tab.dom,
        &tab.taffy,
        &tab.node_to_taffy_id,
        &tab.scroll_offsets,
        mouse.last_logical_x,
        mouse.last_logical_y - CHROME_HEIGHT + tab.root_scroll_offset_y,
    );
    let mut action_node_id = None;
    let mut submit_node_id = None;
    let mut new_focus = None;
    let mut file_input_id = None;
    let mut current_hit = hit_node_id;

    while let Some(id) = current_hit {
        if let Some(node_ref) = tab.dom.get(id) {
            if node_ref.value().primitive == crate::parser::Primitive::Input {
                // A `type "file"` input opens the native picker instead of
                // taking the text-caret focus — it has no typed-text state.
                if is_file_input(&tab.dom, id) {
                    file_input_id = Some(id);
                } else {
                    new_focus = Some(id);
                }
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

    if tab.focused_node != new_focus {
        // Re-render both inputs: the old one regains its
        // placeholder, the new one shows the caret.
        if let Some(prev) = tab.focused_node {
            tab.mark_text_dirty(prev);
        }
        if let Some(next) = new_focus {
            tab.mark_text_dirty(next);
        }
        tab.focused_node = new_focus;
        window.request_redraw();
    }

    if let Some(node_id) = file_input_id
        && dispatch_file_input_click(tab, node_id)
    {
        window.request_redraw();
    }

    if let Some(node_id) = action_node_id
        && dispatch_click_gesture(tab, ctx.logic_tx, node_id)
    {
        window.request_redraw();
    }

    // A click on a submit button gathers the enclosing form's
    // fields and forwards them to the logic worker.
    if let Some(submit_id) = submit_node_id
        && dispatch_form_submit(tab, ctx.logic_tx, submit_id)
    {
        window.request_redraw();
    }
}

/// Handles `WindowEvent::MouseInput` (left button pressed): routes to
/// exactly one of the chrome bar, the history sidebar, the inspector panel,
/// the element picker, or DOM content, in that priority order.
fn dispatch_mouse_pressed(
    manager: &mut MizuWindowManager,
    window: &Window,
    elwt: &winit::event_loop::EventLoopWindowTarget<MizuUserEvent>,
    mouse: &mut MouseState,
) {
    let scale = window.scale_factor() as f32;
    let logical_width = window.inner_size().width as f32 / scale;

    let now = std::time::Instant::now();
    let dx = mouse.last_logical_x - mouse.last_click_pos.map(|(x, _)| x).unwrap_or(-1000.0);
    let dy = mouse.last_logical_y - mouse.last_click_pos.map(|(_, y)| y).unwrap_or(-1000.0);
    let dist = (dx * dx + dy * dy).sqrt();

    if let Some(last_time) = mouse.last_click_time {
        if now.duration_since(last_time) < std::time::Duration::from_millis(500) && dist < 5.0 {
            mouse.click_count += 1;
        } else {
            mouse.click_count = 1;
        }
    } else {
        mouse.click_count = 1;
    }
    mouse.last_click_time = Some(now);
    mouse.last_click_pos = Some((mouse.last_logical_x, mouse.last_logical_y));

    let dropdown_count = {
        let tab = manager.active();
        if tab.chrome_state.focused {
            tab.chrome_state.suggestions.len()
        } else {
            0
        }
    };
    let dropdown_h = if dropdown_count > 0 {
        dropdown_count as f32 * 24.0 + 8.0
    } else {
        0.0
    };
    let url_bar_right = (logical_width - crate::render::chrome_vello::STATUS_W)
        .max(crate::render::chrome_vello::URL_BAR_X + 10.0);
    let in_dropdown = dropdown_count > 0
        && mouse.last_logical_y >= CHROME_HEIGHT
        && mouse.last_logical_y < CHROME_HEIGHT + dropdown_h
        && mouse.last_logical_x >= crate::render::chrome_vello::URL_BAR_X
        && mouse.last_logical_x < url_bar_right;

    if mouse.last_logical_y < CHROME_HEIGHT || in_dropdown {
        dispatch_chrome_click(manager, window, elwt, mouse, logical_width);
        return;
    }
    // The sidebar overlays the page, so it must claim clicks over its own
    // column before the inspector or the DOM see them.
    if manager.history_sidebar.open && dispatch_history_sidebar_click(manager, window, mouse) {
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

/// Handles a left-click inside the history sidebar panel.
///
/// Returns `true` when the click landed on the panel — including on inert
/// parts of it, so a click on the panel background can never fall through to
/// the page it is covering.
fn dispatch_history_sidebar_click(
    manager: &mut MizuWindowManager,
    window: &Window,
    mouse: &MouseState,
) -> bool {
    use crate::render::history_sidebar::{HistorySidebarHit, history_sidebar_hit};
    let hit = history_sidebar_hit(
        mouse.last_logical_x,
        mouse.last_logical_y,
        &manager.history_log,
        manager.history_sidebar.scroll_offset,
        CHROME_HEIGHT,
    );
    match hit {
        HistorySidebarHit::None => return false,
        HistorySidebarHit::Background => {}
        HistorySidebarHit::Clear => {
            manager.history_log.clear();
            // Persisted immediately rather than at exit: "clear my
            // history" must survive a crash, or it did not happen.
            manager.history_log.save_to_disk();
            manager.history_sidebar.hovered = None;
        }
        HistorySidebarHit::Entry(index) => {
            // Copy the URL out before the split borrow: navigation needs
            // `&mut` on the manager, which the log borrow would block.
            let url = manager
                .history_log
                .get(index)
                .map(|record| record.url.clone());
            if let Some(url) = url {
                // The panel stays open, so a mis-clicked entry costs one
                // more click rather than a re-open — the sidebar is a list
                // to browse, not a menu that dismisses itself.
                let (tab, mut ctx) = manager.split_active();
                navigate_to_url(tab, &mut ctx, url, NavigationInitiator::UserGesture);
            }
        }
    }
    window.request_redraw();
    true
}

/// Builds the strip's view of the open tabs.
///
/// Titles come from each document's `title` attribute, falling back to the
/// URL and then to a generic label. They are run through
/// [`crate::render::bidi::strip_bidi_overrides`] for the same reason the URL
/// bar is: a document-controlled title carrying an RLO override could
/// otherwise repaint itself over a neighbouring tab's label, which is a
/// spoofing vector, not a cosmetic bug.
fn tab_strip_entries(
    manager: &MizuWindowManager,
) -> Vec<crate::render::chrome_vello::TabStripEntry> {
    let active = manager.active().id;
    manager
        .tabs
        .iter()
        .map(|t| {
            let raw = t
                .dom
                .root()
                .value()
                .attributes
                .get("title")
                .cloned()
                .unwrap_or_else(|| {
                    if t.chrome_state.url.is_empty() {
                        "New Tab".to_string()
                    } else {
                        t.chrome_state.url.clone()
                    }
                });
            crate::render::chrome_vello::TabStripEntry {
                title: crate::render::bidi::strip_bidi_overrides(&raw).into_owned(),
                active: t.id == active,
            }
        })
        .collect()
}

/// URL a freshly opened tab starts on. Not a network target — the blank
/// document is built locally by `open_tab`.
const BLANK_TAB_URL: &str = "about:blank";

/// Sets the OS window title from the active tab's document title.
///
/// Only the active tab may retitle the window; a background tab finishing a
/// load must not rename what the user is looking at.
fn retitle_window(manager: &MizuWindowManager, window: &Window) {
    let title = manager
        .active()
        .dom
        .root()
        .value()
        .attributes
        .get("title")
        .cloned()
        .unwrap_or_else(|| "Mizu Navigator".to_string());
    window.set_title(&title);
}

/// Handles the tab-management shortcuts, before any focus-sensitive handler
/// sees the key.
///
/// Ordering matters: `handle_focused_node_key`'s text-input arm would
/// otherwise treat a Ctrl-chord as typed text, so these must be consumed
/// first. Returns `true` when the key was a tab shortcut.
fn handle_tab_shortcuts(
    manager: &mut MizuWindowManager,
    window: &Window,
    elwt: &winit::event_loop::EventLoopWindowTarget<MizuUserEvent>,
    key_event: &winit::event::KeyEvent,
) -> bool {
    if !manager.modifiers.control_key() {
        return false;
    }
    let shift = manager.modifiers.shift_key();
    match &key_event.logical_key {
        Key::Character(c) if c.as_str().eq_ignore_ascii_case("h") => {
            // Ctrl+H: toggle the history sidebar, as in Firefox and Edge.
            manager.history_sidebar.toggle();
            window.request_redraw();
            return true;
        }
        Key::Character(c) if c.as_str().eq_ignore_ascii_case("t") => {
            if let Some(id) = manager.open_tab(BLANK_TAB_URL) {
                manager.switch_to_tab(id);
                retitle_window(manager, window);
            }
        }
        Key::Character(c) if c.as_str().eq_ignore_ascii_case("w") => {
            let active = manager.active().id;
            if !manager.close_tab(active) {
                elwt.exit();
                return true;
            }
            retitle_window(manager, window);
        }
        Key::Character(c)
            if c.as_str().len() == 1 && c.as_str().chars().all(|ch| ch.is_ascii_digit()) =>
        {
            let d = c.as_str().as_bytes()[0] - b'0';
            if d == 0 {
                return false;
            }
            // Browser convention: Ctrl+9 is "last tab", not "ninth tab".
            let idx = if d == 9 {
                manager.tabs.len() - 1
            } else {
                (d as usize - 1).min(manager.tabs.len().saturating_sub(1))
            };
            if d != 9 && d as usize > manager.tabs.len() {
                return false;
            }
            let id = manager.tabs[idx].id;
            manager.switch_to_tab(id);
            retitle_window(manager, window);
        }
        Key::Named(NamedKey::Tab) => {
            let n = manager.tabs.len();
            let cur = manager.active_tab_index();
            let next = if shift {
                (cur + n - 1) % n
            } else {
                (cur + 1) % n
            };
            let id = manager.tabs[next].id;
            manager.switch_to_tab(id);
            retitle_window(manager, window);
        }
        _ => return false,
    }
    window.request_redraw();
    true
}

/// Handles the global keyboard shortcuts that apply regardless of focus:
/// F12 (toggle inspector) and Alt+Left/Right (history back/forward). Returns
/// `true` if the key was one of these (caller should stop processing).
fn handle_global_key_shortcuts(
    manager: &mut MizuWindowManager,
    window: &Window,
    key_event: &winit::event::KeyEvent,
) -> bool {
    let (tab, mut ctx) = manager.split_active();
    if let Key::Named(NamedKey::F12) = key_event.logical_key {
        tab.inspector.toggle();
        let physical_size = window.inner_size();
        let scale = window.scale_factor() as f32;
        if let Err(e) = resize_tab_viewport(
            tab,
            &mut ctx,
            physical_size.width as f32 / scale,
            physical_size.height as f32 / scale,
            None,
        ) {
            tracing::error!("layout recalculation failed on inspector toggle: {e}");
        }
        window.request_redraw();
        return true;
    }

    // Alt+Left / Alt+Right: history Back/Forward, regardless of focus (the
    // platform-standard shortcuts).
    if ctx.modifiers.alt_key() {
        match key_event.logical_key {
            Key::Named(NamedKey::ArrowLeft) => {
                navigate_back(tab, &mut ctx);
                window.request_redraw();
                return true;
            }
            Key::Named(NamedKey::ArrowRight) => {
                navigate_forward(tab, &mut ctx);
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
    {
        let tab = manager.active();
        if !tab.chrome_state.focused {
            return false;
        }
    }

    let url_before = manager.active().chrome_state.url.clone();

    let action = {
        let (tab, ctx) = manager.split_active();
        let cs = &mut tab.chrome_state;
        cs.handle_key(
            &key_event.logical_key,
            key_event.text.as_deref(),
            ctx.modifiers,
        )
    };

    match action {
        ChromeKeyAction::Navigate(url) => {
            let (tab, mut ctx) = manager.split_active();
            tab.chrome_state.loading = true;
            navigate_to_url(tab, &mut ctx, url, NavigationInitiator::UserGesture);
        }
        ChromeKeyAction::Reload => {
            let (tab, mut ctx) = manager.split_active();
            let url = tab.chrome_state.url.clone();
            tab.chrome_state.loading = true;
            navigate_to_url(tab, &mut ctx, url, NavigationInitiator::UserGesture);
        }
        ChromeKeyAction::Back => {
            let (tab, mut ctx) = manager.split_active();
            navigate_back(tab, &mut ctx);
        }
        ChromeKeyAction::Copy => {
            let tab = manager.active();
            if let Some(text) = tab.chrome_state.copy_text()
                && let Ok(mut cb) = arboard::Clipboard::new()
            {
                let _ = cb.set_text(text);
            }
        }
        ChromeKeyAction::Cut => {
            let tab = manager.active_mut();
            if let Some(text) = tab.chrome_state.cut_text()
                && let Ok(mut cb) = arboard::Clipboard::new()
            {
                let _ = cb.set_text(text);
            }
        }
        ChromeKeyAction::Paste => {
            let tab = manager.active_mut();
            if let Ok(mut cb) = arboard::Clipboard::new()
                && let Ok(text) = cb.get_text()
            {
                tab.chrome_state.paste_text(&text);
            }
        }
        ChromeKeyAction::Handled | ChromeKeyAction::Ignored => {}
    }

    let url_after = manager.active().chrome_state.url.clone();
    if url_before != url_after {
        let suggestions = manager.history_log.autocomplete(&url_after, 6);
        let tab = manager.active_mut();
        let cs = &mut tab.chrome_state;
        cs.suggestions = suggestions;
        cs.selected_suggestion = None;

        if let Some(first) = cs.suggestions.first() {
            let url_lower = first.url.to_lowercase();
            let query_lower = url_after.to_lowercase();

            let without_scheme = url_lower
                .strip_prefix("mizu://")
                .or_else(|| url_lower.strip_prefix("file://"))
                .or_else(|| url_lower.strip_prefix("https://"))
                .or_else(|| url_lower.strip_prefix("http://"))
                .unwrap_or(&url_lower);

            if url_lower.starts_with(&query_lower) {
                cs.inline_completion = Some(first.url[url_after.len()..].to_string());
            } else if without_scheme.starts_with(&query_lower) {
                let scheme_len = first.url.len() - without_scheme.len();
                cs.inline_completion = Some(first.url[scheme_len + url_after.len()..].to_string());
            } else {
                cs.inline_completion = None;
            }
        } else {
            cs.inline_completion = None;
        }
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
    let (tab, ctx) = manager.split_active();
    if !matches!(key_event.logical_key, Key::Named(NamedKey::Tab)) {
        return false;
    }
    let backward = ctx.modifiers.shift_key();
    if let Some(next) = tab.next_focus_target(backward)
        && tab.focused_node != Some(next)
    {
        if let Some(prev) = tab.focused_node {
            tab.mark_text_dirty(prev);
        }
        tab.mark_text_dirty(next);
        tab.focused_node = Some(next);
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
    let (tab, ctx) = manager.split_active();
    let Some(focus_id) = tab.focused_node else {
        return false;
    };
    let Some(&input_u32) = tab.node_id_to_u32.get(&focus_id) else {
        return false;
    };
    let is_input = tab
        .dom
        .get(focus_id)
        .map(|n| n.value().primitive == crate::parser::Primitive::Input)
        .unwrap_or(false);

    match &key_event.logical_key {
        Key::Named(NamedKey::Escape) => {
            // Blur the focused node; Escape only exits the
            // app / closes pickers when nothing is focused.
            tab.focused_node = None;
            tab.mark_text_dirty(focus_id);
            window.request_redraw();
        }
        Key::Named(NamedKey::Backspace) if is_input => {
            if let Some(buf) = tab.local_inputs.get_mut(&input_u32)
                && buf.pop().is_some()
            {
                tab.mark_text_dirty(focus_id);
                window.request_redraw();
            }
        }
        Key::Named(NamedKey::Enter) if is_input => {
            // Enter submits the enclosing form, exactly
            // like clicking its submit button.
            if let Some(submitter) = find_form_submitter(&tab.dom, focus_id)
                && dispatch_form_submit(tab, ctx.logic_tx, submitter)
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
            let (action_node_id, submit_node_id) = find_click_and_submit(&tab.dom, focus_id);
            let mut redraw = false;
            if let Some(node_id) = action_node_id
                && dispatch_click_gesture(tab, ctx.logic_tx, node_id)
            {
                redraw = true;
            }
            if let Some(submit_id) = submit_node_id
                && dispatch_form_submit(tab, ctx.logic_tx, submit_id)
            {
                redraw = true;
            }
            if redraw {
                window.request_redraw();
            }
        }
        _ if is_input => {
            let is_paste = ctx.modifiers.control_key()
                && matches!(
                    &key_event.logical_key,
                    Key::Character(c) if c.eq_ignore_ascii_case("v")
                );
            if is_paste {
                if let Ok(mut cb) = arboard::Clipboard::new()
                    && let Ok(text) = cb.get_text()
                {
                    let buf = tab.local_inputs.entry(input_u32).or_default();
                    if push_input_text(buf, &text) {
                        tab.mark_text_dirty(focus_id);
                        window.request_redraw();
                    }
                }
            } else if !ctx.modifiers.control_key()
                && !ctx.modifiers.alt_key()
                && !ctx.modifiers.super_key()
                && let Some(text) = key_event.text.as_deref()
            {
                let buf = tab.local_inputs.entry(input_u32).or_default();
                if push_input_text(buf, text) {
                    tab.mark_text_dirty(focus_id);
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
    // Dismissing an open overlay comes before anything else Escape does —
    // and long before it reaches the branch that quits the browser.
    if manager.history_sidebar.open {
        manager.history_sidebar.close();
        window.request_redraw();
        return;
    }
    let (tab, mut ctx) = manager.split_active();
    if tab.inspector.picker {
        tab.inspector.set_picker(false);
        window.set_cursor_icon(winit::window::CursorIcon::Default);
        window.request_redraw();
    } else if tab.inspector.open {
        tab.inspector.toggle();
        let physical_size = window.inner_size();
        let scale = window.scale_factor() as f32;
        if let Err(e) = resize_tab_viewport(
            tab,
            &mut ctx,
            physical_size.width as f32 / scale,
            physical_size.height as f32 / scale,
            None,
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
    let (_tab, _ctx) = manager.split_active();
    if !key_event.state.is_pressed() {
        return;
    }
    if handle_tab_shortcuts(manager, window, elwt, key_event) {
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

    // ── Wheel over the history sidebar scrolls its content ────
    if manager.history_sidebar.open
        && crate::render::history_sidebar::contains_x(mouse.last_logical_x)
        && mouse.last_logical_y >= CHROME_HEIGHT
    {
        let logical_height = window.inner_size().height as f32 / scale;
        manager.history_sidebar.scroll_offset = crate::render::history_sidebar::scroll_by(
            manager.history_sidebar.scroll_offset,
            delta_y,
            &manager.history_log,
            logical_height,
            CHROME_HEIGHT,
        );
        window.request_redraw();
        return;
    }

    let (tab, mut ctx) = manager.split_active();

    // ── Wheel over the inspector panel scrolls its content ────
    if tab.inspector.open {
        let logical_width = window.inner_size().width as f32 / scale;
        if mouse.last_logical_x >= crate::render::inspector::panel_left(logical_width)
            && mouse.last_logical_y >= CHROME_HEIGHT
        {
            tab.inspector.scroll_by(delta_y * 2.0);
            window.request_redraw();
            return;
        }
    }

    let mut candidate = hit_test(
        &tab.dom,
        &tab.taffy,
        &tab.node_to_taffy_id,
        &tab.scroll_offsets,
        mouse.last_logical_x,
        mouse.last_logical_y - CHROME_HEIGHT + tab.root_scroll_offset_y,
    );

    let mut scrolled = false;
    while let Some(node_id) = candidate {
        let is_scroll = tab
            .dom
            .get(node_id)
            .and_then(|n| {
                n.value().attributes.get("class").and_then(|cls| {
                    let cls_name = cls.strip_prefix('.').unwrap_or(cls);
                    tab.style_rules.get(cls_name)
                })
            })
            .map(|rules| rules.overflow == MizuOverflow::Scroll)
            .unwrap_or(false);

        if is_scroll {
            let max_scroll = if let Some(&t_id) = tab.node_to_taffy_id.get(&node_id) {
                if let Ok(container_layout) = tab.taffy.layout(t_id) {
                    let container_h = container_layout.size.height;
                    let mut content_h: f32 = 0.0;
                    if let Some(node_ref) = tab.dom.get(node_id) {
                        for child in node_ref.children() {
                            if let Some(&c_t_id) = tab.node_to_taffy_id.get(&child.id())
                                && let Ok(child_layout) = tab.taffy.layout(c_t_id)
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

            let current = tab.scroll_offsets.get(&node_id).copied().unwrap_or(0.0);
            let new_offset = (current + delta_y).clamp(0.0, max_scroll);
            tab.scroll_offsets.insert(node_id, new_offset);
            scrolled = true;
            break;
        }

        candidate = tab
            .dom
            .get(node_id)
            .and_then(|n| n.parent().map(|p| p.id()));
    }

    if scrolled {
        window.request_redraw();
    } else if mouse.last_logical_y >= CHROME_HEIGHT {
        // No scrollable DOM container — scroll root document
        let phys = window.inner_size();
        let sf = window.scale_factor() as f32;
        let viewport_h = phys.height as f32 / sf - CHROME_HEIGHT;
        let content_h = tab
            .taffy
            .layout(tab.root_taffy_id)
            .map(|l| l.size.height)
            .unwrap_or(0.0);
        let max_scroll = (content_h - viewport_h).max(0.0);
        tab.root_scroll_offset_y = (tab.root_scroll_offset_y + delta_y).clamp(0.0, max_scroll);
        // Cheap no-op in the common case (small scroll deltas well inside the
        // already-expanded window); only pays for a re-expansion once the
        // scroll position has moved far enough to need one. See
        // `MizuWindowManager::refresh_virtualized_windows`.
        if let Err(e) = refresh_tab_virtualized_windows(tab, &mut ctx, viewport_h) {
            tracing::error!("virtualized Each re-expansion failed during scroll: {e}");
        }
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
        let chrome_url_snapshot = tab.chrome_state.url.clone();
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
        crate::core::types::Value::Int(
            (tab.root_scroll_offset_y as f64 * crate::core::types::DECIMAL_SCALE as f64).round()
                as i64,
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
        // Route to the tab that issued the command. A background tab
        // finishing a navigation must replace *its own* document, never the
        // one the user is looking at; an unknown id means the tab closed
        // while the request was in flight, and the result is dropped.
        let target = network_result_tab(&res).unwrap_or_else(|| manager.active().id);
        let Some((tab, mut ctx)) = manager.split_tab(target) else {
            tracing::debug!(tab = target.0, "network result for closed tab; dropped");
            continue;
        };
        // Image completions fan out to every tab waiting on that URL, so the
        // waiter list has to be read before the result is consumed.
        let image_url = match &res {
            crate::network::NetworkResult::FetchImageSuccess { url, .. }
            | crate::network::NetworkResult::FetchImageFailed { url, .. } => Some(url.clone()),
            _ => None,
        };
        process_network_result(tab, &mut ctx, res);
        if let Some(url) = image_url {
            notify_image_waiters(manager, &url, target);
        }
    }
}

/// Relays out every *other* tab that was waiting on `url`'s image.
///
/// The requesting tab is handled inline by `process_network_result`; the rest
/// were deduped at request time against the shared, URL-keyed decoded-image
/// cache and would otherwise keep the layout they built while the slot was
/// still `Loading`.
fn notify_image_waiters(
    manager: &mut MizuWindowManager,
    url: &str,
    requester: crate::network::TabId,
) {
    let Some(waiters) = manager.fetching_images.remove(url) else {
        return;
    };
    for waiter in waiters {
        if waiter == requester {
            continue;
        }
        let Some((tab, mut ctx)) = manager.split_tab(waiter) else {
            continue;
        };
        super::navigate::rebuild_tab_taffy_after_image(tab, &mut ctx);
        tab.layout_stale = true;
    }
}

/// The tab a network result belongs to, or `None` for worker-startup failures
/// that predate any command (see [`crate::network::NetworkResult::Error`]).
fn network_result_tab(res: &crate::network::NetworkResult) -> Option<crate::network::TabId> {
    use crate::network::NetworkResult as R;
    match res {
        R::Success { tab, .. }
        | R::FetchFailed { tab, .. }
        | R::NavigateSuccess { tab, .. }
        | R::NavigationRedirect { tab, .. }
        | R::FetchImageSuccess { tab, .. }
        | R::FetchImageFailed { tab, .. } => Some(*tab),
        R::Error(tab, _) => *tab,
    }
}

/// Drains the logic worker's response channel, applying mutated variables
/// and dispatching runtime actions (navigate/clipboard/capability actions).
/// Returns whether any variable changed and which symbols changed, so the
/// caller can decide whether a layout recompute is needed.
fn drain_logic_worker_results(manager: &mut MizuWindowManager) -> (bool, Vec<Symbol>) {
    let mut state_changed = false;
    let mut mutated_symbols = Vec::new();
    // Collect before processing: the split borrow below needs exclusive
    // access to `manager`, so the channel drain has to finish first (same
    // reason `drain_network_results` collects into a `Vec`).
    let responses: Vec<_> = std::iter::from_fn(|| manager.logic_rx.try_recv().ok()).collect();
    let active_id = manager.active().id;
    for (tab_id, res) in responses {
        // Route by the id the worker echoed back, never to the active tab: a
        // response carries `Symbol`s that are only meaningful against the
        // interner of the tab that produced them, and its mutations must land
        // in that tab's store even if the user switched away meanwhile. An
        // unknown id means the tab was closed while the response was in
        // flight — drop it.
        let Some((tab, mut ctx)) = manager.split_tab(tab_id) else {
            tracing::debug!(tab = tab_id.0, "worker response for closed tab; dropped");
            continue;
        };
        let is_active = tab_id == active_id;
        match res {
            Ok(response) => {
                for (sym, val) in response.state_update.mutated_variables {
                    let name_str = tab.store.interner.resolve(sym).unwrap_or("<unknown>");
                    tab.inspector_log.push_event(
                        crate::render::inspector::log::EventKind::Mutation,
                        format!("{name_str} = {val}"),
                    );
                    tab.store.evaluator.set_global(sym, val);
                    tab.recent_mutations.insert(sym, std::time::Instant::now());
                    if is_active {
                        state_changed = true;
                        mutated_symbols.push(sym);
                    } else {
                        // Relaid out on switch; a background tab paints nothing
                        // now, so doing the work now would be wasted.
                        tab.layout_stale = true;
                    }
                }
                for action in response.runtime_actions {
                    if let crate::network::RuntimeAction::Navigate { url } = &action {
                        // N2+N3: Navigate actions go through the choke point;
                        // capture the current gesture flag so cross-origin
                        // logic-driven navigation is blocked without a click.
                        tab.chrome_state.loading = true;
                        let url = url.clone();
                        let initiator = if tab.has_user_gesture {
                            NavigationInitiator::UserGesture
                        } else {
                            NavigationInitiator::DocumentLogic
                        };
                        navigate_to_url(tab, &mut ctx, url, initiator);
                    } else if let crate::network::RuntimeAction::CopyToClipboard { node_id } =
                        &action
                    {
                        // Clipboard is intercepted here (not in execute_capability_action)
                        // so we can enforce the user-gesture gate and do DOM lookup.
                        let node_id = node_id.clone();
                        match apply_clipboard_action(
                            &node_id,
                            &tab.dom,
                            &tab.local_inputs,
                            &tab.node_id_to_u32,
                            &tab.store,
                            tab.has_user_gesture,
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
                        execute_tab_capability_action(tab, &ctx, action);
                    }
                }
                // User-gesture activation is transitory: consume it after each
                // action batch so subsequent batches without a click are blocked.
                tab.has_user_gesture = false;
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
fn recompute_dirty_layout(
    manager: &mut MizuWindowManager,
    window: &Window,
    mutated_symbols: Vec<Symbol>,
) {
    let (tab, mut ctx) = manager.split_active();
    tab.setup_timers();

    let mut layout_dirty = tab.typing_layout_dirty;
    tab.typing_layout_dirty = false;

    // Resolve mutated symbol names for the Each-granularity check below.
    // Only allocate if there are actually mutated symbols; the common idle
    // case pays zero cost.
    let mut dirty_list_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for sym in &mutated_symbols {
        // Check if any Each node's backing list variable matches this symbol.
        if let Some(name) = tab.store.interner.resolve(*sym) {
            for node in tab.dom.nodes() {
                if node.value().primitive == crate::parser::Primitive::Each {
                    if let Some((_, list_name)) = &node.value().iterator_context {
                        if list_name == name {
                            dirty_list_names.insert(list_name.clone());
                        }
                    }
                }
            }
        }
    }

    for sym in mutated_symbols {
        if let Some(nodes) = tab.dependency_index.get(&sym) {
            for &node_id in nodes {
                tab.dirty_nodes.insert(node_id);

                let current_width = if let Some(&taffy_node) = tab.node_to_taffy_id.get(&node_id) {
                    if let Ok(layout) = tab.taffy.layout(taffy_node) {
                        Some(layout.size.width)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let old_dims = tab.text_dimensions.get(&node_id).copied();

                let render_env = crate::render::responsive::RenderEnvironment {
                    viewport: tab.viewport_size,
                    color_scheme: ctx.preferences.color_scheme,
                };
                if let Some((new_dims, layout)) = crate::render::text_engine::calculate_node_text(
                    node_id,
                    current_width,
                    &mut crate::render::text_engine::TextLayoutContext {
                        dom: &tab.dom,
                        style_rules: &tab.style_rules,
                        font_cx: ctx.font_cx,
                        layout_cx: ctx.layout_cx,
                        store: &tab.store,
                        local_inputs: &tab.local_inputs,
                        node_id_to_u32: &tab.node_id_to_u32,
                        focused_input: tab.focused_node,
                        style_variants: &tab.style_variants,
                        render_env: &render_env,
                    },
                ) {
                    tab.text_layouts.insert(node_id, layout);
                    tab.text_dimensions.insert(node_id, new_dims);
                    tab.dirty_nodes.remove(&node_id);

                    let dimensions_changed = match old_dims {
                        Some(old) => {
                            (old.0 - new_dims.0).abs() > f32::EPSILON
                                || (old.1 - new_dims.1).abs() > f32::EPSILON
                        }
                        None => true,
                    };

                    if dimensions_changed
                        && let Some(&taffy_node) = tab.node_to_taffy_id.get(&node_id)
                    {
                        let _ = tab.taffy.mark_dirty(taffy_node);
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
        // Pass the set of dirty list names so resize_viewport → expand_each_nodes
        // only rebuilds the affected Each blocks instead of all of them.
        let dirty_lists = if dirty_list_names.is_empty() {
            None
        } else {
            Some(dirty_list_names)
        };
        if let Err(e) =
            resize_tab_viewport(tab, &mut ctx, logical_width, logical_height, dirty_lists)
        {
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
    // Hoisted before `split_active` borrows `manager`: these are window-level
    // fields not carried in `WindowCtx`.
    let pending_resize = manager.pending_resize;
    let last_layout_time = manager.last_layout_time;
    let mut new_last_layout_time = None;
    let mut clear_pending_resize = false;
    // Hoisted for the same reason: read across all tabs, before the split
    // borrow narrows `manager` to one of them.
    let loading = manager.tabs.iter().any(|t| t.chrome_state.loading);
    let (tab, mut ctx) = manager.split_active();
    let now = std::time::Instant::now();
    let mut redraw = false;
    let mut next_wakeup = tab.root_timer_queue.keys().next().copied();

    if let Some((w, h)) = pending_resize {
        let elapsed = now.duration_since(last_layout_time);
        if elapsed >= std::time::Duration::from_millis(16) {
            if let Err(e) = resize_tab_viewport(tab, &mut ctx, w, h, None) {
                tracing::error!("throttled layout recalculation failed: {e}");
            }
            new_last_layout_time = Some(now);
            clear_pending_resize = true;
            redraw = true;
        } else {
            let wake_time = last_layout_time + std::time::Duration::from_millis(16);
            next_wakeup = Some(next_wakeup.map(|t| t.min(wake_time)).unwrap_or(wake_time));
        }
    }

    let mut timers_fired = false;

    // Root `timer` declarations fire on the same clock; the action is
    // dispatched to the logic worker by declaration index.
    //
    // Only the active tab's queue is walked here; every other tab's is walked
    // by `fire_background_timers` after this borrow ends. A background
    // document's timers keep running (its state must stay live while the user
    // is elsewhere) — the known cost is that the event loop never idles longer
    // than the shortest timer across all tabs. Throttling background timers,
    // as real browsers do, is deliberately left out of this change.
    while let Some(&deadline) = tab.root_timer_queue.keys().next() {
        if now >= deadline {
            if let Some(indices) = tab.root_timer_queue.remove(&deadline) {
                for idx in indices {
                    let interval = match tab.root_timers.get(idx) {
                        Some(rt) => tab.resolve_root_timer_interval(&rt.interval),
                        None => continue,
                    };
                    let _ = ctx
                        .logic_tx
                        .send((tab.id, UiEvent::RootTimer { index: idx as u32 }));
                    timers_fired = true;
                    if tab.inspector.open {
                        tab.inspector_log.push_event(
                            crate::render::inspector::log::EventKind::Timer,
                            format!("root timer #{idx}"),
                        );
                    }
                    if let Some(interval_ms) = interval {
                        let next_deadline = now + std::time::Duration::from_millis(interval_ms);
                        tab.root_timer_queue
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
        if let Err(e) = resize_tab_viewport(tab, &mut ctx, logical_width, logical_height, None) {
            tracing::error!("layout recalculation failed after timer: {e}");
        }
        window.request_redraw();
    }

    if let Some(&t) = tab.root_timer_queue.keys().next() {
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
    if tab.inspector.open
        && matches!(
            tab.inspector.tab,
            crate::render::inspector::InspectorTab::Events
                | crate::render::inspector::InspectorTab::Logic
        )
    {
        if now.duration_since(tab.inspector.last_events_refresh)
            >= std::time::Duration::from_millis(500)
        {
            tab.inspector.last_events_refresh = now;
            window.request_redraw();
        }
        let tick = tab.inspector.last_events_refresh + std::time::Duration::from_millis(500);
        next_wakeup = Some(next_wakeup.map(|w| w.min(tick)).unwrap_or(tick));
    }

    // While a network fetch is in flight, poll every 16 ms so the
    // try_recv drain fires regularly and the UI stays responsive. Checked
    // across all tabs, not just the visible one: a background tab's response
    // still has to be drained promptly, or its document silently stalls.
    let any_loading = loading;
    if any_loading {
        let poll_deadline = std::time::Instant::now() + std::time::Duration::from_millis(16);
        next_wakeup = Some(
            next_wakeup
                .map(|d: std::time::Instant| d.min(poll_deadline))
                .unwrap_or(poll_deadline),
        );
    }

    // The split borrow of `manager` ends here, so the window-level throttle
    // bookkeeping hoisted above can finally be written back.
    if let Some(t) = new_last_layout_time {
        manager.last_layout_time = t;
    }
    if clear_pending_resize {
        manager.pending_resize = None;
    }

    if let Some((fired, bg_deadline)) = fire_background_timers(manager, now) {
        timers_fired |= fired;
        if let Some(d) = bg_deadline {
            next_wakeup = Some(next_wakeup.map(|w| w.min(d)).unwrap_or(d));
        }
    }
    if timers_fired {
        let drain_at = now + std::time::Duration::from_millis(16);
        next_wakeup = Some(next_wakeup.map(|w| w.min(drain_at)).unwrap_or(drain_at));
    }

    if let Some(deadline) = next_wakeup {
        elwt.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(deadline));
    } else {
        elwt.set_control_flow(winit::event_loop::ControlFlow::Wait);
    }
}

/// Floor on a background tab's root-timer period.
///
/// A hidden document's `timer 100ms` would otherwise keep the event loop
/// waking 10x a second per background tab, painting nothing. Browsers clamp
/// background timers for exactly this reason; the timer still fires, just no
/// faster than this. The active tab is never clamped.
const BACKGROUND_TIMER_MIN_MS: u64 = 1000;

/// The period a background tab's timer is rescheduled at.
pub(super) fn background_timer_period(interval_ms: u64) -> u64 {
    interval_ms.max(BACKGROUND_TIMER_MIN_MS)
}

/// Fires due root timers for every **background** tab and reports the earliest
/// deadline still outstanding across them.
///
/// Split from `schedule_next_wakeup`'s active-tab walk because that walk runs
/// inside a `split_active` borrow; this one re-borrows a different tab per
/// iteration. Returns `(any_timer_fired, earliest_pending_deadline)`.
fn fire_background_timers(
    manager: &mut MizuWindowManager,
    now: std::time::Instant,
) -> Option<(bool, Option<std::time::Instant>)> {
    let active = manager.active().id;
    let ids: Vec<_> = manager
        .tabs
        .iter()
        .map(|t| t.id)
        .filter(|id| *id != active)
        .collect();
    if ids.is_empty() {
        return None;
    }
    let mut fired = false;
    let mut earliest: Option<std::time::Instant> = None;
    for id in ids {
        let Some((tab, ctx)) = manager.split_tab(id) else {
            continue;
        };
        while let Some(&deadline) = tab.root_timer_queue.keys().next() {
            if now < deadline {
                break;
            }
            let Some(indices) = tab.root_timer_queue.remove(&deadline) else {
                break;
            };
            for idx in indices {
                let interval = match tab.root_timers.get(idx) {
                    Some(rt) => tab.resolve_root_timer_interval(&rt.interval),
                    None => continue,
                };
                let _ = ctx
                    .logic_tx
                    .send((tab.id, UiEvent::RootTimer { index: idx as u32 }));
                fired = true;
                if let Some(interval_ms) = interval {
                    let throttled = background_timer_period(interval_ms);
                    let next_deadline = now + std::time::Duration::from_millis(throttled);
                    tab.root_timer_queue
                        .entry(next_deadline)
                        .or_default()
                        .push(idx);
                }
            }
        }
        if let Some(&t) = tab.root_timer_queue.keys().next() {
            earliest = Some(earliest.map(|e: std::time::Instant| e.min(t)).unwrap_or(t));
        }
    }
    Some((fired, earliest))
}

/// Handles `Event::AboutToWait`: drains network/logic worker results,
/// recomputes dirty layout if anything changed, then schedules the next
/// wakeup.
fn dispatch_about_to_wait(
    manager: &mut MizuWindowManager,
    window: &Window,
    elwt: &winit::event_loop::EventLoopWindowTarget<MizuUserEvent>,
) {
    // Pure orchestration: each callee takes `&mut manager` and does its own
    // `split_active` internally, so this function must not hold a split
    // borrow across them.
    drain_network_results(manager);
    let (state_changed, mutated_symbols) = drain_logic_worker_results(manager);
    if state_changed || manager.active().typing_layout_dirty {
        recompute_dirty_layout(manager, window, mutated_symbols);
    }
    // Throttled internally: a no-op on all but a handful of idle ticks.
    manager.history_log.autosave();
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
    let (tab, ctx) = manager.split_active();
    match ak_event.window_event {
        accesskit_winit::WindowEvent::InitialTreeRequested => {
            a11y_adapter.update_if_active(|| {
                build_a11y_tree(
                    tab.a11y_epoch,
                    &tab.dom,
                    &tab.node_id_to_u32,
                    tab.focused_node,
                    &tab.store,
                )
            });
        }
        accesskit_winit::WindowEvent::ActionRequested(request) => {
            // SECURITY (ux-2 guardrail): an AT-initiated action is a
            // real user gesture — route it through the *same*
            // gesture-gated dispatch keyboard activation (ux-1) uses,
            // never a second path into the evaluator.
            let Some(ego_id) = resolve_ego_id(tab.a11y_epoch, &tab.u32_to_node_id, request.target)
            else {
                return;
            };
            let mut redraw = false;
            match request.action {
                accesskit::Action::Focus => {
                    if tab.focused_node != Some(ego_id) {
                        if let Some(prev) = tab.focused_node {
                            tab.mark_text_dirty(prev);
                        }
                        tab.mark_text_dirty(ego_id);
                        tab.focused_node = Some(ego_id);
                        redraw = true;
                    }
                }
                accesskit::Action::Default => {
                    let (action_node_id, submit_node_id) = find_click_and_submit(&tab.dom, ego_id);
                    if let Some(node_id) = action_node_id
                        && dispatch_click_gesture(tab, ctx.logic_tx, node_id)
                    {
                        redraw = true;
                    }
                    if let Some(submit_id) = submit_node_id
                        && dispatch_form_submit(tab, ctx.logic_tx, submit_id)
                    {
                        redraw = true;
                    }
                }
                _ => {}
            }
            if redraw && let Some(window) = ctx.window.as_ref() {
                window.request_redraw();
            }
        }
        accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
    }
}
