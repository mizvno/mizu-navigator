//! URL resolution and navigation/network-result handling.

use rustc_hash::FxHashMap;
use std::collections::HashMap;


use crate::core::types::Value;
use crate::render::navigation::{NavigationInitiator, NavigationVerdict, check_navigation};
use crate::render::security::CapabilityPolicy;

use super::AssetSlot;
use super::history::HistoryEntry;
use super::manager::{MAX_REDIRECTS, MizuWindowManager};

/// Resolves and validates a navigation URL given the current document's URL.
///
/// Returns `None` if the navigation is blocked:
/// * A `mizu://` document attempting to navigate to a `file://` resource.
/// * A `file://` document attempting to navigate outside its **Sandbox Base
///   Directory** (the parent folder of the currently-loaded document) via a
///   relative path containing `..` or via an absolute `file://` URL that
///   points outside the sandbox.
///
/// Returns `Some(resolved_url)` otherwise:
/// * Relative paths from a `file://` document are resolved to absolute
///   `file://` URLs using the sandbox base directory.
/// * Bare hostnames / paths with no scheme are normalised to `mizu://`.
///
/// Note: `http://` and `https://` are not valid Mizu schemes and are rejected
/// at dispatch time in `navigate_to_url` before they can ever become the
/// current URL.
pub(crate) fn resolve_navigate_url(current_url: &str, target: &str) -> Option<String> {
    let origin_is_remote = current_url.starts_with("mizu://");
    let origin_is_file = current_url.starts_with("file://");

    // Block A: remote document must not navigate to local files.
    if origin_is_remote && target.starts_with("file://") {
        return None;
    }

    let mut url = target.to_owned();

    // Block B: resolve relative paths from a local-file document.
    if !url.contains("://") && origin_is_file {
        let current = current_url.strip_prefix("file:///").unwrap_or(current_url);
        let current_path = std::path::Path::new(current);
        let base_dir = current_path.parent().unwrap_or(std::path::Path::new("."));
        let resolved = base_dir.join(&url);

        // Fail-closed sandbox check: the resolved path must stay inside base_dir.
        if !crate::render::security::file_sandbox_contains(base_dir, &resolved) {
            tracing::warn!(
                current = %current_url,
                target = %url,
                "SecurityViolation: relative path escapes file:// sandbox base directory"
            );
            return None;
        }

        let canonical = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
        let path_str = canonical.to_string_lossy().replace('\\', "/");
        return Some(format!("file:///{}", path_str));
    }

    // Block C: absolute file:// from a file:// origin — enforce sandbox.
    if origin_is_file && url.starts_with("file://") {
        let current = current_url.strip_prefix("file:///").unwrap_or(current_url);
        let current_path = std::path::Path::new(current);
        let base_dir = current_path.parent().unwrap_or(std::path::Path::new("."));
        let target_path_str = url
            .strip_prefix("file:///")
            .or_else(|| url.strip_prefix("file://"))
            .unwrap_or(url.as_str());
        let target_path = std::path::Path::new(target_path_str);

        if !crate::render::security::file_sandbox_contains(base_dir, target_path) {
            tracing::warn!(
                current = %current_url,
                target = %url,
                "SecurityViolation: absolute file:// target escapes sandbox base directory"
            );
            return None;
        }
        return Some(url);
    }

    // Normalise bare hostname/path (no scheme) to mizu://.
    if !url.contains("://") {
        url = format!("mizu://{}", url);
    }

    Some(url)
}

/// Returns the sandbox base directory for a `file://` document URL, or `None`
/// for non-`file://` origins.
///
/// The sandbox base is the parent directory of the currently-loaded document.
/// All local asset fetches from this document are restricted to this subtree.
pub(crate) fn chrome_url_to_file_sandbox_base(chrome_url: &str) -> Option<String> {
    let file_path = chrome_url.strip_prefix("file:///")?;
    std::path::Path::new(file_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
}
/// Applies a successfully-fetched document: parses all blocks from `source`
/// and reloads the manager's DOM, styles, logic, and URL registry.
///
/// Called both from the file:// fast path in `navigate_to_url` (synchronous
/// disk read) and from `process_network_result` (async QUIC fetch result).
pub(super) fn handle_navigate_success(manager: &mut MizuWindowManager, url: String, source: String) {
    tracing::debug!(url = %url, "navigate success");
    manager.chrome_state.url = url.clone();
    manager.chrome_state.loading = false;
    manager.reset_redirect_count();
    manager
        .store
        .set("window_url", crate::core::types::Value::from(url.clone()));

    let current_dir = std::env::current_dir().unwrap_or_default();
    match crate::parser::split_source_with_origin(
        &source,
        &current_dir,
        crate::parser::Origin::Network,
    ) {
        Ok(blocks) => {
            let mut new_interner = crate::core::types::StringInterner::new();
            let logic_fns = if !blocks.logic_block.trim().is_empty() {
                match crate::parser::logic::parse_logic(&blocks.logic_block, &mut new_interner) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(error = ?e, "logic parse error during navigation");
                        FxHashMap::default()
                    }
                }
            } else {
                FxHashMap::default()
            };
            let new_computed = if !blocks.logic_block.trim().is_empty() {
                match crate::parser::logic::parse_computed_with_functions(
                    &blocks.logic_block,
                    &mut new_interner,
                    &logic_fns,
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!(error = ?e, "computed parse error during navigation");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let new_root_timers = if !blocks.logic_block.trim().is_empty() {
                match crate::parser::logic::parse_root_timers(
                    &blocks.logic_block,
                    &mut new_interner,
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(error = ?e, "root timer parse error during navigation");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let style_rules = if !blocks.style_block.trim().is_empty() {
                match crate::parser::style::parse_style(&blocks.style_block) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = ?e, "style parse error during navigation");
                        HashMap::new()
                    }
                }
            } else {
                HashMap::new()
            };
            let new_url_registry = if !blocks.urls_block.trim().is_empty() {
                match crate::parser::urls::parse_urls(&blocks.urls_block, &mut new_interner) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(error = ?e, "urls parse error during navigation");
                        rustc_hash::FxHashMap::default()
                    }
                }
            } else {
                rustc_hash::FxHashMap::default()
            };
            match crate::parser::layout::parse_layout_with_urls(
                &blocks.layout_block,
                &mut new_interner,
                Some(&new_url_registry),
                url.starts_with("mizu://"),
            ) {
                Ok(dom) => {
                    // Check Information Flow (Invariant F)
                    match crate::parser::flow::check_information_flow(
                        &dom,
                        &new_root_timers,
                        &logic_fns,
                        &new_computed,
                        &new_url_registry,
                        &new_interner,
                    ) {
                        Ok(metrics) => {
                            manager.inspector.flow_metrics = Some(metrics);
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, "flow check error");
                            return; // Reject document load
                        }
                    }

                    manager.url_registry = new_url_registry;
                    if let Err(e) = manager.reload_document(
                        dom,
                        style_rules,
                        logic_fns,
                        new_interner,
                        new_computed,
                        new_root_timers,
                    ) {
                        tracing::error!(error = ?e, "document reload error");
                    } else {
                        tracing::debug!("document reloaded");
                        // ux-4: restore scroll position after a history
                        // (Back/Forward) step. `reload_document` always
                        // resets `root_scroll_offset_y` to 0.0 first, so this
                        // must run after it. A `None` here (the overwhelming
                        // majority of navigations, which aren't history
                        // steps) is a no-op.
                        if let Some(scroll_y) = manager.pending_scroll_restore.take() {
                            manager.root_scroll_offset_y = scroll_y;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = ?e, "layout parse error during navigation");
                }
            }
        }
        Err(e) => {
            tracing::error!(error = ?e, "source split error during navigation");
        }
    }
}

/// Dispatches a single [`crate::network::NetworkResult`] received from the
/// network worker onto the manager's state.
///
/// Called from the `AboutToWait` drain loop — never from a blocking context.
pub(super) fn process_network_result(manager: &mut MizuWindowManager, res: crate::network::NetworkResult) {
    use crate::network::NetworkResult;
    use crate::render::inspector::log::NetOutcome;
    match res {
        NetworkResult::Success { target_var, data } => {
            let bytes = match &data {
                Value::String(s) => Some(s.len()),
                _ => None,
            };
            manager
                .inspector_log
                .complete_net(&target_var, NetOutcome::Ok, bytes);
            // `UiEvent::UpdateVariable` carries the resolved name, not a
            // Symbol: `target_var` already is that name (see the
            // `/* FIX SYMBOL */` fix in `execute_capability_action`), and
            // the worker resolves it against its own frozen interner via
            // `set_runtime` — no interner lookup needed on this side at all.
            let _ = manager
                .logic_tx
                .send(crate::network::UiEvent::UpdateVariable {
                    name: target_var,
                    value: data,
                });
            let _ = manager
                .logic_tx
                .send(crate::network::UiEvent::UpdateVariable {
                    name: "stato_navigazione".to_string(),
                    value: crate::core::types::Value::from("Completato.".to_string()),
                });
        }
        NetworkResult::FetchFailed { target_var, error } => {
            tracing::error!(error = ?error, target = %target_var, "fetch failed");
            manager.inspector_log.complete_net(
                &target_var,
                NetOutcome::Failed(error.to_string()),
                None,
            );
            // Write a readable error where the response would have gone, so
            // the document shows it (e.g. `Status: error: connection refused`).
            let _ = manager
                .logic_tx
                .send(crate::network::UiEvent::UpdateVariable {
                    name: target_var,
                    value: crate::core::types::Value::from(format!("error: {error}")),
                });
        }
        NetworkResult::Error(e) => {
            tracing::error!(error = ?e, "network error");
            manager
                .inspector_log
                .complete_latest_pending(NetOutcome::Failed(e.to_string()));
            manager.chrome_state.loading = false;
            let _ = manager
                .logic_tx
                .send(crate::network::UiEvent::UpdateVariable {
                    name: "stato_navigazione".to_string(),
                    value: crate::core::types::Value::from(format!("Errore: {e}")),
                });
        }
        NetworkResult::NavigateSuccess { url, source } => {
            manager
                .inspector_log
                .complete_net(&url, NetOutcome::Ok, Some(source.len()));
            handle_navigate_success(manager, url, source);
        }
        NetworkResult::NavigationRedirect { new_url } => {
            manager
                .inspector_log
                .complete_latest_pending(NetOutcome::Redirect);
            if manager.register_redirect() {
                tracing::debug!(
                    url = %new_url,
                    count = manager.redirect_count,
                    "redirecting (through choke point)"
                );
                // N2+N5: route through the single choke point so scheme,
                // origin, gesture, and lifecycle checks all apply.
                // The redirect inherits the initiator of the navigation
                // chain — a user-gesture navigation that redirects
                // cross-origin is still user agency.
                //
                // TODO: once navigation carries the original initiator
                // through the redirect chain, wrap it here.  For now,
                // redirects of top-level navigations always carry
                // UserGesture because Navigate can only be reached
                // through navigate_to_url (which is always either
                // user-initiated or a same-origin logic action).
                navigate_to_url(
                    manager,
                    new_url,
                    NavigationInitiator::RedirectOf(Box::new(
                        NavigationInitiator::UserGesture,
                    )),
                );
            } else {
                tracing::error!(
                    limit = *MAX_REDIRECTS,
                    "redirect limit exceeded; aborting navigation"
                );
                manager.chrome_state.loading = false;
                let _ = manager
                    .logic_tx
                    .send(crate::network::UiEvent::UpdateVariable {
                        name: "stato_navigazione".to_string(),
                        value: crate::core::types::Value::from(
                            "Errore: troppi redirect".to_string(),
                        ),
                    });
            }
        }
        NetworkResult::FetchImageSuccess { url, image } => {
            manager
                .inspector_log
                .push_net_done("IMG", &url, NetOutcome::Ok);
            manager.fetching_images.remove(&url);
            manager
                .image_cache
                .insert(url.clone(), AssetSlot::Ready(image));
            let mut new_taffy = taffy::TaffyTree::new();
            let mut new_node_map = HashMap::new();
            match crate::render::layout_bridge::build_taffy_tree(
                manager.dom.root(),
                &manager.style_rules,
                &mut new_taffy,
                &mut new_node_map,
                &manager.image_cache,
                &manager.chrome_state.url,
            ) {
                Ok(new_root) => {
                    manager.taffy = new_taffy;
                    manager.node_to_taffy_id = new_node_map;
                    manager.root_taffy_id = new_root;
                }
                Err(e) => {
                    tracing::error!(error = ?e, "taffy rebuild failed after image fetch");
                }
            }
        }
        NetworkResult::FetchImageFailed { url, error } => {
            manager
                .inspector_log
                .push_net_done("IMG", &url, NetOutcome::Failed(error.to_string()));
            manager.fetching_images.remove(&url);
            manager.image_cache.insert(url.clone(), AssetSlot::Failed);
            tracing::error!(url = %url, error = ?error, "image load failed");
        }
    }

    // Request a layout recalc + redraw for every network result.
    // Clone the Arc so resize_viewport can take &mut manager without a borrow conflict.
    if let Some(w) = manager.window.clone() {
        let physical_size = w.inner_size();
        let logical_width = physical_size.width as f32 / w.scale_factor() as f32;
        let logical_height = physical_size.height as f32 / w.scale_factor() as f32;
        let _ = manager.resize_viewport(logical_width, logical_height);
        w.request_redraw();
    }
}

/// Triggers a navigation to `url`, enforcing the unified navigation policy.
///
/// This is the **single choke point** (invariant N2) for all document-level
/// navigation.  Every navigation — address bar, link click, `navigate`
/// action from logic, redirect of a prior navigation — must pass through
/// this function before any state change or `NetworkCmd::Navigate` is
/// emitted.
///
/// The `initiator` records who/what triggered the navigation so the policy
/// can enforce N3 (cross-origin without user gesture is blocked).
///
/// On a blocked verdict, the reason is logged to both `tracing::warn!` and
/// the inspector Net panel (`BLOCKED` entry).  No state changes occur.
///
/// On an allowed verdict:
/// - `chrome_state.url` is updated (N5: single mutation site).
/// - `capability_policy` is reset for the new origin (N5).
/// - The redirect chain counter is reset for non-redirect initiators.
/// - `file://` documents are loaded directly; `mizu://` dispatches
///   `NetworkCmd::Navigate` to the network worker.
pub(super) fn navigate_to_url(
    manager: &mut MizuWindowManager,
    url: String,
    initiator: NavigationInitiator,
) {
    // ux-4: a pending scroll restore only ever belongs to the history step
    // that set it. Any other navigation (fresh, logic, redirect) starting
    // here must not inherit a stale value from an earlier, unrelated step.
    if !matches!(initiator, NavigationInitiator::HistoryStep) {
        manager.pending_scroll_restore = None;
    }

    // Reloading or navigating to the blank start page is a no-op: there is
    // nothing to fetch, and `about:` is not a routable scheme.
    if url == "about:blank" {
        manager.chrome_state.loading = false;
        return;
    }

    // For file:// origins with relative paths, we still need resolve_navigate_url
    // for sandbox enforcement (it does I/O via canonicalize).  check_navigation
    // handles the pure policy, then we do the I/O-dependent resolution.
    let resolved_url = if !url.contains("://") && manager.chrome_state.url.starts_with("file://") {
        // check_navigation allows this at the policy level; now enforce sandbox.
        match resolve_navigate_url(&manager.chrome_state.url, &url) {
            Some(u) => u,
            None => {
                tracing::warn!(
                    current = %manager.chrome_state.url,
                    target = %url,
                    "blocked: relative path escapes file:// sandbox"
                );
                manager.inspector_log.push_net_blocked(
                    "NAV",
                    &url,
                    "relative path escapes file:// sandbox".to_string(),
                );
                manager.chrome_state.loading = false;
                return;
            }
        }
    } else if url.starts_with("file://") && manager.chrome_state.url.starts_with("file://") {
        // Absolute file→file: sandbox check via resolve_navigate_url.
        match resolve_navigate_url(&manager.chrome_state.url, &url) {
            Some(u) => u,
            None => {
                tracing::warn!(
                    current = %manager.chrome_state.url,
                    target = %url,
                    "blocked: file:// target escapes sandbox"
                );
                manager.inspector_log.push_net_blocked(
                    "NAV",
                    &url,
                    "file:// target escapes sandbox".to_string(),
                );
                manager.chrome_state.loading = false;
                return;
            }
        }
    } else {
        url.clone()
    };

    // N2: all navigation decisions go through the policy choke point.
    match check_navigation(&manager.chrome_state.url, &resolved_url, &initiator) {
        NavigationVerdict::Allow(target) => {
            // N5: reset redirect chain for non-redirect initiators.
            if !matches!(initiator, NavigationInitiator::RedirectOf(_)) {
                manager.reset_redirect_count();
            }

            // ux-4: record the page being left, unless this navigation IS a
            // history step (back/forward restoring a prior entry) or a
            // mid-chain redirect continuation of one — those must not also
            // push a fresh history entry. This runs through the exact same
            // Allow branch as every other navigation, so history can never
            // become a choke-point bypass (N2).
            if !matches!(
                initiator,
                NavigationInitiator::HistoryStep | NavigationInitiator::RedirectOf(_)
            ) {
                manager.history.record_navigation(HistoryEntry {
                    url: manager.chrome_state.url.clone(),
                    scroll_y: manager.root_scroll_offset_y,
                });
            }

            // N5: update chrome state and reset capability policy.
            manager.chrome_state.url = target.clone();
            manager.capability_policy = CapabilityPolicy::new(&target);

            if target.starts_with("file://") {
                if let Some(path) = target.strip_prefix("file:///")
                    && let Ok(content) = std::fs::read_to_string(path)
                {
                    handle_navigate_success(manager, target, content);
                }
            } else if target.starts_with("mizu://") {
                manager.chrome_state.loading = true;
                manager
                    .inspector_log
                    .push_net_start("NAV", &target, Some(target.clone()));
                // N2: this is the ONLY site that emits NetworkCmd::Navigate.
                let _ = manager
                    .network_tx
                    .send(crate::network::NetworkCmd::Navigate { url: target });
            }
        }
        NavigationVerdict::Block(reason) => {
            tracing::warn!(
                current = %manager.chrome_state.url,
                target = %resolved_url,
                reason = reason,
                "navigation blocked by policy"
            );
            manager.inspector_log.push_net_blocked(
                "NAV",
                &resolved_url,
                reason.to_string(),
            );
            manager.chrome_state.loading = false;
            // A blocked history step must not leave a stale scroll restore
            // hanging around for some later, unrelated navigation.
            manager.pending_scroll_restore = None;
        }
    }
}

/// Steps back one entry in session history (the chrome Back button /
/// `Alt+Left`). A no-op when the back stack is empty — clicking a disabled
/// Back button fires no navigation.
///
/// Like every top-level navigation, this goes through [`navigate_to_url`]
/// (N2) with [`NavigationInitiator::HistoryStep`] — a Back/Forward click is a
/// real user gesture (N3), but the step must still pass through the single
/// choke point for scheme/origin/lifecycle handling (N4/N5) rather than
/// swapping `chrome_state.url` directly.
pub(super) fn navigate_back(manager: &mut MizuWindowManager) {
    let leaving = HistoryEntry {
        url: manager.chrome_state.url.clone(),
        scroll_y: manager.root_scroll_offset_y,
    };
    let Some(target) = manager.history.go_back(leaving) else {
        return;
    };
    manager.pending_scroll_restore = Some(target.scroll_y);
    navigate_to_url(manager, target.url, NavigationInitiator::HistoryStep);
}

/// Steps forward one entry in session history (the chrome Forward button /
/// `Alt+Right`). Symmetric to [`navigate_back`]; a no-op when the forward
/// stack is empty.
pub(super) fn navigate_forward(manager: &mut MizuWindowManager) {
    let leaving = HistoryEntry {
        url: manager.chrome_state.url.clone(),
        scroll_y: manager.root_scroll_offset_y,
    };
    let Some(target) = manager.history.go_forward(leaving) else {
        return;
    };
    manager.pending_scroll_restore = Some(target.scroll_y);
    navigate_to_url(manager, target.url, NavigationInitiator::HistoryStep);
}
