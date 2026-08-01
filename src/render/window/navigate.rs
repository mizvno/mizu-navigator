//! URL resolution and navigation/network-result handling.

use rustc_hash::FxHashMap;
use std::collections::HashMap;

use crate::core::types::Value;
use crate::render::navigation::{NavigationInitiator, NavigationVerdict, check_navigation};

use super::AssetSlot;
use super::history::{HistoryEntry, VisitRecord};
use super::manager::{
    MAX_REDIRECTS, ReloadedDocument, TabState, WindowCtx, reload_tab_document, resize_tab_viewport,
};

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
pub(super) fn handle_navigate_success(
    tab: &mut TabState,
    ctx: &mut WindowCtx<'_>,
    url: String,
    source: String,
) {
    tracing::debug!(url = %url, "navigate success");
    // N5: this is the single commit point. The origin only changes here —
    // together with the document it belongs to — so the previous document is
    // never left running under the next document's origin (see
    // `ChromeState::committed_url`). `capability_policy` is rebuilt from the
    // same string in the same breath, so the quota tier and the storage domain
    // can never describe a different origin than the code that is executing.
    tab.chrome_state.committed_url = url.clone();
    tab.capability_policy = crate::render::security::capability_policy_for(&url, ctx.storage_usage);
    // ux-7: only the *displayed* URL is stripped of bidi override/isolate
    // controls; `committed_url` keeps the real string every origin comparison
    // and every fetch is made against.
    tab.chrome_state
        .set_displayed_url(crate::render::bidi::strip_bidi_overrides(&url).into_owned());
    tab.chrome_state.loading = false;
    tab.reset_redirect_count();
    tab.store
        .set_runtime("window_url", crate::core::types::Value::from(url.clone()));

    // `Origin::Network` unconditionally, including for the `file://` fast path
    // in `navigate_to_url` — so a document loaded by *navigating* never gets
    // `import`/`include`, even when it came off local disk. Deliberate, and
    // the conservative side of an inconsistency worth naming rather than
    // quietly living with:
    //
    // * Only startup (`main.rs`) passes `Origin::LocalFile`. Imports therefore
    //   work for the document the browser was launched with and silently stop
    //   working the moment the user follows a link to a sibling file.
    // * `current_dir` below is the *process's* working directory. Startup does
    //   not have that problem — it derives its base from the document's own
    //   parent directory — but this call site has nothing to do with where the
    //   document lives, which is precisely why it must not be paired with
    //   `Origin::LocalFile`. `splitter::process_import` canonicalises the base
    //   and the resolved file and requires containment, so a wrong base can
    //   only refuse imports, never widen them; still, "the sandbox root is
    //   whatever directory the binary was started from" is not a root anyone
    //   should be relying on.
    //
    // Closing the gap properly means threading the *document's* directory in
    // (from `url` when it is `file://`) and passing `Origin::LocalFile` only
    // then — not flipping the flag on its own. Until that happens, erring
    // toward `Network` costs local documents a feature and costs network
    // documents nothing: an `import` is a local file read, the containment
    // check is the only thing bounding it, and a document an attacker controls
    // must not get one at all.
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
                    500,
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
            let (style_rules, style_variants) = if !blocks.style_block.trim().is_empty() {
                match crate::parser::style::parse_style_with_variants(&blocks.style_block) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = ?e, "style parse error during navigation");
                        (HashMap::new(), Vec::new())
                    }
                }
            } else {
                (HashMap::new(), Vec::new())
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
                &logic_fns,
            ) {
                Ok(dom) => {
                    // Check Static Types (Phase B)
                    if let Err(e) = crate::parser::typecheck::check_types(
                        &dom,
                        &new_root_timers,
                        &logic_fns,
                        &new_computed,
                        &new_interner,
                    ) {
                        tracing::error!(error = ?e, "static type check error");
                        return; // Reject document load
                    }

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
                            tab.inspector.flow_metrics = Some(metrics);
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, "flow check error");
                            return; // Reject document load
                        }
                    }

                    tab.url_registry = new_url_registry;
                    if let Err(e) = reload_tab_document(
                        tab,
                        ctx,
                        ReloadedDocument {
                            dom,
                            style_rules,
                            style_variants,
                            logic_fns,
                            interner: new_interner,
                            computed_bindings: new_computed,
                            root_timers: new_root_timers,
                        },
                        // A background tab finishing a load must not rename the
                        // window the user is looking at.
                        tab.id == ctx.active_tab_id,
                    ) {
                        tracing::error!(error = ?e, "document reload error");
                    } else {
                        tracing::debug!("document reloaded");
                        // The sidebar log records *arrivals*, not departures:
                        // recording the page being left would leave whatever
                        // is currently on screen missing from the history
                        // until the user navigated away from it. Here the new
                        // document is already installed, so its `doc` title —
                        // the same source `retitle_window` and the tab strip
                        // read — is available to store alongside the URL.
                        let title = tab
                            .dom
                            .root()
                            .value()
                            .attributes
                            .get("title")
                            .cloned()
                            .unwrap_or_default();
                        ctx.history_log.push(VisitRecord::new(
                            tab.chrome_state.committed_url.clone(),
                            title,
                        ));
                        // ux-4: restore scroll position after a history
                        // (Back/Forward) step. `reload_document` always
                        // resets `root_scroll_offset_y` to 0.0 first, so this
                        // must run after it. A `None` here (the overwhelming
                        // majority of navigations, which aren't history
                        // steps) is a no-op.
                        if let Some(scroll_y) = tab.pending_scroll_restore.take() {
                            tab.root_scroll_offset_y = scroll_y;
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
pub(super) fn process_network_result(
    tab: &mut TabState,
    ctx: &mut WindowCtx<'_>,
    res: crate::network::NetworkResult,
) {
    use crate::network::NetworkResult;
    use crate::render::inspector::log::NetOutcome;
    match res {
        NetworkResult::Success {
            tab: _,
            target_var,
            data,
        } => {
            let bytes = match &data {
                Value::String(s) => Some(s.len()),
                _ => None,
            };
            tab.inspector_log
                .complete_net(&target_var, NetOutcome::Ok, bytes);
            // `UiEvent::UpdateVariable` carries the resolved name, not a
            // Symbol: `target_var` already is that name (see the
            // `/* FIX SYMBOL */` fix in `execute_capability_action`), and
            // the worker resolves it against its own frozen interner via
            // `set_runtime` — no interner lookup needed on this side at all.
            let _ = ctx.logic_tx.send((
                tab.id,
                crate::network::UiEvent::UpdateVariable {
                    name: target_var,
                    value: data,
                },
            ));
            let _ = ctx.logic_tx.send((
                tab.id,
                crate::network::UiEvent::UpdateVariable {
                    name: "stato_navigazione".to_string(),
                    value: crate::core::types::Value::from("Completato.".to_string()),
                },
            ));
        }
        NetworkResult::FetchFailed {
            tab: _,
            target_var,
            error,
        } => {
            tracing::error!(error = ?error, target = %target_var, "fetch failed");
            tab.inspector_log.complete_net(
                &target_var,
                NetOutcome::Failed(error.to_string()),
                None,
            );
            // Write a readable error where the response would have gone, so
            // the document shows it (e.g. `Status: error: connection refused`).
            let _ = ctx.logic_tx.send((
                tab.id,
                crate::network::UiEvent::UpdateVariable {
                    name: target_var,
                    value: crate::core::types::Value::from(format!("error: {error}")),
                },
            ));
        }
        NetworkResult::Error(_, e) => {
            tracing::error!(error = ?e, "network error");
            tab.inspector_log
                .complete_latest_pending(NetOutcome::Failed(e.to_string()));
            tab.chrome_state.loading = false;
            let _ = ctx.logic_tx.send((
                tab.id,
                crate::network::UiEvent::UpdateVariable {
                    name: "stato_navigazione".to_string(),
                    value: crate::core::types::Value::from(format!("Errore: {e}")),
                },
            ));
        }
        NetworkResult::NavigateSuccess {
            tab: _,
            url,
            source,
        } => {
            tab.inspector_log
                .complete_net(&url, NetOutcome::Ok, Some(source.len()));
            handle_navigate_success(tab, ctx, url, source);
        }
        NetworkResult::NavigationRedirect {
            tab: _,
            new_url,
            initiator,
        } => {
            tab.inspector_log
                .complete_latest_pending(NetOutcome::Redirect);
            if tab.register_redirect() {
                tracing::debug!(
                    url = %new_url,
                    count = tab.redirect_count,
                    "redirecting (through choke point)"
                );
                // N2+N5: route through the single choke point so scheme,
                // origin, gesture, and lifecycle checks all apply.
                //
                // N3: the redirect inherits the agency of the navigation it
                // continues, and nothing more. `initiator` is the value the
                // originating `navigate_to_url` put on the command and the
                // worker echoed back untouched, so a user-gesture navigation
                // that redirects cross-origin stays allowed while a
                // document-logic one stays blocked. Substituting a synthetic
                // `UserGesture` here — as this site did before — let any
                // server convert a same-origin logic fetch into an authorised
                // cross-origin navigation with one `Location` header.
                navigate_to_url(tab, ctx, new_url, initiator.redirect_of());
            } else {
                tracing::error!(
                    limit = *MAX_REDIRECTS,
                    "redirect limit exceeded; aborting navigation"
                );
                tab.chrome_state.loading = false;
                let _ = ctx.logic_tx.send((
                    tab.id,
                    crate::network::UiEvent::UpdateVariable {
                        name: "stato_navigazione".to_string(),
                        value: crate::core::types::Value::from(
                            "Errore: troppi redirect".to_string(),
                        ),
                    },
                ));
            }
        }
        NetworkResult::FetchImageSuccess { tab: _, url, image } => {
            tab.inspector_log.push_net_done("IMG", &url, NetOutcome::Ok);
            ctx.image_cache.put(url.clone(), AssetSlot::Ready(image));
            rebuild_tab_taffy_after_image(tab, ctx);
        }
        NetworkResult::FetchImageFailed { tab: _, url, error } => {
            tab.inspector_log
                .push_net_done("IMG", &url, NetOutcome::Failed(error.to_string()));
            ctx.image_cache.put(url.clone(), AssetSlot::Failed);
            tracing::error!(url = %url, error = ?error, "image load failed");
        }
    }

    // Request a layout recalc + redraw for every network result.
    // Clone the Arc so resize_viewport can take &mut manager without a borrow conflict.
    if let Some(w) = ctx.window.cloned() {
        let physical_size = w.inner_size();
        let logical_width = physical_size.width as f32 / w.scale_factor() as f32;
        let logical_height = physical_size.height as f32 / w.scale_factor() as f32;
        let _ = resize_tab_viewport(tab, ctx, logical_width, logical_height, None);
        w.request_redraw();
    }
}

/// Rebuilds `tab`'s taffy tree after an image finished loading: an image's
/// intrinsic size participates in layout, so the tree built while the slot was
/// `Loading` is stale.
///
/// Applied to every tab that was waiting on that URL, not just the one that
/// requested it first — the decoded-image cache is shared and URL-keyed, so a
/// second tab showing the same image is deduped at request time and would
/// otherwise never be told the bytes arrived.
pub(super) fn rebuild_tab_taffy_after_image(tab: &mut TabState, ctx: &mut WindowCtx<'_>) {
    let mut new_taffy = taffy::TaffyTree::new();
    let mut new_node_map = HashMap::new();
    let env = crate::render::responsive::RenderEnvironment {
        viewport: tab.viewport_size,
        color_scheme: ctx.preferences.color_scheme,
    };
    match crate::render::layout_bridge::build_taffy_tree(
        tab.dom.root(),
        &mut crate::render::layout_bridge::TaffyBuildContext {
            style_rules_map: &tab.style_rules,
            taffy: &mut new_taffy,
            node_to_taffy_id: &mut new_node_map,
            image_cache: ctx.image_cache,
            chrome_url: &tab.chrome_state.committed_url,
            variants: &tab.style_variants,
            env: &env,
        },
    ) {
        Ok(new_root) => {
            tab.taffy = new_taffy;
            tab.node_to_taffy_id = new_node_map;
            tab.root_taffy_id = new_root;
        }
        Err(e) => {
            tracing::error!(error = ?e, "taffy rebuild failed after image fetch");
        }
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
/// - The redirect chain counter is reset for non-redirect initiators.
/// - `file://` documents are loaded directly; `mizu://` dispatches
///   `NetworkCmd::Navigate` to the network worker.
///
/// # The origin does not move here (N5)
///
/// Dispatching a navigation deliberately leaves `chrome_state.committed_url`
/// and `capability_policy` alone: they change only when a document actually
/// commits, in [`handle_navigate_success`]. A `mizu://` navigation is answered
/// asynchronously and may never be answered at all (NXDOMAIN, a timeout, a
/// `SecurityViolation`), while the previous document keeps running with its
/// DOM, its logic and its root timers intact. Relabelling the origin at
/// dispatch time would hand that still-running document the *target's* origin:
/// a local `file://` document could shed the `file://`→remote call block —
/// and with it the exfiltration guard on every `media` endpoint it
/// declares — by doing nothing more than following one link to a host that
/// does not resolve. The current URL read below is the committed one for the
/// same reason: the URL-bar text is an editing buffer, so anything decided
/// from it would change under the user's keystrokes.
pub(super) fn navigate_to_url(
    tab: &mut TabState,
    ctx: &mut WindowCtx<'_>,
    url: String,
    initiator: NavigationInitiator,
) {
    // ux-4: a pending scroll restore only ever belongs to the history step
    // that set it. Any other navigation (fresh, logic, redirect) starting
    // here must not inherit a stale value from an earlier, unrelated step.
    if !matches!(initiator, NavigationInitiator::HistoryStep) {
        tab.pending_scroll_restore = None;
    }

    if url == "about:blank" {
        handle_navigate_success(tab, ctx, url, "layout\n  doc\n".to_string());
        return;
    }

    // The origin every check below is made against is the *committed* one —
    // the document actually loaded in this tab — never the URL-bar text.
    let current_url = tab.chrome_state.committed_url.clone();

    // For file:// origins with relative paths, we still need resolve_navigate_url
    // for sandbox enforcement (it does I/O via canonicalize).  check_navigation
    // handles the pure policy, then we do the I/O-dependent resolution.
    let resolved_url = if !url.contains("://") && current_url.starts_with("file://") {
        // check_navigation allows this at the policy level; now enforce sandbox.
        match resolve_navigate_url(&current_url, &url) {
            Some(u) => u,
            None => {
                tracing::warn!(
                    current = %current_url,
                    target = %url,
                    "blocked: relative path escapes file:// sandbox"
                );
                tab.inspector_log.push_net_blocked(
                    "NAV",
                    &url,
                    "relative path escapes file:// sandbox".to_string(),
                );
                tab.chrome_state.loading = false;
                return;
            }
        }
    } else if url.starts_with("file://") && current_url.starts_with("file://") {
        // Absolute file→file: sandbox check via resolve_navigate_url.
        match resolve_navigate_url(&current_url, &url) {
            Some(u) => u,
            None => {
                tracing::warn!(
                    current = %current_url,
                    target = %url,
                    "blocked: file:// target escapes sandbox"
                );
                tab.inspector_log.push_net_blocked(
                    "NAV",
                    &url,
                    "file:// target escapes sandbox".to_string(),
                );
                tab.chrome_state.loading = false;
                return;
            }
        }
    } else {
        url.clone()
    };

    // N2: all navigation decisions go through the policy choke point.
    match check_navigation(&current_url, &resolved_url, &initiator) {
        NavigationVerdict::Allow(target) => {
            // N5: reset redirect chain for non-redirect initiators.
            if !matches!(initiator, NavigationInitiator::RedirectOf(_)) {
                tab.reset_redirect_count();
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
                tab.history.record_navigation(HistoryEntry {
                    url: current_url.clone(),
                    scroll_y: tab.root_scroll_offset_y,
                });
            }

            // N5: the origin is *not* moved here. `committed_url` and
            // `capability_policy` are installed by `handle_navigate_success`,
            // together with the document they describe — see this function's
            // doc comment. The address bar likewise keeps showing the page the
            // user is actually looking at until the new one commits, so a
            // navigation that stalls or fails cannot leave the bar attesting
            // to a document that was never loaded.
            if target.starts_with("file://") {
                if let Some(path) = target.strip_prefix("file:///")
                    && let Ok(content) = std::fs::read_to_string(path)
                {
                    handle_navigate_success(tab, ctx, target, content);
                }
            } else if target.starts_with("mizu://") {
                tab.chrome_state.loading = true;
                tab.inspector_log
                    .push_net_start("NAV", &target, Some(target.clone()));
                // N2: this is the ONLY site that emits NetworkCmd::Navigate.
                // The initiator rides along so that a 3xx answer to this
                // request re-enters this function with the agency it actually
                // had, instead of one reconstructed after the fact (N3).
                let _ = ctx.network_tx.send(crate::network::NetworkCmd::Navigate {
                    tab: tab.id,
                    url: target,
                    initiator,
                });
            }
        }
        NavigationVerdict::Block(reason) => {
            tracing::warn!(
                current = %current_url,
                target = %resolved_url,
                reason = reason,
                "navigation blocked by policy"
            );
            tab.inspector_log
                .push_net_blocked("NAV", &resolved_url, reason.to_string());
            tab.chrome_state.loading = false;
            // A blocked history step must not leave a stale scroll restore
            // hanging around for some later, unrelated navigation.
            tab.pending_scroll_restore = None;
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
/// swapping the tab's URL directly.
pub(super) fn navigate_back(tab: &mut TabState, ctx: &mut WindowCtx<'_>) {
    // The entry being left is the document actually loaded, not whatever the
    // user has half-typed into the bar.
    let leaving = HistoryEntry {
        url: tab.chrome_state.committed_url.clone(),
        scroll_y: tab.root_scroll_offset_y,
    };
    let Some(target) = tab.history.go_back(leaving) else {
        return;
    };
    tab.pending_scroll_restore = Some(target.scroll_y);
    navigate_to_url(tab, ctx, target.url, NavigationInitiator::HistoryStep);
}

/// Steps forward one entry in session history (the chrome Forward button /
/// `Alt+Right`). Symmetric to [`navigate_back`]; a no-op when the forward
/// stack is empty.
pub(super) fn navigate_forward(tab: &mut TabState, ctx: &mut WindowCtx<'_>) {
    let leaving = HistoryEntry {
        url: tab.chrome_state.committed_url.clone(),
        scroll_y: tab.root_scroll_offset_y,
    };
    let Some(target) = tab.history.go_forward(leaving) else {
        return;
    };
    tab.pending_scroll_restore = Some(target.scroll_y);
    navigate_to_url(tab, ctx, target.url, NavigationInitiator::HistoryStep);
}
