//! The `AboutToWait` idle tick: drains network/logic-worker results, recomputes dirty layout, fires background timers, and schedules the next wakeup.

use winit::window::Window;

use crate::core::types::Symbol;
use crate::network::UiEvent;
use crate::render::navigation::NavigationInitiator;

use crate::render::accessibility::MizuUserEvent;

use super::super::input::apply_clipboard_action;
use super::super::manager::{
    MizuWindowManager, execute_tab_capability_action, resize_tab_viewport,
};
use super::super::navigate::{navigate_to_url, process_network_result};

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
        super::super::navigate::rebuild_tab_taffy_after_image(tab, &mut ctx);
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
        // The worker answered for this tab, so one unit of its dispatch
        // capacity is free again — recorded before the match so an `Err`
        // response frees capacity exactly like an `Ok` one, and a document
        // whose actions keep failing does not throttle itself to a standstill.
        tab.release_timer_tick();
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
                // Gate G1: the agency of *this* batch, decided by the worker
                // from the event variant that produced it and carried on the
                // response itself (see `WorkerResponse::gesture`). Never an
                // ambient per-tab flag: responses are drained FIFO with no
                // correlation to the events that produced them, so a flag set
                // at input-dispatch time and read here would let a `RootTimer`
                // batch that merely arrived near a click inherit the click's
                // authority.
                let batch_gesture = response.gesture;
                for action in response.runtime_actions {
                    if let crate::network::RuntimeAction::Navigate { url } = &action {
                        // N2+N3: Navigate actions go through the choke point;
                        // cross-origin logic-driven navigation is blocked
                        // unless this batch itself carries a user gesture.
                        tab.chrome_state.loading = true;
                        let url = url.clone();
                        let initiator = if batch_gesture {
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
                            batch_gesture,
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
                    // Admission gate: a tick the worker has no capacity for is
                    // dropped, never queued. The timer is still re-armed below,
                    // so it resumes on its own as soon as the worker catches
                    // up — this throttles, it does not disarm.
                    if tab.may_dispatch_timer_tick() {
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
                    } else {
                        tracing::trace!(
                            tab = tab.id.0,
                            timer = idx,
                            "root timer tick dropped: logic worker backlog at capacity"
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
pub(crate) fn background_timer_period(interval_ms: u64) -> u64 {
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
                // Same admission gate as the active tab's loop, and per-tab
                // for the same T1 reason the redirect budget is: a background
                // document must not be able to spend the foreground one's
                // capacity.
                if tab.may_dispatch_timer_tick() {
                    let _ = ctx
                        .logic_tx
                        .send((tab.id, UiEvent::RootTimer { index: idx as u32 }));
                    fired = true;
                } else {
                    tracing::trace!(
                        tab = tab.id.0,
                        timer = idx,
                        "background root timer tick dropped: logic worker backlog at capacity"
                    );
                }
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
pub(super) fn dispatch_about_to_wait(
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
