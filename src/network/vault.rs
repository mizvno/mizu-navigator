use crate::core::errors::MizuError;
use crate::core::storage::ValidatedDomain;
use keyring::Entry;
use serde::{Deserialize, Serialize};

/// Returns 0 (Unix epoch) as the default expiry for vault entries loaded from
/// keyring records that predate the `exp` field.  Expiry 0 causes
/// [`VaultEntry::is_expired`] to return `true` immediately, forcing
/// re-authentication rather than permitting indefinite token reuse.
fn default_exp_secs() -> u64 {
    0
}

/// Represents a Vault entry containing an access token and allowed HTTP methods.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct VaultEntry {
    /// The bearer token string.
    pub token: String,
    /// HTTP methods authorized for this token.
    ///
    /// Acts as a **ceiling**: server-declared scope is intersected with the
    /// runtime's permitted-methods list at import time, preventing scope
    /// inflation by a compromised server.
    pub allowed_methods: Vec<String>,
    /// Token expiry as a UNIX timestamp (seconds since epoch).
    ///
    /// Defaults to `0` for legacy keyring records that lack this field,
    /// causing [`is_expired`] to return `true` immediately (fail-secure).
    #[serde(default = "default_exp_secs")]
    pub exp: u64,
}

impl VaultEntry {
    /// Returns `true` if this token's `exp` timestamp is in the past.
    ///
    /// Fails closed: if the system clock cannot be read the token is treated as
    /// expired rather than accepted.
    pub fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX); // clock failure → treat as expired (fail-secure)
        self.exp <= now
    }

    /// Serializes and saves this `VaultEntry` into the OS keychain keyed by
    /// the SHA-256 hex digest inside `domain`.
    ///
    /// Accepting [`ValidatedDomain`] instead of a raw `&str` guarantees that
    /// the keyring user field is always the canonical, collision-free digest —
    /// never a raw, un-normalised hostname that could collide with a different
    /// casing of the same logical domain.
    pub fn save(domain: &ValidatedDomain, entry: &VaultEntry) -> Result<(), MizuError> {
        let serialized = serde_json::to_string(entry)
            .map_err(|e| MizuError::Network(format!("Serialization error: {}", e)))?;
        let keyring_entry = Entry::new("mizu_vault", domain.as_str())
            .map_err(|e| MizuError::Network(format!("Keyring error: {}", e)))?;
        keyring_entry
            .set_password(&serialized)
            .map_err(|e| MizuError::Network(format!("Failed to save token to keyring: {}", e)))?;
        Ok(())
    }

    /// Loads the `VaultEntry` from the OS keychain for the specified `domain`.
    ///
    /// If the stored JSON predates the `exp` field, serde fills `exp` with `0`
    /// via `#[serde(default)]`, which [`is_expired`] treats as immediately expired.
    pub fn load(domain: &ValidatedDomain) -> Result<Option<Self>, MizuError> {
        let keyring_entry = Entry::new("mizu_vault", domain.as_str())
            .map_err(|e| MizuError::Network(format!("Keyring error: {}", e)))?;
        match keyring_entry.get_password() {
            Ok(serialized) => {
                let entry: VaultEntry = serde_json::from_str(&serialized)
                    .map_err(|e| MizuError::Network(format!("Deserialization error: {}", e)))?;
                Ok(Some(entry))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(MizuError::Network(format!(
                "Failed to load token from keyring: {}",
                e
            ))),
        }
    }

    /// Removes the vault entry for `domain` from the OS keychain.
    ///
    /// Idempotent: returns `Ok(())` if no entry exists.
    pub fn delete(domain: &ValidatedDomain) -> Result<(), MizuError> {
        let keyring_entry = Entry::new("mizu_vault", domain.as_str())
            .map_err(|e| MizuError::Network(format!("Keyring error: {}", e)))?;
        match keyring_entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(MizuError::Network(format!("Failed to revoke token: {}", e))),
        }
    }

    /// Verifies that `method` is within the allowed scope for this token.
    pub fn check_scope(&self, method: &str) -> Result<(), MizuError> {
        let method_upper = method.to_uppercase();
        if self
            .allowed_methods
            .iter()
            .any(|m| m.to_uppercase() == method_upper)
        {
            Ok(())
        } else {
            Err(MizuError::SecurityViolation(format!(
                "MethodScopeViolation: {} is not allowed by the vault token",
                method_upper
            )))
        }
    }
}

#[cfg(test)]
mod tests {
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
}
