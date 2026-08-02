//! `Each`-block Taffy expansion: [`EachIterationOverrides`]/[`EachGroupEntries`]
//! type aliases, the [`EachExpansion`] tracking struct, and `expand_each_nodes`
//! (the entry point that syncs Taffy's synthetic per-iteration nodes to a
//! list variable's current length, including virtualization windowing).

use std::collections::HashMap;

use ego_tree::{NodeId as EgoNodeId, Tree};
use taffy::TaffyTree;

use crate::core::errors::MizuError;
use crate::core::types::{Value, VariableStore};
use crate::parser::{MizuNode, Primitive};

use super::helpers::{clone_taffy_subtree, count_dom_subtree_size, new_spacer_leaf};

/// Mapping from template DOM node IDs to their per-iteration synthetic Taffy
/// node IDs.  `paint_each` installs this as a temporary override so that
/// `paint_node` reads Taffy-computed coordinates from the expanded tree
/// rather than from the stale single-template node.
pub type EachIterationOverrides = HashMap<EgoNodeId, taffy::prelude::NodeId>;

/// Global budget for synthetic layout nodes (L1 invariant).
///
/// L1 — No unmetered work proportional to remote data. Any subsystem that
/// performs O(data) allocation or CPU work must draw from an explicit,
/// named budget. This constant sits at the same order as MAX_INSTRUCTIONS
/// so the expression cliff and the layout cliff coincide.
///
/// An unmeasured starting value, overridable for a single run via
/// `MIZU_MAX_SYNTHETIC_LAYOUT_NODES` (see the module doc on
/// [`crate::core::config`]).
pub static MAX_SYNTHETIC_LAYOUT_NODES: std::sync::LazyLock<usize> =
    std::sync::LazyLock::new(|| {
        crate::core::config::env_override("MIZU_MAX_SYNTHETIC_LAYOUT_NODES", 20_000)
    });

/// Default estimated row height (logical px) used to decide which rows of an
/// `Each` list fall inside the virtualized window before any row of that
/// block has actually been measured. Refined every frame from real Taffy
/// measurements once available — see `MizuWindowManager::each_row_height_estimate`.
pub static DEFAULT_ROW_HEIGHT_ESTIMATE_PX: std::sync::LazyLock<f32> =
    std::sync::LazyLock::new(|| {
        crate::core::config::env_override("MIZU_EACH_ROW_HEIGHT_ESTIMATE_PX", 96.0)
    });

/// Extra rows of slack expanded on each side of the visible viewport, so
/// small scroll deltas don't force a re-expansion every frame.
pub static VIRTUALIZATION_BUFFER_ROWS: std::sync::LazyLock<usize> =
    std::sync::LazyLock::new(|| {
        crate::core::config::env_override("MIZU_EACH_VIRTUALIZATION_BUFFER_ROWS", 6)
    });

/// One entry per *visible* list element: `(row_container_taffy_id, override_map)`.
/// Indexed relative to the block's `EachExpansion::window_start`, not the
/// absolute list index — see [`expand_each_nodes`]'s windowing pass.
pub type EachGroupEntries = Vec<(taffy::prelude::NodeId, EachIterationOverrides)>;

/// All synthetic Taffy nodes produced during one [`expand_each_nodes`] call.
///
/// Stored in `MizuWindowManager` and rebuilt every time `resize_viewport` runs
/// so that changes to a list variable are reflected in layout on the next frame.
#[derive(Default)]
pub struct EachExpansion {
    /// `each_dom_id → [(row_taffy_id, {template_dom_id → synth_taffy_id})]`
    pub groups: HashMap<EgoNodeId, EachGroupEntries>,
    /// Snapshot of the original Taffy children of each `Each` node, taken
    /// before expansion.  Used to restore the tree before re-expanding.
    pub original_children: HashMap<EgoNodeId, Vec<taffy::prelude::NodeId>>,
    /// Every synthetic Taffy node created, collected for bulk removal on the
    /// next call (prevents arena growth on each frame).
    pub all_synthetic_ids: Vec<taffy::prelude::NodeId>,
    /// Number of hidden list items per `Each` node due to budget truncation.
    pub truncated: HashMap<EgoNodeId, usize>,
    /// Index of the first list element represented in `groups` for each
    /// `Each` node (0 when the whole list fit inside the window). Lets
    /// painting and scroll-driven re-virtualization map a visible-window
    /// position back to an absolute list index.
    pub window_start: HashMap<EgoNodeId, usize>,
}

/// Expands every `Each` node in the DOM into N synthetic Taffy subtrees
/// (one per list element) so that `taffy.compute_layout` sees the full
/// N-row tree and produces correct per-item positions.
///
/// **Must** be called before `compute_layout` / `compute_layout_with_measure`
/// so that Taffy computes the expanded positions.
///
/// `prev` is the expansion from the previous frame; its synthetic nodes are
/// restored / removed before the new expansion is built **for the dirty
/// blocks only** — see `dirty_list_names` below.
///
/// `dirty_list_names` controls granular invalidation:
///
/// * `None` — full rebuild: tear down *all* previous synthetic nodes and
///   re-expand every `Each` block from scratch. Use this after
///   `build_taffy_tree` creates a brand-new `TaffyTree` (e.g. on a window
///   resize), where the old synthetic IDs no longer exist.
/// * `Some(set)` — partial rebuild: only the `Each` blocks whose backing
///   list variable name is in `set` are torn down and re-expanded; all other
///   blocks are carried forward from `prev` as-is. Use this after a store
///   mutation so that unaffected lists pay zero Taffy allocation cost.
///
/// ## Virtualization
///
/// Only the rows whose estimated position falls in
/// `[scroll_y - buffer, scroll_y + viewport_height + buffer]` are expanded
/// into real synthetic Taffy subtrees; the rest are represented by up to two
/// fixed-height "spacer" leaves (before/after the visible window) so the
/// `Each` container's total height stays approximately correct without ever
/// cloning more than a small, viewport-bounded number of rows. `each_offsets_y`
/// and `each_row_heights` are the previous frame's measurements (see
/// `MizuWindowManager::each_container_offset_y` / `each_row_height_estimate`);
/// missing entries (first paint) fall back to `y = 0.0` /
/// `DEFAULT_ROW_HEIGHT_ESTIMATE_PX` and self-correct on the next frame.
pub fn expand_each_nodes(
    dom: &Tree<MizuNode>,
    store: &VariableStore,
    taffy: &mut TaffyTree<EgoNodeId>,
    node_to_taffy_id: &HashMap<EgoNodeId, taffy::prelude::NodeId>,
    prev: &EachExpansion,
    dirty_list_names: Option<&std::collections::HashSet<String>>,
    scroll_y: f32,
    viewport_height: f32,
    each_offsets_y: &HashMap<EgoNodeId, f32>,
    each_row_heights: &HashMap<EgoNodeId, f32>,
) -> Result<EachExpansion, MizuError> {
    // ── Step 1: restore the previous expansion ────────────────────────────
    // When `dirty_list_names` is `Some(set)`, only restore (and free) the
    // synthetic nodes belonging to the dirty blocks; the rest stay in place.
    // When `None` (full rebuild), restore everything — old IDs are stale.
    for (&each_dom_id, orig_children) in &prev.original_children {
        let should_rebuild = dirty_list_names
            .map(|set| {
                // Look up this Each node's list_name to decide whether it's dirty.
                dom.get(each_dom_id)
                    .and_then(|n| n.value().iterator_context.as_ref())
                    .map(|(_, name)| set.contains(name))
                    .unwrap_or(false)
            })
            .unwrap_or(true); // None = full rebuild, always restore

        if should_rebuild {
            if let Some(&each_taffy_id) = node_to_taffy_id.get(&each_dom_id) {
                let _ = taffy.set_children(each_taffy_id, orig_children);
            }
        }
    }
    // Free synthetic nodes only for the dirty (or all, on full rebuild) blocks.
    if let Some(set) = dirty_list_names {
        // `all_synthetic_ids` is a flat list with no per-block grouping, so we
        // build a set of the IDs that belong to dirty blocks by iterating
        // `prev.groups`, then free only those.
        let mut dirty_synth_ids: std::collections::HashSet<taffy::prelude::NodeId> =
            std::collections::HashSet::new();
        for (each_dom_id, groups) in &prev.groups {
            let is_dirty = dom
                .get(*each_dom_id)
                .and_then(|n| n.value().iterator_context.as_ref())
                .map(|(_, name)| set.contains(name))
                .unwrap_or(false);
            if is_dirty {
                for (row_id, _) in groups {
                    dirty_synth_ids.insert(*row_id);
                }
            }
        }
        for &synth_id in &prev.all_synthetic_ids {
            if dirty_synth_ids.contains(&synth_id) {
                let _ = taffy.remove(synth_id);
            }
        }
    } else {
        // Full rebuild: free every synthetic node unconditionally.
        for &synth_id in &prev.all_synthetic_ids {
            let _ = taffy.remove(synth_id);
        }
    }

    // ── Step 2: build the new expansion ───────────────────────────────────
    // Start from a copy of the previous expansion for the blocks we are NOT
    // rebuilding (only meaningful when dirty_list_names is Some).
    let mut expansion = if let Some(set) = dirty_list_names {
        // Carry forward everything from prev, then overwrite the dirty blocks.
        let mut carried = EachExpansion {
            groups: HashMap::new(),
            original_children: HashMap::new(),
            all_synthetic_ids: Vec::new(),
            truncated: HashMap::new(),
            window_start: HashMap::new(),
        };
        for (each_dom_id, groups) in &prev.groups {
            let is_dirty = dom
                .get(*each_dom_id)
                .and_then(|n| n.value().iterator_context.as_ref())
                .map(|(_, name)| set.contains(name))
                .unwrap_or(false);
            if !is_dirty {
                carried.groups.insert(*each_dom_id, groups.clone());
                if let Some(orig) = prev.original_children.get(each_dom_id) {
                    carried.original_children.insert(*each_dom_id, orig.clone());
                }
                if let Some(&trunc) = prev.truncated.get(each_dom_id) {
                    carried.truncated.insert(*each_dom_id, trunc);
                }
                if let Some(&ws) = prev.window_start.get(each_dom_id) {
                    carried.window_start.insert(*each_dom_id, ws);
                }
                // Carry their synthetic IDs so future full-rebuild teardowns work.
                for (row_id, _) in groups {
                    carried.all_synthetic_ids.push(*row_id);
                }
            }
        }
        carried
    } else {
        EachExpansion::default()
    };

    let mut remaining_budget =
        MAX_SYNTHETIC_LAYOUT_NODES.saturating_sub(expansion.all_synthetic_ids.len());

    // Collect Each-node metadata without holding tree borrows.
    let each_nodes: Vec<(EgoNodeId, String)> = dom
        .nodes()
        .filter_map(|node_ref| {
            let v = node_ref.value();
            if v.primitive != Primitive::Each {
                return None;
            }
            let (_, list_name) = v.iterator_context.as_ref()?;
            // Skip blocks that are not in the dirty set (they were carried above).
            if let Some(set) = dirty_list_names {
                if !set.contains(list_name) {
                    return None;
                }
            }
            Some((node_ref.id(), list_name.clone()))
        })
        .collect();

    for (each_dom_id, list_name) in each_nodes {
        let n = match store.get(&list_name).ok() {
            Some(Value::List(arc)) => arc.len(),
            _ => continue, // list not yet in store — leave this Each as-is
        };
        if n == 0 {
            continue;
        }

        let each_taffy_id = match node_to_taffy_id.get(&each_dom_id) {
            Some(&id) => id,
            None => continue,
        };

        // DOM children of this Each are the template nodes.
        let template_dom_children: Vec<EgoNodeId> = dom
            .get(each_dom_id)
            .map(|n| n.children().map(|c| c.id()).collect())
            .unwrap_or_default();

        if template_dom_children.is_empty() {
            continue;
        }

        let mut template_size = 0;
        for &tmpl_dom_id in &template_dom_children {
            template_size += count_dom_subtree_size(dom, tmpl_dom_id);
        }

        let budget_per_row = template_size + 1; // +1 for the row container

        // ── Virtualization window: only expand rows near the viewport ──────
        let y0 = each_offsets_y.get(&each_dom_id).copied().unwrap_or(0.0);
        let row_h = each_row_heights
            .get(&each_dom_id)
            .copied()
            .unwrap_or(*DEFAULT_ROW_HEIGHT_ESTIMATE_PX)
            .max(1.0);
        let buffer = *VIRTUALIZATION_BUFFER_ROWS as f32 * row_h;
        let rel_top = scroll_y - y0 - buffer;
        let rel_bottom = scroll_y + viewport_height - y0 + buffer;
        let first = ((rel_top / row_h).floor().max(0.0) as usize).min(n);
        let wanted_last = ((rel_bottom / row_h).ceil().max(0.0) as usize).clamp(first, n);
        let wanted_rows = wanted_last - first;

        let max_rows = if budget_per_row == 0 {
            0
        } else {
            remaining_budget / budget_per_row
        };
        let visible_rows = wanted_rows.min(max_rows);
        let last = first + visible_rows;

        if visible_rows < wanted_rows {
            // Budget-clamped — distinct from rows simply outside the
            // virtualized window (those are expected and unremarkable).
            expansion
                .truncated
                .insert(each_dom_id, wanted_rows - visible_rows);
        }
        remaining_budget = remaining_budget.saturating_sub(visible_rows * budget_per_row);

        // Save the original Taffy children for restoration next frame.
        let orig_taffy_children: Vec<taffy::prelude::NodeId> = template_dom_children
            .iter()
            .filter_map(|dom_id| node_to_taffy_id.get(dom_id).copied())
            .collect();
        expansion
            .original_children
            .insert(each_dom_id, orig_taffy_children);

        // Build one iteration group per visible row, each containing a clone
        // of the template subtree.
        let mut groups: EachGroupEntries = Vec::with_capacity(visible_rows);

        for _ in first..last {
            let mut overrides: EachIterationOverrides = HashMap::new();
            let mut row_children: Vec<taffy::prelude::NodeId> = Vec::new();

            for &tmpl_dom_id in &template_dom_children {
                if let Some(tmpl_node) = dom.get(tmpl_dom_id) {
                    let synth_id = clone_taffy_subtree(
                        tmpl_node,
                        taffy,
                        node_to_taffy_id,
                        &mut overrides,
                        &mut expansion.all_synthetic_ids,
                    )?;
                    row_children.push(synth_id);
                }
            }

            // Row container: a transparent, non-shrinking flex column that
            // wraps the synthetic template copies for this iteration.
            let row_style = taffy::style::Style {
                flex_shrink: 0.0,
                ..taffy::style::Style::default()
            };
            let row_id = taffy
                .new_with_children(row_style, &row_children)
                .map_err(|e| MizuError::ParseError(format!("Each row container: {e}")))?;

            expansion.all_synthetic_ids.push(row_id);
            groups.push((row_id, overrides));
        }

        // Replace the Each's Taffy children with (optional top spacer) +
        // the visible row containers + (optional bottom spacer), so the
        // container's total height stays approximately correct for rows
        // that were virtualized out rather than actually laid out.
        let mut row_ids: Vec<taffy::prelude::NodeId> = Vec::with_capacity(visible_rows + 2);

        if first > 0 {
            let spacer_id = new_spacer_leaf(taffy, first as f32 * row_h)?;
            expansion.all_synthetic_ids.push(spacer_id);
            row_ids.push(spacer_id);
        }
        row_ids.extend(groups.iter().map(|(id, _)| *id));
        if last < n {
            let spacer_id = new_spacer_leaf(taffy, (n - last) as f32 * row_h)?;
            expansion.all_synthetic_ids.push(spacer_id);
            row_ids.push(spacer_id);
        }

        taffy
            .set_children(each_taffy_id, &row_ids)
            .map_err(|e| MizuError::ParseError(format!("Each set_children: {e}")))?;

        // Ensure the Each container is a Flex column so rows stack vertically
        // regardless of the display mode the single-template style specified.
        if let Ok(mut style) = taffy.style(each_taffy_id).cloned() {
            style.display = taffy::style::Display::Flex;
            style.flex_direction = taffy::style::FlexDirection::Column;
            let _ = taffy.set_style(each_taffy_id, style);
        }

        expansion.groups.insert(each_dom_id, groups);
        expansion.window_start.insert(each_dom_id, first);
    }

    Ok(expansion)
}
