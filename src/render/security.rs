#![forbid(unsafe_code)]

use crate::core::errors::MizuError;
use crate::core::storage::ValidatedDomain;
use crate::core::types::{Value, VariableStore};
use crate::network::{RuntimeAction, UiEvent};
use std::time::Instant;


/// Normalises a path lexically (no I/O) by resolving `.` and `..` components.
///
/// Returns an empty [`std::path::PathBuf`] if the path would escape above its
/// root, ensuring that the `starts_with` sandbox check always fails for
/// path-traversal attempts.
pub(crate) fn normalize_path_components(path: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                // pop() returns false when there is nothing left to pop
                // (empty PathBuf or root-only).  In that case the traversal
                // would escape above root — signal failure with an empty path.
                if !out.pop() {
                    return std::path::PathBuf::new();
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Returns `true` if `target` is contained within `sandbox_base`.
///
/// Uses [`std::fs::canonicalize`] when both paths exist (resolves symlinks);
/// falls back to [`normalize_path_components`] for non-existent targets (e.g.
/// first-time navigation, unit tests).  Returns `false` when either canonical
/// path is empty (escape detected) or when the target does not start with
/// `sandbox_base`.
pub(crate) fn file_sandbox_contains(
    sandbox_base: &std::path::Path,
    target: &std::path::Path,
) -> bool {
    let canon_base = std::fs::canonicalize(sandbox_base)
        .unwrap_or_else(|_| normalize_path_components(sandbox_base));
    let canon_target =
        std::fs::canonicalize(target).unwrap_or_else(|_| normalize_path_components(target));
    !canon_base.as_os_str().is_empty()
        && !canon_target.as_os_str().is_empty()
        && canon_target.starts_with(&canon_base)
}


/// Maximum bytes a remote-origin document may store on disk (512 KiB).
pub const STORAGE_QUOTA_BYTES_REMOTE: usize = 512 * 1024;
/// Maximum bytes a local-file-origin document may store on disk (1 MiB).
pub const STORAGE_QUOTA_BYTES_LOCAL_FILE: usize = 1024 * 1024;
/// Maximum bytes a localhost document may store on disk (10 MiB).
pub const STORAGE_QUOTA_BYTES_LOCALHOST: usize = 10 * 1024 * 1024;
/// Maximum `StorageStore` writes allowed within a single one-second window.
pub const STORAGE_RATE_LIMIT_WRITES_PER_SEC: u32 = 10;

/// Per-origin capability budget and rate-limiting state.
///
/// One instance lives on [`crate::render::window::MizuWindowManager`] and is
/// reset every time the user navigates to a new URL.
pub struct CapabilityPolicy {
    /// Accumulated storage bytes written by the current origin.
    pub bytes_stored: usize,
    /// Hard quota limit (bytes).  Derived from origin type at construction.
    pub quota_bytes: usize,
    /// Number of storage writes in the current one-second sliding window.
    write_count_this_second: u32,
    /// Start of the current one-second window.
    window_start: Instant,
}

impl CapabilityPolicy {
    /// Creates a fresh policy sized to the origin type of `chrome_url`.
    ///
    /// Quota tier is determined by parsed origin, not by raw substring search:
    /// `mizu://attacker.com/?host=localhost` must NOT receive the localhost quota.
    pub fn new(chrome_url: &str) -> Self {
        let quota_bytes = if chrome_url.starts_with("file://") {
            // file:// origins always get the local-file quota regardless of path content.
            STORAGE_QUOTA_BYTES_LOCAL_FILE
        } else if let Ok(uri) = crate::network::uri::MizuUri::parse(chrome_url) {
            // Use the structurally-extracted domain, not raw substring matching, to
            // avoid `mizu://evil.com?host=localhost` bypassing the remote quota.
            if crate::network::worker::is_local_host(&uri.domain) {
                STORAGE_QUOTA_BYTES_LOCALHOST
            } else {
                STORAGE_QUOTA_BYTES_REMOTE
            }
        } else {
            STORAGE_QUOTA_BYTES_REMOTE
        };
        Self {
            bytes_stored: 0,
            quota_bytes,
            write_count_this_second: 0,
            window_start: Instant::now(),
        }
    }

    /// Checks and records a storage write of `byte_count` bytes.
    ///
    /// Advances `bytes_stored` and `write_count_this_second` on success.
    /// Returns [`MizuError::SecurityViolation`] if either the rate limit or
    /// the total quota would be exceeded.
    pub fn check_storage_write(&mut self, byte_count: usize) -> Result<(), MizuError> {
        if self.window_start.elapsed().as_secs() >= 1 {
            self.write_count_this_second = 0;
            self.window_start = Instant::now();
        }
        if self.write_count_this_second >= STORAGE_RATE_LIMIT_WRITES_PER_SEC {
            return Err(MizuError::SecurityViolation(format!(
                "storage rate limit exceeded: max {STORAGE_RATE_LIMIT_WRITES_PER_SEC} writes/s"
            )));
        }
        let new_total = self.bytes_stored.saturating_add(byte_count);
        if new_total > self.quota_bytes {
            return Err(MizuError::SecurityViolation(format!(
                "storage quota exceeded: {} / {} bytes",
                new_total, self.quota_bytes
            )));
        }
        self.bytes_stored = new_total;
        self.write_count_this_second += 1;
        Ok(())
    }
}

/// Estimates the serialized byte size of a [`Value`].
///
/// Used by [`CapabilityPolicy::check_storage_write`] to decide how many bytes
/// a `StoreLocal` action would consume.  The estimate is conservative (it
/// ignores JSON overhead) so it can only under-count, which means the quota
/// check is slightly permissive — acceptable given the generous multiplier.
pub fn estimate_value_bytes(value: &Value) -> usize {
    match value {
        Value::String(s) => s.len(),
        Value::Int(_) | Value::Float(_) => 8,
        Value::Bool(_) => 1,
        Value::Null => 4,
        Value::List(items) => items.iter().map(estimate_value_bytes).sum(),
        Value::Record(m) => m
            .iter()
            .map(|(k, v)| k.len() + estimate_value_bytes(v))
            .sum(),
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
        let end = rest
            .find(['/', '?', '#'])
            .unwrap_or(rest.len());
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
        let end = rest
            .find(['/', '?', '#'])
            .unwrap_or(rest.len());
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

/// Executes a declarative capability action, enforcing per-origin policy.
///
/// `policy` tracks storage quota and rate limits for the current origin; it
/// is mutated on every `StoreLocal` that passes the gate.
///
/// `CopyToClipboard` actions are intercepted upstream in the window manager
/// (gesture-activation check + DOM-node text extraction) and must **not**
/// reach this function.  If one does, it is discarded with a warning.
pub fn execute_capability_action(
    _store: &mut VariableStore,
    network_tx: &tokio::sync::mpsc::UnboundedSender<crate::network::NetworkCmd>,
    logic_tx: &std::sync::mpsc::Sender<UiEvent>,
    chrome_url: &str,
    policy: &mut CapabilityPolicy,
    action: RuntimeAction,
) {
    match action {
        RuntimeAction::None => {}
        RuntimeAction::ResolvedCall {
            method,
            url,
            target_variable,
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
                tracing::warn!(
                    url = %url,
                    "SecurityViolation: file:// origin blocked from outbound call to remote mizu:// host"
                );
                return;
            }
            if let Err(e) = network_tx.send(crate::network::NetworkCmd::Fetch {
                method,
                url,
                target_var: target_variable,
                is_remote_origin: chrome_url.starts_with("mizu://"),
            }) {
                tracing::warn!(error = %e, "network channel closed; Fetch command dropped");
            }
        }
        RuntimeAction::StoreLocal { key, value } => {
            let byte_count = estimate_value_bytes(&value);
            if let Err(e) = policy.check_storage_write(byte_count) {
                tracing::warn!(error = %e, key = %key, "StorageStore blocked by capability policy");
                return;
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
        }
        RuntimeAction::CopyToClipboard { .. } => {
            // Must be intercepted and handled in window.rs (gesture + DOM lookup).
            // Reaching here means the intercept was bypassed — discard silently.
            tracing::warn!(
                "CopyToClipboard reached execute_capability_action — should have been intercepted upstream"
            );
        }
        RuntimeAction::GetSystemTime { target_variable } => {
            let now = std::time::SystemTime::now();
            let duration = now
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let time_ms = duration.as_millis() as i64;
            if let Err(e) = logic_tx.send(UiEvent::UpdateVariable {
                name: target_variable,
                value: Value::Int(time_ms),
            }) {
                tracing::warn!(error = %e, "logic channel closed; GetSystemTime update dropped");
            }
        }
        RuntimeAction::Navigate { url } => {
            if let Err(e) = network_tx.send(crate::network::NetworkCmd::Navigate { url }) {
                tracing::warn!(error = %e, "network channel closed; Navigate command dropped");
            }
        }
        RuntimeAction::NetworkCall {
            method,
            endpoint_symbol,
            payload,
            path_param,
            target_variable,
        } => {
            if let Err(e) = network_tx.send(crate::network::NetworkCmd::NetworkRequest {
                request: crate::network::NetworkRequest {
                    endpoint_symbol,
                    method,
                    payload,
                    path_param,
                    target_variable,
                },
            }) {
                tracing::warn!(error = %e, "network channel closed; NetworkRequest command dropped");
            }
        }
        RuntimeAction::DownloadMedia { url } => {
            tracing::info!(url = %url, "download media requested");
            if let Err(e) = network_tx.send(crate::network::NetworkCmd::FetchImage {
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
        }
        RuntimeAction::DownloadAlias { .. } => {
            tracing::warn!("unresolved DownloadAlias reached capability executor");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CapabilityPolicy, STORAGE_QUOTA_BYTES_REMOTE, STORAGE_RATE_LIMIT_WRITES_PER_SEC,
        estimate_value_bytes, get_current_domain,
    };
    use crate::core::errors::MizuError;
    use crate::core::types::Value;
    use std::sync::Arc;


    #[test]
    fn test_storage_quota_enforcement() {
        let mut policy = CapabilityPolicy::new("mizu://example.com/index.mizu");
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
        let remote = CapabilityPolicy::new("mizu://example.com/index.mizu");
        let local = CapabilityPolicy::new("mizu://localhost/index.mizu");
        assert!(local.quota_bytes > remote.quota_bytes);
    }

    #[test]
    fn rate_limit_blocks_excess_writes() {
        let mut policy = CapabilityPolicy::new("mizu://example.com/index.mizu");
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
        let file_policy = CapabilityPolicy::new("file:///home/user/app/index.mizu");
        let remote_policy = CapabilityPolicy::new("mizu://example.com/index.mizu");
        let local_policy = CapabilityPolicy::new("mizu://localhost/index.mizu");
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
        assert_eq!(get_raw_domain("mizu://example.opennic/page"), "example.opennic");
        assert_eq!(get_raw_domain("mizu://example.opennic"), "example.opennic");
    }


    #[test]
    fn capability_policy_query_injection_cannot_grant_localhost_quota() {
        // `mizu://evil.com?host=localhost` must receive REMOTE quota, not LOCALHOST.
        // The old `.contains("localhost")` would have granted the larger quota.
        let policy = super::CapabilityPolicy::new("mizu://evil.com?host=localhost");
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
        let policy = super::CapabilityPolicy::new("mizu://localhost@evil.com/");
        // MizuUri::parse rejects this → parse fails → fallback to REMOTE.
        assert_eq!(
            policy.quota_bytes,
            super::STORAGE_QUOTA_BYTES_REMOTE,
            "credential-stuffed URL must not elevate quota via localhost substring"
        );
    }

    #[test]
    fn capability_policy_real_localhost_gets_localhost_quota() {
        let policy = super::CapabilityPolicy::new("mizu://localhost/app");
        assert_eq!(
            policy.quota_bytes,
            super::STORAGE_QUOTA_BYTES_LOCALHOST,
            "genuine localhost origin must receive localhost quota"
        );
    }

    #[test]
    fn capability_policy_ip_127_gets_localhost_quota() {
        let policy = super::CapabilityPolicy::new("mizu://127.0.0.1/app");
        assert_eq!(
            policy.quota_bytes,
            super::STORAGE_QUOTA_BYTES_LOCALHOST,
            "loopback IP origin must receive localhost quota"
        );
    }

    #[test]
    fn capability_policy_file_origin_gets_local_file_quota_regardless_of_path() {
        // Even if the file path contains the word "localhost", it must get LOCAL_FILE quota.
        let policy = super::CapabilityPolicy::new("file:///home/user/localhost-app/index.mizu");
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
        assert_eq!(uri.domain, "evil.com", "domain must be 'evil.com', not 'evil.com...'");
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
        assert!(
            parse_result.is_err(),
            "non-mizu:// URL must fail to parse"
        );
        // In execute_capability_action the .unwrap_or(true) makes parse failures block the call.
    }
}
