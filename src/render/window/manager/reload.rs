//! `reload_tab_document`: replaces a tab's document completely, resetting
//! its layout and logic state.

use std::collections::HashMap;

use taffy::TaffyTree;

use crate::core::errors::MizuError;
use crate::core::types::Value;
use crate::render::layout_bridge::EachExpansion;
use crate::render::responsive::RenderEnvironment;

use super::types::{ReloadedDocument, TabState, WindowCtx};

/// Reloads `tab`'s document completely, resetting its layout and logic state.
///
/// `is_active` gates the OS window retitle: a background tab finishing a load
/// must not rename the window out from under the tab the user is looking at.
pub(crate) fn reload_tab_document(
    tab: &mut TabState,
    ctx: &mut WindowCtx<'_>,
    doc: ReloadedDocument,
    is_active: bool,
) -> Result<(), MizuError> {
    let ReloadedDocument {
        dom,
        style_rules,
        style_variants,
        logic_fns,
        interner,
        computed_bindings,
        root_timers,
    } = doc;

    tab.root_timers = root_timers;
    // Old node ids die with the old tree — drop inspector selection state.
    tab.inspector.reset_document_state();
    tab.recent_mutations.clear();
    let mut taffy = TaffyTree::new();
    let mut node_to_taffy_id = HashMap::new();

    let env = RenderEnvironment {
        viewport: tab.viewport_size,
        color_scheme: ctx.preferences.color_scheme,
    };
    let root_taffy_id = crate::render::layout_bridge::build_taffy_tree(
        dom.root(),
        &mut crate::render::layout_bridge::TaffyBuildContext {
            style_rules_map: &style_rules,
            taffy: &mut taffy,
            node_to_taffy_id: &mut node_to_taffy_id,
            image_cache: ctx.image_cache,
            chrome_url: &tab.chrome_state.committed_url,
            variants: &style_variants,
            env: &env,
        },
    )?;

    tab.dom = dom;
    // Keep the OS window title in sync with the newly loaded document's
    // `doc "..."` title attribute (falls back to the same default used at
    // startup, matching `render::window::event_loop`) — but only for the tab
    // actually on screen.
    if is_active && let Some(window) = ctx.window {
        let title = tab
            .dom
            .root()
            .value()
            .attributes
            .get("title")
            .cloned()
            .unwrap_or_else(|| "Mizu Navigator".to_string());
        window.set_title(&title);
    }
    tab.style_rules = style_rules;
    tab.style_variants = style_variants;
    tab.logic_fns = logic_fns;
    tab.computed_bindings = computed_bindings;
    tab.taffy = taffy;
    tab.node_to_taffy_id = node_to_taffy_id;
    tab.root_taffy_id = root_taffy_id;

    tab.scroll_offsets.clear();
    tab.root_timer_queue.clear();
    tab.focused_node = None;
    tab.root_scroll_offset_y = 0.0;
    tab.chrome_state.focused = false;
    tab.chrome_state.selection = None;
    tab.text_layouts.clear();
    tab.text_dimensions.clear();
    tab.dirty_nodes.clear();
    tab.local_inputs.clear();
    tab.local_file_selections.clear();
    // The new Taffy tree has fresh node IDs; the old synthetic IDs are invalid.
    tab.each_expansion = EachExpansion::default();
    // Both are keyed by `EgoNodeId` from the *previous* DOM. Left behind they
    // grow by one document's worth of entries per navigation, and — worse than
    // the leak — an id reused by the new tree would seed virtualization with
    // another document's measured row height.
    tab.each_row_height_estimate.clear();
    tab.each_container_offset_y.clear();

    tab.rebuild_node_mappings();
    tab.store = crate::core::types::VariableStore {
        evaluator: Default::default(),
        interner,
    }
    .freeze();
    tab.store.set_runtime(
        "window_url",
        Value::from(tab.chrome_state.committed_url.clone()),
    );
    tab.rebuild_dependency_index();

    tab.trigger_logic_reload(ctx.logic_tx);

    // The worker's per-tab state is rebuilt by the reload above, so any tick
    // still outstanding against the previous document is never coming back.
    tab.reset_timer_ticks();
    tab.setup_timers();
    Ok(())
}
