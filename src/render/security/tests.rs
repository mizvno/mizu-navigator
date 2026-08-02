//! Tests for the security module.

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
    let remote = capability_policy_for("mizu://example.com/index.mizu", &StorageUsageLedger::new());
    let local = capability_policy_for("mizu://localhost/index.mizu", &StorageUsageLedger::new());
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
    let normalized = super::normalize_path_components(Path::new("home/user/app/../../etc/passwd"));
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
    let policy =
        super::capability_policy_for("mizu://evil.com?host=localhost", &StorageUsageLedger::new());
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
    let policy = super::capability_policy_for("mizu://localhost/app", &StorageUsageLedger::new());
    assert_eq!(
        policy.quota_bytes,
        super::STORAGE_QUOTA_BYTES_LOCALHOST,
        "genuine localhost origin must receive localhost quota"
    );
}

#[test]
fn capability_policy_ip_127_gets_localhost_quota() {
    let policy = super::capability_policy_for("mizu://127.0.0.1/app", &StorageUsageLedger::new());
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
