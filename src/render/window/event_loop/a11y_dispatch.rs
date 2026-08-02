//! Accesskit `UserEvent` dispatch (accessibility tree activation/updates).

use crate::render::accessibility::{build_a11y_tree, resolve_ego_id};

use super::super::focus::find_click_and_submit;
use super::super::input::{dispatch_click_gesture, dispatch_form_submit};
use super::super::manager::MizuWindowManager;

// ── Accesskit UserEvent dispatch ────────────────────────────────────────────

/// Handles an `accesskit_winit::Event` delivered via `Event::UserEvent`:
/// serves the initial accessibility tree on request, and routes an
/// AT-initiated action through the same gesture-gated dispatch helpers
/// keyboard activation uses.
pub(super) fn dispatch_accesskit_event(
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
