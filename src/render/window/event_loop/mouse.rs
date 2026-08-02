//! Mouse-input dispatch: resize, click routing (chrome/inspector/history-sidebar/DOM), and wheel scrolling.

use vello::util::{RenderContext, RenderSurface};
use winit::{dpi::PhysicalSize, event::MouseScrollDelta, window::Window};

use crate::parser::{MizuOverflow, Primitive};
use crate::render::chrome_vello::{CHROME_HEIGHT, ChromeHitZone, chrome_hit_zone, url_text_left};
use crate::render::hit_test::hit_test;
use crate::render::navigation::NavigationInitiator;

use crate::render::accessibility::MizuUserEvent;

use super::super::input::{
    dispatch_click_gesture, dispatch_file_input_click, dispatch_form_submit, is_file_input,
};
use super::super::manager::{MizuWindowManager, refresh_tab_virtualized_windows};
use super::super::navigate::{navigate_back, navigate_forward, navigate_to_url};
use super::*;

pub(super) fn dispatch_resized(
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
pub(super) fn dispatch_cursor_moved(
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

    // ── Inspector hover: track the cursor in panel-local coordinates ─
    // Stored as a point, not a resolved row: the paint pass already computes
    // the row geometry and resolves the point against it, so the two can
    // never disagree about which row is lit.
    if tab.inspector.open {
        let logical_width = window.inner_size().width as f32 / scale_factor as f32;
        let left = crate::render::inspector::panel_left(logical_width);
        let over_panel = mouse.last_logical_x >= left && mouse.last_logical_y >= CHROME_HEIGHT;
        let hover = over_panel.then(|| {
            (
                mouse.last_logical_x - left,
                mouse.last_logical_y - CHROME_HEIGHT,
            )
        });
        if tab.inspector.hover != hover {
            tab.inspector.hover = hover;
            window.request_redraw();
        }
        // The panel shrinks the page viewport rather than overlaying it, so
        // there is no document under these coordinates to hit-test.
        if over_panel && !tab.inspector.picker {
            window.set_cursor_icon(winit::window::CursorIcon::Default);
            return;
        }
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
            // Reload re-fetches the loaded document, not whatever text the URL
            // bar happens to be showing.
            let url = tab.chrome_state.committed_url.clone();
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

            tab.chrome_state.set_displayed_url(url.clone());
            tab.chrome_state.focused = false;
            tab.chrome_state.suggestions.clear();
            tab.chrome_state.selected_suggestion = None;

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
    let panel_height =
        window.inner_size().height as f32 / window.scale_factor() as f32 - CHROME_HEIGHT;
    let outcome =
        crate::render::inspector::handle_panel_click(&mut tab.inspector, &rows, panel_height, x, y);
    if let Some(text) = outcome.copy
        && let Ok(mut cb) = arboard::Clipboard::new()
    {
        let _ = cb.set_text(text);
    }
    if outcome.changed {
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
                (logical_height - CHROME_HEIGHT - crate::render::inspector::content_top()).max(0.0);
            // Rows are not uniformly tall, so the target offset comes from the
            // same layout the panel paints with.
            let tops = crate::render::inspector::row_tops(&rows);
            tab.inspector.scroll_to(tops[idx], viewport_h);
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
pub(super) fn dispatch_mouse_pressed(
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
pub(super) fn tab_strip_entries(
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
                    if t.chrome_state.committed_url.is_empty() {
                        "New Tab".to_string()
                    } else {
                        t.chrome_state.committed_url.clone()
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
pub(super) const BLANK_TAB_URL: &str = "about:blank";

/// Sets the OS window title from the active tab's document title.
///
/// Only the active tab may retitle the window; a background tab finishing a
/// load must not rename what the user is looking at.
pub(super) fn retitle_window(manager: &MizuWindowManager, window: &Window) {
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
/// Handles `WindowEvent::MouseWheel`: scrolls the inspector panel, the
/// nearest scrollable DOM ancestor under the cursor, or the root document,
/// in that priority order.
pub(super) fn dispatch_mouse_wheel(
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
            let panel_y = mouse.last_logical_y - CHROME_HEIGHT;
            let panel_height = window.inner_size().height as f32 / scale - CHROME_HEIGHT;
            // The drawer has its own scroll region; only route the wheel to
            // the row list when the cursor is above it.
            let over_drawer = tab.inspector.value_view.is_some()
                && panel_y >= panel_height - crate::render::inspector::DRAWER_HEIGHT;
            if over_drawer {
                if let Some(view) = &mut tab.inspector.value_view {
                    view.scroll =
                        (view.scroll + delta_y * 2.0).clamp(0.0, view.max_scroll.max(0.0));
                }
            } else {
                tab.inspector.scroll_by(delta_y * 2.0);
            }
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
