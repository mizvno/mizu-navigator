//! Keyboard-input dispatch: tab/global shortcuts, chrome-bar text editing, focused-node keys, and Escape.

use winit::{
    keyboard::{Key, NamedKey},
    window::Window,
};

use crate::render::chrome_vello::ChromeKeyAction;
use crate::render::navigation::NavigationInitiator;

use crate::render::accessibility::MizuUserEvent;

use super::super::focus::find_click_and_submit;
use super::super::input::{
    dispatch_click_gesture, dispatch_form_submit, find_form_submitter, push_input_text,
};
use super::super::manager::{MizuWindowManager, resize_tab_viewport};
use super::super::navigate::{navigate_back, navigate_forward, navigate_to_url};
use super::mouse::{BLANK_TAB_URL, retitle_window};

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
            let url = tab.chrome_state.committed_url.clone();
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
    } else if tab.inspector.value_view.is_some() {
        // Dismissing the value drawer is a smaller Escape than closing the
        // whole panel, same precedence as the picker above it.
        tab.inspector.value_view = None;
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
pub(super) fn dispatch_keyboard_input(
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
