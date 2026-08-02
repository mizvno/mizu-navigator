//! Tests for the vault module.

use super::*;

#[test]
fn test_vault_scope_check() {
    let entry = VaultEntry {
        token: "xyz123".to_string(),
        allowed_methods: vec!["GET".to_string(), "POST".to_string()],
        exp: u64::MAX,
    };

    assert!(entry.check_scope("GET").is_ok());
    assert!(entry.check_scope("post").is_ok()); // case-insensitive check

    let err = entry.check_scope("DELETE").unwrap_err();
    if let MizuError::SecurityViolation(msg) = err {
        assert!(msg.contains("MethodScopeViolation"));
    } else {
        panic!("Expected SecurityViolation error");
    }
}

#[test]
fn test_vault_entry_deserialization_compatibility() {
    // Legacy format: no `exp` field → must default to 0 (epoch = expired).
    let legacy_json = r#"{"token":"old_token","allowed_methods":["GET","POST"]}"#;
    let entry: VaultEntry =
        serde_json::from_str(legacy_json).expect("legacy format must deserialize");
    assert_eq!(entry.exp, 0, "missing exp must default to 0");
    assert!(
        entry.is_expired(),
        "legacy token without exp must be treated as expired"
    );

    // Modern format: future expiry → not expired.
    let future_exp = std::time::SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs() + 3600)
        .unwrap_or(9_999_999_999);
    let modern_json =
        format!(r#"{{"token":"new_token","allowed_methods":["GET"],"exp":{future_exp}}}"#);
    let new_entry: VaultEntry =
        serde_json::from_str(&modern_json).expect("modern format must deserialize");
    assert!(
        !new_entry.is_expired(),
        "token with future exp must not be expired"
    );

    // Explicit exp=0 must also be treated as expired.
    let zero_json = r#"{"token":"zero_token","allowed_methods":["GET"],"exp":0}"#;
    let zero_entry: VaultEntry =
        serde_json::from_str(zero_json).expect("zero-exp format must deserialize");
    assert!(
        zero_entry.is_expired(),
        "token with exp=0 must be treated as expired"
    );
}

#[test]
fn test_token_rotation_and_explicit_revocation() {
    let domain_raw = "rotation-revoke-test.mizu.test";
    let vd = ValidatedDomain::from_raw(domain_raw);

    // delete() on a non-existent entry must never error (idempotent).
    VaultEntry::delete(&vd).expect("delete on non-existent entry must be idempotent");

    let future_exp = std::time::SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs() + 3600)
        .unwrap_or(9_999_999_999);

    let v1 = VaultEntry {
        token: "token_v1".to_string(),
        allowed_methods: vec!["GET".to_string()],
        exp: future_exp,
    };
    let v2 = VaultEntry {
        token: "token_v2".to_string(),
        allowed_methods: vec!["GET".to_string(), "POST".to_string()],
        exp: future_exp,
    };

    // save() must not error.
    VaultEntry::save(&vd, &v1).expect("save v1 must succeed");

    // Check whether the keyring round-trips in this environment.
    let roundtrip = VaultEntry::load(&vd)
        .ok()
        .flatten()
        .map(|e| e.token == "token_v1")
        .unwrap_or(false);

    if roundtrip {
        // Rotate: overwrite with v2.
        VaultEntry::save(&vd, &v2).expect("save v2 must succeed");
        let loaded_v2 = VaultEntry::load(&vd)
            .expect("load v2 must succeed")
            .expect("v2 must be present");
        assert_eq!(loaded_v2.token, "token_v2", "rotation must overwrite v1");

        // Explicit revocation.
        VaultEntry::delete(&vd).expect("delete must succeed");
        let after_delete = VaultEntry::load(&vd).expect("load after delete must not error");
        assert!(after_delete.is_none(), "revoked token must not be in vault");
    }

    // Idempotent: delete must never error, even with no entry present.
    VaultEntry::delete(&vd).expect("second delete must be idempotent");
}
