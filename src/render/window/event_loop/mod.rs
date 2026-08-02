//! `run_window_loop`, the Winit event loop, and the shared `MouseState`/`InitialDocument` types.
//!
//! Split by dispatch phase: [`mouse`] and [`keyboard`] (WindowEvent input),
//! [`redraw`] (WindowEvent::RedrawRequested), [`idle`] (AboutToWait), and
//! [`a11y_dispatch`] (the Accesskit UserEvent). Every item those files need
//! to be reachable from here is re-exported via `pub(super) use`.

use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::sync::Arc;

use ego_tree::Tree;
use vello::{Renderer, RendererOptions, util::RenderContext};
use winit::{
    event::{Event, WindowEvent},
    window::WindowBuilder,
};

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol};
use crate::parser::logic::{ComputedBinding, MizuFunction, RootTimer};
use crate::parser::{MizuNode, Primitive, StyleRules};

use crate::render::accessibility::MizuUserEvent;

use super::manager::MizuWindowManager;

mod a11y_dispatch;
mod idle;
mod keyboard;
mod mouse;
mod redraw;

use a11y_dispatch::dispatch_accesskit_event;
use idle::dispatch_about_to_wait;
// Re-exported for `window::tests::event_loop`, which is the only consumer
// outside this module — the lib build itself never names it directly.
#[cfg(test)]
pub(super) use idle::background_timer_period;
use keyboard::dispatch_keyboard_input;
use mouse::{
    dispatch_cursor_moved, dispatch_mouse_pressed, dispatch_mouse_wheel, dispatch_resized,
};
use redraw::dispatch_redraw_requested;

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
        // Startup commits a document synchronously (it is already parsed and
        // in the tree), so the origin of record is established here alongside
        // the displayed URL — see `ChromeState::committed_url`.
        tab.chrome_state.committed_url = initial_url.clone();
        tab.chrome_state.set_displayed_url(initial_url);

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
