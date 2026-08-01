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
                let reason =
                    format!("file:// origin blocked from downloading remote media {url}");
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
mod tests {
    use super::{
        STORAGE_QUOTA_BYTES_REMOTE, STORAGE_RATE_LIMIT_WRITES_PER_SEC, StorageUsageLedger,
        capability_policy_for, estimate_value_bytes, get_current_domain,
    };
    use crate::core::errors::MizuError;
    use crate::core::types::Value;
    use std::sync::Arc;

    #[test]
    fn test_storage_quota_enforcement() {
        let mut policy =
            capability_policy_for("mizu://example.com/index.mizu", &StorageUsageLedger::new());
        assert_eq!(policy.quota_bytes, STORAGE_QUOTA_BYTES_REMOTE);

        // Write a value just under the quota — must succeed.
        let large = "x".repeat(STORAGE_QUOTA_BYTES_REMOTE - 1);
        let val = Value::String(Arc::from(large.as_str()));
        let bytes = estimate_value_bytes(&val);
        policy
            .check_storage_write(bytes)
            .expect("write under quota should succeed");

        // Next write (1 byte) would exceed the quota — must be rejected.
        let result = policy.check_storage_write(2);
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "write over quota must return SecurityViolation, got: {result:?}"
        );
    }

    #[test]
    fn localhost_gets_larger_quota() {
        let remote =
            capability_policy_for("mizu://example.com/index.mizu", &StorageUsageLedger::new());
        let local =
            capability_policy_for("mizu://localhost/index.mizu", &StorageUsageLedger::new());
        assert!(local.quota_bytes > remote.quota_bytes);
    }

    #[test]
    fn rate_limit_blocks_excess_writes() {
        let mut policy =
            capability_policy_for("mizu://example.com/index.mizu", &StorageUsageLedger::new());
        for _ in 0..STORAGE_RATE_LIMIT_WRITES_PER_SEC {
            policy
                .check_storage_write(1)
                .expect("write within rate limit should succeed");
        }
        let result = policy.check_storage_write(1);
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "write exceeding rate limit must return SecurityViolation, got: {result:?}"
        );
    }

    #[test]
    fn estimate_value_bytes_string_is_len() {
        let s = "hello";
        let v = Value::String(Arc::from(s));
        assert_eq!(estimate_value_bytes(&v), s.len());
    }

    #[test]
    fn file_origin_gets_local_file_quota() {
        let file_policy = capability_policy_for(
            "file:///home/user/app/index.mizu",
            &StorageUsageLedger::new(),
        );
        let remote_policy =
            capability_policy_for("mizu://example.com/index.mizu", &StorageUsageLedger::new());
        let local_policy =
            capability_policy_for("mizu://localhost/index.mizu", &StorageUsageLedger::new());
        // file:// quota must be strictly larger than remote but smaller than localhost.
        assert!(file_policy.quota_bytes > remote_policy.quota_bytes);
        assert!(file_policy.quota_bytes < local_policy.quota_bytes);
    }

    #[test]
    fn normalize_path_resolves_dotdot() {
        use std::path::Path;
        let normalized =
            super::normalize_path_components(Path::new("home/user/app/../../etc/passwd"));
        // Lexically: home/user/app → home/user → home → home/etc → home/etc/passwd
        assert_eq!(normalized, Path::new("home/etc/passwd"));
    }

    #[test]
    fn normalize_path_escape_above_root_returns_empty() {
        use std::path::Path;
        // Attempting to go above the implicit root on a relative path.
        let normalized = super::normalize_path_components(Path::new("../../etc/passwd"));
        assert!(
            normalized.as_os_str().is_empty(),
            "escaping above root must yield an empty PathBuf, got: {normalized:?}"
        );
    }

    #[test]
    fn file_sandbox_contains_same_dir_is_true() {
        use std::path::Path;
        assert!(super::file_sandbox_contains(
            Path::new("home/user/app"),
            Path::new("home/user/app/about.mizu"),
        ));
    }

    #[test]
    fn file_sandbox_contains_existing_base_missing_target() {
        // Regression: on Windows, canonicalising an EXISTING base yields a
        // verbatim path (`\\?\C:\…`) while a MISSING target falls back to the
        // lexical form — the mixed prefixes made starts_with always fail, so
        // any not-yet-existing file inside the sandbox was reported as an
        // escape.  A missing file directly inside an existing base must be
        // contained.
        let base = std::env::temp_dir();
        let target = base.join("mizu_sandbox_regression_missing_file.bin");
        assert!(
            super::file_sandbox_contains(&base, &target),
            "missing file inside an existing sandbox base must be contained"
        );
    }

    #[test]
    fn file_sandbox_contains_traversal_is_false() {
        use std::path::Path;
        // target lexically resolves to "home/etc/passwd" which is NOT inside "home/user/app"
        assert!(!super::file_sandbox_contains(
            Path::new("home/user/app"),
            Path::new("home/user/app/../../etc/passwd"),
        ));
    }

    #[test]
    fn distinct_file_urls_get_distinct_domains() {
        // Two different local documents must map to two distinct storage
        // domains (and therefore distinct encryption keys / storage files).
        let a = get_current_domain("file:///tmp/mizu_app_a/index.mizu");
        let b = get_current_domain("file:///tmp/mizu_app_b/index.mizu");
        assert_ne!(
            a.as_str(),
            b.as_str(),
            "different file paths must yield different domains"
        );
        // Both must be 64-char hex digests.
        assert_eq!(a.as_str().len(), 64);
        assert_eq!(b.as_str().len(), 64);
    }

    #[test]
    fn mizu_url_domain_is_deterministic() {
        let a = get_current_domain("mizu://example.com/index.mizu");
        let b = get_current_domain("mizu://example.com/other.mizu");
        // Same host, different path → same domain digest.
        assert_eq!(a.as_str(), b.as_str());
    }

    #[test]
    fn different_mizu_hosts_yield_different_digests() {
        let a = get_current_domain("mizu://app-a.mizu/index.mizu");
        let b = get_current_domain("mizu://app-b.mizu/index.mizu");
        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn get_current_domain_strips_query_from_mizu_url() {
        // `mizu://evil.com?q=localhost` must NOT hash `evil.com?q=localhost` —
        // that would create a distinct key bucket from legitimate `evil.com` traffic,
        // AND the raw domain would embed the query string, bypassing SSRF guards.
        let a = get_current_domain("mizu://evil.com?q=localhost");
        let b = get_current_domain("mizu://evil.com");
        assert_eq!(
            a.as_str(),
            b.as_str(),
            "query string must not change the storage domain digest"
        );
    }

    #[test]
    fn get_current_domain_strips_fragment_from_mizu_url() {
        let a = get_current_domain("mizu://evil.com#frag");
        let b = get_current_domain("mizu://evil.com");
        assert_eq!(
            a.as_str(),
            b.as_str(),
            "fragment must not change the storage domain digest"
        );
    }

    #[test]
    fn get_raw_domain_strips_query_and_fragment() {
        use super::get_raw_domain;
        assert_eq!(get_raw_domain("mizu://evil.com?q=x"), "evil.com");
        assert_eq!(get_raw_domain("mizu://evil.com#frag"), "evil.com");
        assert_eq!(get_raw_domain("mizu://evil.com/path?q=x"), "evil.com");
    }

    #[test]
    fn get_raw_domain_clean_url_unchanged() {
        use super::get_raw_domain;
        assert_eq!(
            get_raw_domain("mizu://example.opennic/page"),
            "example.opennic"
        );
        assert_eq!(get_raw_domain("mizu://example.opennic"), "example.opennic");
    }

    #[test]
    fn capability_policy_query_injection_cannot_grant_localhost_quota() {
        // `mizu://evil.com?host=localhost` must receive REMOTE quota, not LOCALHOST.
        // The old `.contains("localhost")` would have granted the larger quota.
        let policy = super::capability_policy_for(
            "mizu://evil.com?host=localhost",
            &StorageUsageLedger::new(),
        );
        assert_eq!(
            policy.quota_bytes,
            super::STORAGE_QUOTA_BYTES_REMOTE,
            "query-injected 'localhost' must not elevate quota to localhost tier"
        );
    }

    #[test]
    fn capability_policy_credential_injection_cannot_grant_localhost_quota() {
        // `mizu://localhost@evil.com/` — MizuUri rejects '@' in domain, so we
        // fall back to REMOTE. The old `.contains("localhost")` would have granted
        // the larger quota by matching the user-info part of the raw URL string.
        let policy =
            super::capability_policy_for("mizu://localhost@evil.com/", &StorageUsageLedger::new());
        // MizuUri::parse rejects this → parse fails → fallback to REMOTE.
        assert_eq!(
            policy.quota_bytes,
            super::STORAGE_QUOTA_BYTES_REMOTE,
            "credential-stuffed URL must not elevate quota via localhost substring"
        );
    }

    #[test]
    fn capability_policy_real_localhost_gets_localhost_quota() {
        let policy =
            super::capability_policy_for("mizu://localhost/app", &StorageUsageLedger::new());
        assert_eq!(
            policy.quota_bytes,
            super::STORAGE_QUOTA_BYTES_LOCALHOST,
            "genuine localhost origin must receive localhost quota"
        );
    }

    #[test]
    fn capability_policy_ip_127_gets_localhost_quota() {
        let policy =
            super::capability_policy_for("mizu://127.0.0.1/app", &StorageUsageLedger::new());
        assert_eq!(
            policy.quota_bytes,
            super::STORAGE_QUOTA_BYTES_LOCALHOST,
            "loopback IP origin must receive localhost quota"
        );
    }

    #[test]
    fn capability_policy_file_origin_gets_local_file_quota_regardless_of_path() {
        // Even if the file path contains the word "localhost", it must get LOCAL_FILE quota.
        let policy = super::capability_policy_for(
            "file:///home/user/localhost-app/index.mizu",
            &StorageUsageLedger::new(),
        );
        assert_eq!(
            policy.quota_bytes,
            super::STORAGE_QUOTA_BYTES_LOCAL_FILE,
            "file:// origin must get LOCAL_FILE quota (not localhost quota)"
        );
    }

    // ------------------------------------------------------------------
    // Task 1 — execute_capability_action SSRF: file:// → remote mizu://
    // (structural domain check, not substring match)
    // ------------------------------------------------------------------

    #[test]
    fn ssrf_query_injection_does_not_bypass_remote_block() {
        // Pre-regression: `mizu://evil.com/data?host=localhost` contained "localhost"
        // in the raw URL string, so the old `.contains("localhost")` check would have
        // allowed a file:// origin to make a call to a remote server.
        //
        // We verify this by directly testing MizuUri::parse + is_local_host, which is
        // the logic that now backs execute_capability_action.
        let target_url = "mizu://evil.com/data?host=localhost";
        let uri = crate::network::uri::MizuUri::parse(target_url).expect("must parse");
        assert_eq!(
            uri.domain, "evil.com",
            "domain must be 'evil.com', not 'evil.com...'"
        );
        assert!(
            !crate::network::worker::is_local_host(&uri.domain),
            "evil.com is not local — call from file:// must be blocked"
        );
    }

    #[test]
    fn ssrf_real_local_target_is_not_blocked() {
        // Genuine `mizu://localhost/api` from a file:// origin must be allowed.
        let target_url = "mizu://localhost/api";
        let uri = crate::network::uri::MizuUri::parse(target_url).expect("must parse");
        assert_eq!(uri.domain, "localhost");
        assert!(
            crate::network::worker::is_local_host(&uri.domain),
            "localhost target must not be blocked for file:// origins"
        );
    }

    #[test]
    fn ssrf_malformed_url_fails_secure() {
        // A URL that MizuUri cannot parse (e.g. uses a different scheme) should be
        // treated as remote (blocked) rather than allowed — fail-secure.
        let parse_result = crate::network::uri::MizuUri::parse("https://evil.com/data");
        assert!(parse_result.is_err(), "non-mizu:// URL must fail to parse");
        // In execute_capability_action the .unwrap_or(true) makes parse failures block the call.
    }

    // ------------------------------------------------------------------
    // POST/PUT/QUERY payload plumbing: ResolvedCall → NetworkCmd::Fetch
    // ------------------------------------------------------------------

    #[test]
    fn resolved_call_payload_reaches_network_cmd() {
        // Regression: the payload declared in the document used to be silently
        // dropped during ResolvedCall dispatch, so POST bodies never reached
        // the wire.  It must now be forwarded intact into NetworkCmd::Fetch.
        let (network_tx, mut network_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let (logic_tx, _logic_rx) = std::sync::mpsc::channel();
        let mut store = crate::core::types::VariableStore::new();
        let target_variable = store.interner.get_or_intern("result");
        let mut store = store.freeze();
        let mut policy =
            capability_policy_for("mizu://example.com/index.mizu", &StorageUsageLedger::new());

        let payload = Value::String(Arc::from(r#"{"who":"mizu"}"#));
        super::execute_capability_action(
            &mut store,
            &network_tx,
            &logic_tx,
            crate::network::TabId(0),
            "mizu://example.com/index.mizu",
            &mut policy,
            crate::network::RuntimeAction::ResolvedCall {
                method: "POST".to_string(),
                url: "mizu://example.com/api/v1/submit".to_string(),
                payload: Some(payload.clone()),
                target_variable,
                format: crate::parser::logic::PayloadFormat::Json,
                headers: vec![],
            },
        );

        match network_rx.try_recv() {
            Ok(crate::network::NetworkCmd::Fetch {
                method,
                payload: sent,
                target_var,
                ..
            }) => {
                assert_eq!(method, "POST");
                assert!(
                    sent.as_ref().is_some_and(|v| v
                        .budget_eq(&payload, &mut u64::MAX, u64::MAX)
                        .unwrap_or(false)),
                    "POST payload must survive the ResolvedCall → Fetch dispatch"
                );
                assert_eq!(
                    target_var, "result",
                    "target_var must be the resolved variable name, not a stringified Symbol id"
                );
            }
            other => panic!("expected NetworkCmd::Fetch, got {other:?}"),
        }
    }

    #[test]
    fn resolved_call_format_reaches_network_cmd() {
        // Mirrors `resolved_call_payload_reaches_network_cmd`: `format` must
        // survive the same ResolvedCall → NetworkCmd::Fetch dispatch intact,
        // for every non-default format value, not just the JSON default.
        for format in [
            crate::parser::logic::PayloadFormat::Form,
            crate::parser::logic::PayloadFormat::Text,
            crate::parser::logic::PayloadFormat::Yaml,
        ] {
            let (network_tx, mut network_rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
            let (logic_tx, _logic_rx) = std::sync::mpsc::channel();
            let mut store = crate::core::types::VariableStore::new();
            let target_variable = store.interner.get_or_intern("result");
            let mut store = store.freeze();
            let mut policy =
                capability_policy_for("mizu://example.com/index.mizu", &StorageUsageLedger::new());

            super::execute_capability_action(
                &mut store,
                &network_tx,
                &logic_tx,
                crate::network::TabId(0),
                "mizu://example.com/index.mizu",
                &mut policy,
                crate::network::RuntimeAction::ResolvedCall {
                    method: "POST".to_string(),
                    url: "mizu://example.com/api/v1/submit".to_string(),
                    payload: Some(Value::from("x".to_string())),
                    target_variable,
                    format,
                    headers: vec![],
                },
            );

            match network_rx.try_recv() {
                Ok(crate::network::NetworkCmd::Fetch {
                    format: sent_format,
                    ..
                }) => {
                    assert_eq!(
                        sent_format, format,
                        "format must survive the ResolvedCall → Fetch dispatch"
                    );
                }
                other => panic!("expected NetworkCmd::Fetch, got {other:?}"),
            }
        }
    }

    /// Regression guard for the `/* FIX SYMBOL */` bug: `target_var` sent to
    /// the network worker must be the variable's actual name, never the
    /// Symbol's raw numeric id stringified (e.g. "3").
    #[test]
    fn resolved_call_target_var_is_resolved_name_not_symbol_id() {
        let (network_tx, mut network_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let (logic_tx, _logic_rx) = std::sync::mpsc::channel();
        let mut store = crate::core::types::VariableStore::new();
        // Intern a few unrelated names first so this symbol's numeric id is
        // guaranteed not to equal its name, ruling out a false pass.
        store.interner.get_or_intern("a");
        store.interner.get_or_intern("b");
        let target_variable = store.interner.get_or_intern("weather_report");
        let mut policy =
            capability_policy_for("mizu://example.com/index.mizu", &StorageUsageLedger::new());

        let mut store = store.freeze();
        super::execute_capability_action(
            &mut store,
            &network_tx,
            &logic_tx,
            crate::network::TabId(0),
            "mizu://example.com/index.mizu",
            &mut policy,
            crate::network::RuntimeAction::ResolvedCall {
                method: "GET".to_string(),
                url: "mizu://example.com/api/weather".to_string(),
                payload: None,
                target_variable,
                format: crate::parser::logic::PayloadFormat::Json,
                headers: vec![],
            },
        );

        match network_rx.try_recv() {
            Ok(crate::network::NetworkCmd::Fetch { target_var, .. }) => {
                assert_eq!(target_var, "weather_report");
                assert_ne!(
                    target_var,
                    target_variable.0.to_string(),
                    "target_var must not be the stringified Symbol id"
                );
            }
            other => panic!("expected NetworkCmd::Fetch, got {other:?}"),
        }
    }

    /// An unresolvable `target_variable` (a Symbol absent from this thread's
    /// interner) must block the dispatch rather than send a meaningless
    /// target name to the network worker.
    #[test]
    fn resolved_call_with_unresolvable_symbol_is_blocked() {
        let (network_tx, mut network_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
        let (logic_tx, _logic_rx) = std::sync::mpsc::channel();
        let mut store = crate::core::types::VariableStore::new().freeze();
        let mut policy =
            capability_policy_for("mizu://example.com/index.mizu", &StorageUsageLedger::new());

        let outcome = super::execute_capability_action(
            &mut store,
            &network_tx,
            &logic_tx,
            crate::network::TabId(0),
            "mizu://example.com/index.mizu",
            &mut policy,
            crate::network::RuntimeAction::ResolvedCall {
                method: "GET".to_string(),
                url: "mizu://example.com/api/x".to_string(),
                payload: None,
                target_variable: crate::core::types::Symbol(0), // never interned
                format: crate::parser::logic::PayloadFormat::Json,
                headers: vec![],
            },
        );

        assert!(matches!(outcome, super::CapabilityOutcome::Blocked(_)));
        assert!(
            network_rx.try_recv().is_err(),
            "no Fetch command must be sent for an unresolvable target_variable"
        );
    }
}
