//! Tests for the config module.

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
    assert_eq!(cfg.max_redirects, MizuConfig::default().max_redirects);
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
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("config.toml")
    );
    assert_eq!(
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str()),
        Some("mizu")
    );
}
