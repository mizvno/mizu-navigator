//! Key derivation: `hex_decode_key_32`/`parse_master_key_hex` (parsing
//! helpers), `derive_domain_key`/`derive_key_from_env_override`/
//! `derive_or_create_key` (the per-domain master key, from the OS keyring
//! or the `MIZU_MASTER_KEY` break-glass override), and `derive_record_key`
//! (the per-variable HKDF-derived record key).

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{KeyInit, OsRng};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

type HmacSha256 = Hmac<Sha256>;

use crate::core::errors::MizuError;

use super::domain::{KEYRING_SERVICE, ValidatedDomain, fail_if_desync, mizu_storage_path};

/// before it drops instead of being left for the allocator to reuse verbatim.
/// The returned `Zeroizing<[u8; 32]>` likewise scrubs itself when it goes out
/// of scope, however that happens (early `drop`, error return, or normal
/// end-of-scope).
fn hex_decode_key_32(hex_str: &str, ctx: &str) -> Result<Zeroizing<[u8; 32]>, MizuError> {
    let mut bytes = hex::decode(hex_str)
        .map_err(|e| MizuError::ExecutionError(format!("{ctx} decode: {e}")))?;
    let result = if bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Zeroizing::new(arr))
    } else {
        Err(MizuError::ExecutionError(format!(
            "{ctx} must be exactly 32 bytes (64 hex chars)"
        )))
    };
    bytes.zeroize();
    result
}

pub(super) fn parse_master_key_hex(hex: &str) -> Result<Zeroizing<[u8; 32]>, MizuError> {
    hex_decode_key_32(hex, "MIZU_MASTER_KEY")
}

pub(super) fn derive_domain_key(
    master_key: &[u8; 32],
    domain: &ValidatedDomain,
) -> Result<Zeroizing<[u8; 32]>, MizuError> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(master_key)
        .map_err(|e| MizuError::ExecutionError(format!("HMAC init: {e}")))?;
    mac.update(domain.as_str().as_bytes());
    let digest: [u8; 32] = mac.finalize().into_bytes().into();
    Ok(Zeroizing::new(digest))
}

/// The `MIZU_MASTER_KEY` branch of [`derive_or_create_key`], factored out so
/// the warning-on-use behavior is testable without mutating the real process
/// environment (which would need `unsafe { std::env::set_var(..) }` —
/// forbidden crate-wide), mirroring how [`crate::core::config::resolve_override`]
/// is factored out of `env_override` for the same reason.
///
/// Returns `Ok(None)` when `raw` is `None` (the caller falls through to the
/// keyring path); `Ok(Some(key))` when `raw` was present and successfully
/// decoded, after logging the break-glass warning; `Err` when present but
/// malformed.
///
/// `MIZU_MASTER_KEY` is a headless/recovery break-glass mechanism, not a
/// supported production key-management path (see the module doc comment and
/// `SECURITY-INVARIANTS.md`) — every use of it bypasses the OS keyring
/// entirely, so it is logged every time it is taken, the same way a bad
/// `MIZU_*` budget override already logs instead of failing silently.
pub(super) fn derive_key_from_env_override(
    raw: Option<String>,
    domain: &ValidatedDomain,
) -> Result<Option<Zeroizing<[u8; 32]>>, MizuError> {
    let Some(hex) = raw else {
        return Ok(None);
    };
    tracing::warn!(
        domain = domain.as_str(),
        "storage master key sourced from MIZU_MASTER_KEY environment variable, not the OS \
         keyring — this is a headless/recovery break-glass path; the key is exposed to \
         anything that can read this process's environment (e.g. /proc/<pid>/environ, \
         child-process inheritance, crash dumps)"
    );
    // RM-10: `master` (the raw domain-wide master key) is only needed to
    // derive this domain's key below; it scrubs itself (`Zeroizing`) the
    // moment this call returns, rather than lingering in memory for the
    // life of the process the way the *result* of `derive_or_create_key`
    // does inside `StorageEngine`.
    let master = parse_master_key_hex(&hex)?;
    Ok(Some(derive_domain_key(&master, domain)?))
}

pub fn derive_or_create_key(domain: &ValidatedDomain) -> Result<Zeroizing<[u8; 32]>, MizuError> {
    if let Some(key) =
        derive_key_from_env_override(crate::core::config::CONFIG.master_key.clone(), domain)?
    {
        return Ok(key);
    }

    let entry = match keyring::Entry::new(KEYRING_SERVICE, domain.as_str()) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                "keyring unavailable ({}); set MIZU_MASTER_KEY for headless operation",
                e
            );
            return Err(MizuError::ExecutionError(format!("keyring open: {e}")));
        }
    };

    match entry.get_password() {
        Ok(hex) => hex_decode_key_32(&hex, "keyring key"),
        Err(keyring::Error::NoEntry) => {
            fail_if_desync(&mizu_storage_path(domain))?;
            let raw_key = Aes256Gcm::generate_key(OsRng);
            let hex_key = hex::encode(raw_key.as_slice());
            entry
                .set_password(&hex_key)
                .map_err(|e| MizuError::ExecutionError(format!("keyring save: {e}")))?;
            Ok(Zeroizing::new(raw_key.into()))
        }
        Err(e) => {
            tracing::warn!(
                "keyring read failed ({}); set MIZU_MASTER_KEY for headless operation",
                e
            );
            Err(MizuError::ExecutionError(format!("keyring read: {e}")))
        }
    }
}

/// Derives a 32-byte encryption key for a specific record from the domain master key.
/// Uses HKDF-SHA256 with the variable name as the `info` parameter.
///
/// # WARNING: DO NOT TOUCH THE HKDF `info` PARAMETER
///
/// **AES-GCM nonce safety in this module relies *entirely* on the key
/// isolation this function provides, keyed per `variable_name`.**
/// `write_batch`/`encrypt_record_with_rng` draw nonces from a CSPRNG
/// (`ChaCha8Rng`) that is seeded once per batch and reused across every
/// record in it, instead of paying for a fresh `OsRng` round-trip per
/// record. That is only safe because AES-GCM's catastrophic failure mode
/// -- encrypting two different plaintexts under the same (key, nonce) pair
/// -- requires the *same key*, and this function guarantees every record
/// gets a distinct key as long as `variable_name` is passed as the `info`
/// parameter here. If this ever changes -- the `info` argument is dropped,
/// replaced with a constant, truncated in a way that can collide, or
/// derived from anything other than a value that is unique per record --
/// that guarantee breaks, and nonce reuse across records under the *same*
/// key becomes possible. Do not change what goes into `hk.expand(..)`
/// below without re-deriving the entire nonce-safety argument in
/// `encrypt_record_with_rng`'s doc comment and `StorageEngine::write_batch`'s
/// doc comment, not just this one.
///
/// RM-10: the returned key is only ever needed for a single encrypt/decrypt
/// call, so it is wrapped in `Zeroizing` -- both call sites (`encrypt_record`,
/// `decrypt_record`) drop it explicitly right after building the cipher from
/// it, rather than letting it sit on the stack until the end of the function.
pub fn derive_record_key(
    master_key: &[u8; 32],
    variable_name: &str,
) -> Result<Zeroizing<[u8; 32]>, MizuError> {
    let hk = Hkdf::<Sha256>::new(None, master_key);
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(variable_name.as_bytes(), out.as_mut())
        .map_err(|e| MizuError::ExecutionError(format!("HKDF expand: {e}")))?;
    Ok(out)
}
