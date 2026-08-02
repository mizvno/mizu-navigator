#![forbid(unsafe_code)]

use crate::core::storage::ValidatedDomain;
use crate::core::types::{Value, VariableStore};
use crate::network::{RuntimeAction, UiEvent};

pub use mizu_core::security::quota::{
    CapabilityPolicy, STORAGE_QUOTA_BYTES_LOCAL_FILE, STORAGE_QUOTA_BYTES_LOCALHOST,
    STORAGE_QUOTA_BYTES_REMOTE, STORAGE_RATE_LIMIT_WRITES_PER_SEC, StorageUsageLedger,
};
pub(crate) use mizu_core::security::sandbox::file_sandbox_contains;
#[cfg(test)]
pub(crate) use mizu_core::security::sandbox::normalize_path_components;

/// Estimates the serialized byte size of a [`Value`].
///
/// Used by [`CapabilityPolicy::check_storage_write`] to decide how many bytes
/// a `StoreLocal` action would consume.  The estimate is conservative (it
/// ignores JSON overhead) so it can only under-count, which means the quota
/// check is slightly permissive — acceptable given the generous multiplier.
pub fn estimate_value_bytes(value: &Value) -> usize {
    match value {
        Value::String(s) => s.len(),
        Value::Int(_) => 8,
        Value::Decimal(_) => 8,
        Value::Bool(_) => 1,
        Value::Null => 4,
        Value::List(items) => items.iter().map(estimate_value_bytes).sum(),
        Value::Record(m) => m
            .iter()
            .map(|f| f.key.len() + estimate_value_bytes(&f.value))
            .sum(),
        // Never actually persisted — `storage`'s `to_json` conversion
        // rejects `FileHandle` outright (see `Value::FileHandle`'s doc
        // comment) — this estimate only needs to exist for the match to be
        // exhaustive.
        Value::FileHandle(handle) => handle.filename.len(),
    }
}

/// Derives a [`ValidatedDomain`] from a Mizu navigation URL.
///
/// * `mizu://host/path` → domain is `host`
/// * `file:///path`     → domain is derived from the canonical filesystem path
///   so that distinct local documents get isolated storage namespaces (and
///   therefore distinct AES keys / storage files) instead of all sharing a
///   single "local_file" namespace.  Canonicalise when possible for stability;
///   fall back to the raw path otherwise.
/// * Everything else    → domain string `"unknown"`
///
/// In all cases the resulting string is fed into [`ValidatedDomain::from_raw`]
/// so the final storage / keyring identifier is always the normalised SHA-256
/// hex digest — never a raw, potentially path-traversal-containing string.
pub fn get_current_domain(url: &str) -> ValidatedDomain {
    let raw = if let Some(rest) = url.strip_prefix("mizu://") {
        // Scan for '/', '?', or '#' — not just '/' — to match MizuUri::parse's strict
        // host boundary. Without this, `mizu://evil.com?q=x` yields domain "evil.com?q=x",
        // corrupting storage filenames and key derivations.
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        rest[..end].to_string()
    } else if let Some(path) = url.strip_prefix("file://") {
        let raw = path.trim_start_matches('/');
        let canonical = std::fs::canonicalize(raw)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        format!("file_{canonical}")
    } else {
        "unknown".to_string()
    };

    ValidatedDomain::from_raw(&raw)
}

/// Extracts the raw (un-hashed) domain string from a Mizu URL for use in
/// URL construction (e.g., `mizu://{domain}/path`).
///
/// Unlike [`get_current_domain`], this returns the actual hostname or a
/// filesystem-derived prefix — it must NOT be used as a storage or keyring
/// key directly.
pub fn get_raw_domain(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("mizu://") {
        // Same strict boundary as MizuUri::parse and get_current_domain: scan for
        // '/', '?', or '#' so query strings cannot bleed into the domain token.
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    if let Some(path) = url.strip_prefix("file://") {
        let raw = path.trim_start_matches('/');
        let canonical = std::fs::canonicalize(raw)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        return format!("file_{canonical}");
    }
    "unknown".to_string()
}

/// Builds the capability policy for a document loaded from `chrome_url`,
/// charging its storage writes to that origin's entry in `ledger`.
///
/// The single place a [`CapabilityPolicy`] is constructed in production, so
/// that the quota key is always [`get_current_domain`] — the origin's storage
/// identity, the very value that names its encrypted store and keyring entry.
/// Deriving the key anywhere else risks a second notion of "same origin" that
/// disagrees with the first, and any disagreement that *splits* one origin in
/// two is a quota bypass: the document reaches the same data through both keys
/// while each carries its own budget.
pub fn capability_policy_for(chrome_url: &str, ledger: &StorageUsageLedger) -> CapabilityPolicy {
    CapabilityPolicy::new(
        chrome_url,
        get_current_domain(chrome_url).as_str().to_string(),
        ledger.clone(),
    )
}

/// Outcome of a capability dispatch, reported to the caller so the UI (e.g.
/// the inspector's network log) can show what actually happened — in
/// particular when an action was blocked by policy before leaving the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityOutcome {
    /// The action was forwarded to the responsible subsystem.
    Dispatched,
    /// The action was rejected by a client-side policy; the reason is
    /// human-readable and safe to display.
    Blocked(String),
}

/// Executes a declarative capability action, enforcing per-origin policy.
///
/// `policy` tracks storage quota and rate limits for the current origin; it
/// is mutated on every `StoreLocal` that passes the gate.
///
/// `CopyToClipboard` actions are intercepted upstream in the window manager
/// (gesture-activation check + DOM-node text extraction) and must **not**
/// reach this function.  If one does, it is discarded with a warning.
pub fn execute_capability_action(
    store: &mut VariableStore,
    network_tx: &tokio::sync::mpsc::UnboundedSender<crate::network::NetworkCmd>,
    logic_tx: &std::sync::mpsc::Sender<(crate::network::TabId, UiEvent)>,
    tab_id: crate::network::TabId,
    chrome_url: &str,
    policy: &mut CapabilityPolicy,
    action: RuntimeAction,
) -> CapabilityOutcome {
    match action {
        RuntimeAction::None => CapabilityOutcome::Dispatched,
        RuntimeAction::ResolvedCall {
            method,
            url,
            payload,
            target_variable,
            format,
            headers,
        } => {
            // Block outbound calls from file:// origins to non-local mizu:// hosts.
            // Prevents SSRF and exfiltration of local data to attacker-controlled servers.
            //
            // Use MizuUri::parse to extract the structural domain — never raw substring
            // search. `mizu://evil.com/path?q=localhost` would defeat `.contains("localhost")`.
            let target_is_remote_mizu = url.starts_with("mizu://")
                && crate::network::uri::MizuUri::parse(&url)
                    .map(|u| !crate::network::worker::is_local_host(&u.domain))
                    .unwrap_or(true); // parse failure → fail-secure: treat as remote
            if chrome_url.starts_with("file://") && target_is_remote_mizu {
                let reason =
                    format!("file:// origin blocked from outbound call to remote host {url}");
                tracing::warn!(url = %url, "SecurityViolation: {reason}");
                return CapabilityOutcome::Blocked(reason);
            }
            // `NetworkCmd`/`NetworkResult` cross into the network worker
            // thread, which holds no interner at all — so `target_var` must
            // be the resolved name, not the Symbol. `target_variable` was
            // interned at parse time (before freeze), so it is always
            // present in this thread's own interner; resolving it here
            // (rather than carrying the Symbol across threads) is what lets
            // the eventual `NetworkResult` response be applied via
            // `UiEvent::UpdateVariable` + `set_runtime` without either side
            // needing to agree on a post-freeze Symbol ID.
            let target_var = match store.interner.resolve(target_variable) {
                Some(name) => name.to_string(),
                None => {
                    tracing::warn!(
                        symbol = target_variable.0,
                        "ResolvedCall target_variable not found in interner; dropping Fetch"
                    );
                    return CapabilityOutcome::Blocked(
                        "internal error: unresolvable target variable".to_string(),
                    );
                }
            };
            if let Err(e) = network_tx.send(crate::network::NetworkCmd::Fetch {
                tab: tab_id,
                method,
                url,
                target_var,
                is_remote_origin: chrome_url.starts_with("mizu://"),
                payload,
                format,
                headers,
            }) {
                tracing::warn!(error = %e, "network channel closed; Fetch command dropped");
            }
            CapabilityOutcome::Dispatched
        }
        RuntimeAction::StoreLocal { key, value } => {
            let byte_count = estimate_value_bytes(&value);
            if let Err(e) = policy.check_storage_write(byte_count) {
                tracing::warn!(error = %e, key = %key, "StorageStore blocked by capability policy");
                return CapabilityOutcome::Blocked(e.to_string());
            }
            // Offload the entire storage operation (keyring IPC + filesystem
            // write) to the network worker's Tokio blocking pool so the UI
            // thread is never stalled.
            let domain = get_raw_domain(chrome_url);
            if let Err(e) =
                network_tx.send(crate::network::NetworkCmd::StorageStore { domain, key, value })
            {
                tracing::warn!(error = %e, "network channel closed; StorageStore command dropped");
            }
            CapabilityOutcome::Dispatched
        }
        RuntimeAction::CopyToClipboard { .. } => {
            // Must be intercepted and handled in window.rs (gesture + DOM lookup).
            // Reaching here means the intercept was bypassed — discard silently.
            tracing::warn!(
                "CopyToClipboard reached execute_capability_action — should have been intercepted upstream"
            );
            CapabilityOutcome::Blocked("clipboard action bypassed the gesture gate".to_string())
        }
        RuntimeAction::GetSystemTime { target_variable } => {
            let now = std::time::SystemTime::now();
            let duration = now
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let time_ms = duration.as_millis() as i64;
            // Resolve to a name for the same reason as ResolvedCall above:
            // `UiEvent::UpdateVariable` carries a String precisely so the
            // worker never has to trust a Symbol minted by this thread.
            match store.interner.resolve(target_variable) {
                Some(name) => {
                    if let Err(e) = logic_tx.send((
                        tab_id,
                        UiEvent::UpdateVariable {
                            name: name.to_string(),
                            // Milliseconds are a whole count, so this is an
                            // `Int`. As a `Decimal` it would have meant
                            // `time_ms / DECIMAL_SCALE`.
                            value: Value::Int(time_ms),
                        },
                    )) {
                        tracing::warn!(error = %e, "logic channel closed; GetSystemTime update dropped");
                    }
                }
                None => {
                    tracing::warn!(
                        symbol = target_variable.0,
                        "GetSystemTime target_variable not found in interner; dropping update"
                    );
                }
            }
            CapabilityOutcome::Dispatched
        }
        RuntimeAction::Navigate { url } => {
            // Must be intercepted upstream in `event_loop.rs`, which is the
            // only place that knows this batch's agency (`WorkerResponse::
            // gesture`) and owns the navigation choke point `navigate_to_url`.
            // Reaching here means the intercept was bypassed, and there is no
            // honest initiator to reconstruct at this point — emitting
            // `NetworkCmd::Navigate` anyway would mean inventing one. Blocked,
            // exactly like `CopyToClipboard` below (N2).
            tracing::warn!(
                url = %url,
                "Navigate reached execute_capability_action — should have been \
                 intercepted upstream by the navigation choke point"
            );
            CapabilityOutcome::Blocked(
                "navigation action bypassed the navigation choke point".to_string(),
            )
        }
        RuntimeAction::NetworkCall {
            method,
            endpoint_symbol,
            payload,
            path_param,
            target_variable,
            format,
            headers,
        } => {
            if let Err(e) = network_tx.send(crate::network::NetworkCmd::NetworkRequest {
                tab: tab_id,
                request: crate::network::NetworkRequest {
                    endpoint_symbol,
                    method,
                    payload,
                    path_param,
                    target_variable,
                    format,
                    headers,
                },
            }) {
                tracing::warn!(error = %e, "network channel closed; NetworkRequest command dropped");
            }
            CapabilityOutcome::Dispatched
        }
        RuntimeAction::DownloadMedia { url } => {
            // Same file:// -> remote-host block as the ResolvedCall arm
            // above: a `media` alias is a declared, absolute `mizu://` URL,
            // so without this check a local document could reach an
            // attacker-controlled host merely by embedding or downloading
            // an image, bypassing the outbound-call SSRF guard entirely.
            let target_is_remote_mizu = url.starts_with("mizu://")
                && crate::network::uri::MizuUri::parse(&url)
                    .map(|u| !crate::network::worker::is_local_host(&u.domain))
                    .unwrap_or(true); // parse failure -> fail-secure: treat as remote
            if chrome_url.starts_with("file://") && target_is_remote_mizu {
                let reason = format!("file:// origin blocked from downloading remote media {url}");
                tracing::warn!(url = %url, "SecurityViolation: {reason}");
                return CapabilityOutcome::Blocked(reason);
            }
            tracing::info!(url = %url, "download media requested");
            if let Err(e) = network_tx.send(crate::network::NetworkCmd::FetchImage {
                tab: tab_id,
                url,
                is_remote_origin: chrome_url.starts_with("mizu://"),
                sandbox_base: if chrome_url.starts_with("file://") {
                    chrome_url
                        .strip_prefix("file:///")
                        .and_then(|p| std::path::Path::new(p).parent())
                        .map(|d| d.to_string_lossy().into_owned())
                } else {
                    None
                },
            }) {
                tracing::warn!(error = %e, "network channel closed; FetchImage command dropped");
            }
            CapabilityOutcome::Dispatched
        }
        RuntimeAction::DownloadAlias { .. } => {
            tracing::warn!("unresolved DownloadAlias reached capability executor");
            CapabilityOutcome::Blocked("unresolved download alias".to_string())
        }
    }
}

#[cfg(test)]
mod tests;
