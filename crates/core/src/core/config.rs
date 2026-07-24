//! Runtime configuration.
//!
//! Two distinct mechanisms, deliberately kept separate:
//!
//! * **Operational settings** ([`MizuConfig`]) â€” network timeouts, pool
//!   sizes, storage debounce, redirect budget, the QUIC port. These are
//!   loaded once from an optional TOML file (see [`config_path`]) and are
//!   safe to expose: none of them weaken a security invariant, they only
//!   tune how the client behaves on a given network/machine.
//! * **Experimental budget overrides** ([`env_override`]) â€” a handful of
//!   constants whose own doc comments admit they are unmeasured starting
//!   guesses, not derived limits (`MAX_COMP_BINDINGS`, `MAX_INSTRUCTIONS`,
//!   `MAX_SYNTHETIC_LAYOUT_NODES`, `INPUT_MAX_BYTES`, `MAX_PARSE_DEPTH`,
//!   `MAX_TOKEN_TTL_SECS`). These are overridable only via `MIZU_*`
//!   environment variables for a single run, deliberately *not* part of
//!   `config.toml`, so they never look like a supported, persisted setting.
//!
//! **What is never configurable, by either mechanism:** `MAX_EVAL_DEPTH`
//! (paired with `LogicWorker::STACK_SIZE_BYTES` by a measurement in RM-14 â€”
//! changing one without the other breaks the proof that the depth guard
//! fires before a native stack overflow), the storage quotas, response-body
//! and image decode limits, `MAX_JSON_DEPTH` (tied to `MAX_EVAL_DEPTH` by
//! design), and `DECIMAL_SCALE` (not a limit â€” the fixed-point numeric
//! format; changing it would silently corrupt already-stored data). See
//! `SECURITY-INVARIANTS.md`.
//!
//! ## `config.toml` â€” every field, with its default
//!
//! Missing entirely, or missing individual fields, is fine â€” anything not
//! set keeps the value shown here.
//!
//! ```toml
//! connect_timeout_secs = 10
//! request_timeout_secs = 30
//! quic_max_idle_timeout_secs = 60
//! quic_keep_alive_interval_secs = 15
//! max_pool_size = 32
//! max_ui_channel_capacity = 32
//! max_concurrent_fetches = 16
//! storage_debounce_window_ms = 150
//! storage_batch_max_keys = 64
//! max_redirects = 10
//! mizu_port = 7399
//! ```
//!
//! ## Experimental overrides â€” environment variables, not `config.toml`
//!
//! `MIZU_MAX_COMP_BINDINGS`, `MIZU_MAX_INSTRUCTIONS`,
//! `MIZU_MAX_SYNTHETIC_LAYOUT_NODES`, `MIZU_INPUT_MAX_BYTES`,
//! `MIZU_MAX_PARSE_DEPTH`, `MIZU_MAX_TOKEN_TTL_SECS` â€” e.g.
//! `MIZU_MAX_COMP_BINDINGS=2000 cargo run -- ./big.mizu`. An unset or
//! unparseable value falls back to the default silently logged via
//! `tracing::warn!`.

use std::path::PathBuf;
use std::sync::LazyLock;

use serde::Deserialize;

/// Operational settings, loadable from `config.toml`. Every field has a
/// default matching this project's original hardcoded constant, so a
/// missing file â€” or a file that only sets some fields â€” changes nothing
/// else.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MizuConfig {
    pub connect_timeout_secs: u64,
    pub request_timeout_secs: u64,
    pub quic_max_idle_timeout_secs: u64,
    pub quic_keep_alive_interval_secs: u64,
    pub max_pool_size: usize,
    pub max_ui_channel_capacity: usize,
    pub max_concurrent_fetches: usize,
    pub storage_debounce_window_ms: u64,
    pub storage_batch_max_keys: usize,
    pub max_redirects: u32,
    pub mizu_port: u16,
}

impl Default for MizuConfig {
    fn default() -> Self {
        Self {
            connect_timeout_secs: 10,
            request_timeout_secs: 30,
            quic_max_idle_timeout_secs: 60,
            quic_keep_alive_interval_secs: 15,
            max_pool_size: 32,
            max_ui_channel_capacity: 32,
            max_concurrent_fetches: 16,
            storage_debounce_window_ms: 150,
            storage_batch_max_keys: 64,
            max_redirects: 10,
            mizu_port: 7399,
        }
    }
}

/// Returns the path to the optional user config file:
/// `%APPDATA%\mizu\config.toml` on Windows, `$XDG_CONFIG_HOME/mizu/config.toml`
/// (falling back to `$HOME/.config/mizu/config.toml`) on Unix. Mirrors
/// [`crate::core::storage::mizu_storage_path`]'s base-directory resolution.
fn config_path() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./mizu_config"));

    #[cfg(unix)]
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|home| PathBuf::from(home).join(".config"))
                .unwrap_or_else(|_| PathBuf::from("./mizu_config"))
        });

    #[cfg(not(any(windows, unix)))]
    let base = PathBuf::from("./mizu_config");

    base.join("mizu").join("config.toml")
}

/// Loads [`MizuConfig`] from [`config_path`]. A missing file is the normal
/// case (no config authored yet) and silently yields defaults; a file that
/// exists but fails to parse falls back to defaults too, but logs a warning
/// so a typo doesn't silently do nothing.
fn load() -> MizuConfig {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return MizuConfig::default(),
    };
    match toml::from_str(&text) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to parse config.toml; using default settings"
            );
            MizuConfig::default()
        }
    }
}

/// Process-wide operational settings, loaded once on first access.
pub static CONFIG: LazyLock<MizuConfig> = LazyLock::new(load);

/// Reads a `MIZU_*` environment-variable override for one of the
/// experimental budget constants described in the module doc comment above.
/// Falls back to `default` when the variable is unset or fails to parse
/// (logging a warning in the latter case, so a typo doesn't silently pick
/// the wrong value).
pub fn env_override<T>(var_name: &str, default: T) -> T
where
    T: std::str::FromStr + Copy,
{
    resolve_override(var_name, std::env::var(var_name).ok(), default)
}

/// The pure decision behind [`env_override`], factored out so it's testable
/// without mutating the real process environment (which would need
/// `unsafe { std::env::set_var(..) }` â€” forbidden crate-wide).
fn resolve_override<T>(var: &str, raw: Option<String>, default: T) -> T
where
    T: std::str::FromStr + Copy,
{
    match raw {
        Some(val) => val.parse().unwrap_or_else(|_| {
            tracing::warn!(var, val, "invalid value for env override; using default");
            default
        }),
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_documented_defaults() {
        let cfg = MizuConfig::default();
        assert_eq!(cfg.connect_timeout_secs, 10);
        assert_eq!(cfg.request_timeout_secs, 30);
        assert_eq!(cfg.quic_max_idle_timeout_secs, 60);
        assert_eq!(cfg.quic_keep_alive_interval_secs, 15);
        assert_eq!(cfg.max_pool_size, 32);
        assert_eq!(cfg.max_ui_channel_capacity, 32);
        assert_eq!(cfg.max_concurrent_fetches, 16);
        assert_eq!(cfg.storage_debounce_window_ms, 150);
        assert_eq!(cfg.storage_batch_max_keys, 64);
        assert_eq!(cfg.max_redirects, 10);
        assert_eq!(cfg.mizu_port, 7399);
    }

    #[test]
    fn empty_toml_yields_all_defaults() {
        let cfg: MizuConfig = toml::from_str("").expect("empty document must parse");
        assert_eq!(cfg.max_pool_size, MizuConfig::default().max_pool_size);
        assert_eq!(cfg.mizu_port, MizuConfig::default().mizu_port);
    }

    #[test]
    fn partial_toml_overrides_only_the_fields_it_sets() {
        let cfg: MizuConfig = toml::from_str("max_pool_size = 8\nmizu_port = 9999\n")
            .expect("partial document must parse");
        assert_eq!(cfg.max_pool_size, 8);
        assert_eq!(cfg.mizu_port, 9999);
        // Everything else keeps the default.
        assert_eq!(
            cfg.connect_timeout_secs,
            MizuConfig::default().connect_timeout_secs
        );
        assert_eq!(
            cfg.max_redirects,
            MizuConfig::default().max_redirects
        );
    }

    #[test]
    fn malformed_toml_is_rejected_by_the_parser() {
        // load() catches this and falls back to defaults; here we just pin
        // down that toml::from_str itself does reject garbage, which is the
        // precondition load()'s fallback path relies on.
        let result: Result<MizuConfig, _> = toml::from_str("not = [valid TOML");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_override_uses_default_when_unset() {
        assert_eq!(resolve_override::<u32>("MIZU_TEST_VAR", None, 42), 42);
    }

    #[test]
    fn resolve_override_parses_a_valid_value() {
        assert_eq!(
            resolve_override::<u32>("MIZU_TEST_VAR", Some("99".to_string()), 42),
            99
        );
    }

    #[test]
    fn resolve_override_falls_back_on_unparseable_value() {
        assert_eq!(
            resolve_override::<u32>("MIZU_TEST_VAR", Some("not-a-number".to_string()), 42),
            42
        );
    }

    #[test]
    fn config_path_lands_under_a_mizu_directory_with_the_right_filename() {
        let path = config_path();
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("config.toml"));
        assert_eq!(
            path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()),
            Some("mizu")
        );
    }
}
