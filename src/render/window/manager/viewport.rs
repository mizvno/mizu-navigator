//! Viewport recomputation: `resize_tab_viewport` (full/partial Taffy
//! rebuild against a new content boundary) and `refresh_tab_virtualized_windows`
//! (cheap scroll-driven re-expansion check for virtualized `each` blocks).

use std::collections::HashMap;

use ego_tree::NodeId as EgoNodeId;
use taffy::{TaffyTree, geometry::Size, style::AvailableSpace};

use crate::core::errors::MizuError;
use crate::core::types::Value;
use crate::render::chrome_vello::CHROME_HEIGHT;
use crate::render::layout_bridge::{EachExpansion, expand_each_nodes};
use crate::render::responsive::{RenderEnvironment, ViewportSize};

use super::types::{TabState, WindowCtx};

pub(crate) fn resize_tab_viewport(
    tab: &mut TabState,
    ctx: &mut WindowCtx<'_>,
    width: f32,
    height: f32,
    dirty_list_names: Option<std::collections::HashSet<String>>,
) -> Result<(), MizuError> {
    if width <= 0.0 || height <= 0.0 {
        return Ok(());
    }

    // The docked inspector panel reduces the document's usable width.
    // Centralised here so every call site (resize, F12 toggle, timers)
    // automatically lays the document out in the remaining space. Per-tab,
    // because the inspector's open state is per-tab.
    let width = if tab.inspector.open {
        (width - crate::render::inspector::PANEL_WIDTH).max(120.0)
    } else {
        width
    };

    let content_height = (height - CHROME_HEIGHT).max(0.0);
    let viewport_size = Size {
        width: AvailableSpace::Definite(width),
        height: AvailableSpace::MaxContent,
    };

    // ux-6: re-resolve breakpoint/color-scheme variants and vw/vh/vmin/
    // vmax dimensions against the new content viewport before laying
    // out. This rebuilds the taffy tree's *styles* (not the DOM/logic
    // state) — the same construction `reload_tab_document` uses, so a
    // resize's responsive re-styling is exactly as correct as a fresh
    // document load, just without re-parsing anything. Bounded by the
    // same ≥16ms debounce this function is already only called behind
    // (see `window::event_loop`'s `WindowEvent::Resized` handler) — "not
    // on every resize pixel", per the design memo.
    tab.viewport_size = ViewportSize {
        width,
        height: content_height,
    };
    tab.layout_stale = false;
    let env = RenderEnvironment {
        viewport: tab.viewport_size,
        color_scheme: ctx.preferences.color_scheme,
    };
    let mut new_taffy = TaffyTree::new();
    let mut new_node_to_taffy_id = HashMap::new();
    let new_root_taffy_id = crate::render::layout_bridge::build_taffy_tree(
        tab.dom.root(),
        &mut crate::render::layout_bridge::TaffyBuildContext {
            style_rules_map: &tab.style_rules,
            taffy: &mut new_taffy,
            node_to_taffy_id: &mut new_node_to_taffy_id,
            image_cache: ctx.image_cache,
            chrome_url: &tab.chrome_state.committed_url,
            variants: &tab.style_variants,
            env: &env,
        },
    )?;
    tab.taffy = new_taffy;
    tab.node_to_taffy_id = new_node_to_taffy_id;
    tab.root_taffy_id = new_root_taffy_id;
    // The rebuilt tree has fresh synthetic-node bookkeeping — the old
    // each-expansion's `groups`/`original_children`/`all_synthetic_ids`
    // reference taffy node ids that no longer exist in `tab.taffy`, so
    // they must not be reused (`expand_each_nodes`'s "restore the
    // previous expansion" step would otherwise operate on stale/
    // possibly-reused ids). `truncated` is keyed by `EgoNodeId`, which
    // *is* still meaningful, and is kept so the budget-change log below
    // compares against the real previous count instead of always
    // reading 0 (which would log a spurious "budget exceeded" on every
    // resize of a document with any truncated list).
    let prev_truncated = std::mem::take(&mut tab.each_expansion.truncated);
    tab.each_expansion = EachExpansion::default();

    if let Ok(mut style) = tab.taffy.style(tab.root_taffy_id).cloned() {
        style.min_size.height = taffy::style::Dimension::Length(content_height);
        style.size.height = taffy::style::Dimension::Auto;
        let _ = tab.taffy.set_style(tab.root_taffy_id, style);
    }

    // Expand `Each` nodes in Taffy to match the current list lengths.
    // Must run before `compute_layout_with_measure` so Taffy sees the
    // full N-row tree and produces correct per-item positions.
    let new_expansion = expand_each_nodes(
        &tab.dom,
        &tab.store,
        &mut tab.taffy,
        &tab.node_to_taffy_id,
        &tab.each_expansion,
        dirty_list_names.as_ref(), // None = full rebuild; Some(set) = granular
        tab.root_scroll_offset_y,
        content_height,
        &tab.each_container_offset_y,
        &tab.each_row_height_estimate,
    )?;

    for (node_id, &new_count) in &new_expansion.truncated {
        let old_count = prev_truncated.get(node_id).copied().unwrap_or(0);
        if new_count != old_count {
            let msg = format!("budget exceeded: clamped list to hide {} items", new_count);
            tab.inspector_log.push_event(
                crate::render::inspector::log::EventKind::Layout,
                msg.clone(),
            );
            tracing::warn!("{}", msg);
        }
    }
    for (node_id, &old_count) in &prev_truncated {
        if !new_expansion.truncated.contains_key(node_id) {
            let msg = format!(
                "budget restored: previously clamped {} items now visible",
                old_count
            );
            tab.inspector_log.push_event(
                crate::render::inspector::log::EventKind::Layout,
                msg.clone(),
            );
            tracing::warn!("{}", msg);
        }
    }

    tab.each_expansion = new_expansion;

    let dom = &tab.dom;
    let style_rules = &tab.style_rules;
    let style_variants = &tab.style_variants;
    let render_env = RenderEnvironment {
        viewport: tab.viewport_size,
        color_scheme: ctx.preferences.color_scheme,
    };
    let font_cx = &mut *ctx.font_cx;
    let layout_cx = &mut *ctx.layout_cx;
    let store = &tab.store;
    let text_layouts = &mut tab.text_layouts;
    let text_dimensions = &mut tab.text_dimensions;
    let dirty_nodes = &mut tab.dirty_nodes;
    let local_inputs = &tab.local_inputs;
    let node_id_to_u32 = &tab.node_id_to_u32;
    let focused_input = tab.focused_node;

    tab.taffy
        .compute_layout_with_measure(
            tab.root_taffy_id,
            viewport_size,
            |_known_dimensions, available_space, _node_id, node_context, _style| {
                if let Some(ego_id) = node_context {
                    let node_id = *ego_id;
                    if !dirty_nodes.contains(&node_id)
                        && let Some(&(w, h)) = text_dimensions.get(&node_id)
                    {
                        return taffy::geometry::Size {
                            width: w,
                            height: h,
                        };
                    }

                    let available_width = match available_space.width {
                        AvailableSpace::Definite(w) => Some(w),
                        AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                    };

                    if let Some((dims, layout)) = crate::render::text_engine::calculate_node_text(
                        node_id,
                        available_width,
                        &mut crate::render::text_engine::TextLayoutContext {
                            dom,
                            style_rules,
                            font_cx: &mut *font_cx,
                            layout_cx: &mut *layout_cx,
                            store,
                            local_inputs,
                            node_id_to_u32,
                            focused_input,
                            style_variants,
                            render_env: &render_env,
                        },
                    ) {
                        text_dimensions.insert(node_id, dims);
                        text_layouts.insert(node_id, layout);
                        dirty_nodes.remove(&node_id);
                        return taffy::geometry::Size {
                            width: dims.0,
                            height: dims.1,
                        };
                    }
                }
                taffy::geometry::Size::ZERO
            },
        )
        .map_err(|e| MizuError::ParseError(format!("Layout computation error: {:?}", e)))?;

    // Refresh each virtualized `Each` block's row-height estimate from
    // this frame's real Taffy measurements, so the *next* layout pass
    // (which rebuilds `tab.taffy` from scratch and can't see these
    // synthetic row ids anymore) windows against real data instead of
    // `DEFAULT_ROW_HEIGHT_ESTIMATE_PX`. Cheap: one `layout()` lookup per
    // currently-visible row, not per list element.
    let mut estimates: Vec<(EgoNodeId, f32)> = Vec::new();
    for (each_dom_id, groups) in &tab.each_expansion.groups {
        if groups.is_empty() {
            continue;
        }
        let mut total = 0.0f32;
        let mut count = 0usize;
        for (row_id, _) in groups {
            if let Ok(layout) = tab.taffy.layout(*row_id) {
                total += layout.size.height;
                count += 1;
            }
        }
        if count > 0 {
            estimates.push((*each_dom_id, total / count as f32));
        }
    }
    for (each_dom_id, estimate) in estimates {
        tab.each_row_height_estimate.insert(each_dom_id, estimate);
    }

    Ok(())
}

/// Cheaply checks whether the current scroll position still falls inside
/// each virtualized `Each` block's already-expanded row window (plus a
/// small slack margin), and only pays for a real re-expansion when it
/// doesn't. Returns `true` when a re-layout actually happened.
pub(crate) fn refresh_tab_virtualized_windows(
    tab: &mut TabState,
    ctx: &mut WindowCtx<'_>,
    viewport_height: f32,
) -> Result<bool, MizuError> {
    let scroll_y = tab.root_scroll_offset_y;
    let slack_rows =
        (*crate::render::layout_bridge::VIRTUALIZATION_BUFFER_ROWS / 2).max(1) as isize;

    let mut dirty: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (&each_dom_id, &window_start) in &tab.each_expansion.window_start {
        let Some(groups) = tab.each_expansion.groups.get(&each_dom_id) else {
            continue;
        };
        let Some(list_name) = tab
            .dom
            .get(each_dom_id)
            .and_then(|n| n.value().iterator_context.as_ref())
            .map(|(_, name)| name.clone())
        else {
            continue;
        };
        let n = match tab.store.get(&list_name).ok() {
            Some(Value::List(arc)) => arc.len(),
            _ => continue,
        };

        let y0 = tab
            .each_container_offset_y
            .get(&each_dom_id)
            .copied()
            .unwrap_or(0.0);
        let row_h = tab
            .each_row_height_estimate
            .get(&each_dom_id)
            .copied()
            .unwrap_or(*crate::render::layout_bridge::DEFAULT_ROW_HEIGHT_ESTIMATE_PX)
            .max(1.0);
        let buffer = *crate::render::layout_bridge::VIRTUALIZATION_BUFFER_ROWS as f32 * row_h;

        let needed_first = (((scroll_y - y0 - buffer) / row_h).floor().max(0.0) as usize).min(n);
        let needed_last = (((scroll_y + viewport_height - y0 + buffer) / row_h)
            .ceil()
            .max(0.0) as usize)
            .clamp(needed_first, n);

        let window_end = window_start + groups.len();
        let still_covered = needed_first as isize >= window_start as isize - slack_rows
            && needed_last as isize <= window_end as isize + slack_rows;

        if !still_covered {
            dirty.insert(list_name);
        }
    }

    if dirty.is_empty() {
        return Ok(false);
    }

    // Reuse the last-known viewport size — this is a scroll-driven
    // refresh, not a real resize, so there is no new width/height to
    // query. `tab.viewport_size` already has the inspector panel width
    // subtracted (and chrome height), so undo both before passing back
    // into `resize_tab_viewport`, which subtracts them again itself.
    let width = tab.viewport_size.width
        + if tab.inspector.open {
            crate::render::inspector::PANEL_WIDTH
        } else {
            0.0
        };
    let height = tab.viewport_size.height + CHROME_HEIGHT;
    resize_tab_viewport(tab, ctx, width, height, Some(dirty))?;
    Ok(true)
}
