//! Tests for the storage module.

use std::collections::HashMap;

use zeroize::Zeroizing;

use super::crypto::{decrypt_record, encrypt_record};
use super::domain::{ValidatedDomain, fail_if_desync, mizu_storage_path};
use super::engine::{STORAGE_TABLE, StorageEngine, open_db, read_storage};
use super::keys::{
    derive_domain_key, derive_key_from_env_override, derive_or_create_key, derive_record_key,
    parse_master_key_hex,
};
use super::pool::StoragePool;
use crate::core::errors::MizuError;
use crate::core::types::Value;

#[test]
fn validated_domain_normalises_case_and_whitespace() {
    let a = ValidatedDomain::from_raw("  Example.COM  ");
    let b = ValidatedDomain::from_raw("example.com");
    assert_eq!(a.as_str(), b.as_str());
}

#[test]
fn validated_domain_distinct_inputs_yield_distinct_digests() {
    let a = ValidatedDomain::from_raw("app-a.mizu");
    let b = ValidatedDomain::from_raw("app-b.mizu");
    assert_ne!(a.as_str(), b.as_str());
}

#[test]
fn encrypt_decrypt_round_trip() {
    let key = [0xABu8; 32];
    let pt = b"hello, mizu encrypted storage!";
    let blob = encrypt_record(&key, "my_var", pt).unwrap();
    let recovered = decrypt_record(&key, "my_var", &blob).unwrap();
    assert_eq!(recovered, pt);
}

#[test]
fn hkdf_isolates_variable_keys() {
    let key = [0x11u8; 32];
    let pt = b"secret";
    let blob_a = encrypt_record(&key, "var_a", pt).unwrap();
    // Trying to decrypt var_a's blob using var_b's derived key should fail
    assert!(decrypt_record(&key, "var_b", &blob_a).is_err());
}

/// RM-10 acceptance test: a compile-time proof that every function which
/// produces key material now returns a type that scrubs itself on drop,
/// rather than a runtime memory-inspection test — this module is
/// `#![forbid(unsafe_code)]`, and reading freed stack memory to check for
/// zeroing would itself require unsafe (and be UB besides). `Zeroizing<T>`
/// implements `zeroize::ZeroizeOnDrop`; a plain `[u8; 32]` does not, so
/// `assert_zeroize_on_drop` only compiles here because the return types
/// of `derive_record_key`/`derive_domain_key`/`parse_master_key_hex`
/// genuinely changed from `[u8; 32]` to `Zeroizing<[u8; 32]>`.
#[test]
fn derived_keys_are_self_zeroizing_on_drop() {
    fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>(_: &T) {}

    let master = [0x11u8; 32];
    let domain = ValidatedDomain::from_raw("zeroize-typecheck.local");

    let record_key = derive_record_key(&master, "var").unwrap();
    assert_zeroize_on_drop(&record_key);

    let domain_key = derive_domain_key(&master, &domain).unwrap();
    assert_zeroize_on_drop(&domain_key);

    let hex_master = hex::encode(master);
    let parsed = parse_master_key_hex(&hex_master).unwrap();
    assert_zeroize_on_drop(&parsed);
}

#[test]
fn env_override_absent_falls_through_to_keyring_path() {
    let domain = ValidatedDomain::from_raw("env-override-absent.local");
    let result = derive_key_from_env_override(None, &domain).unwrap();
    assert!(result.is_none());
}

#[test]
fn env_override_present_derives_the_domain_key_without_the_keyring() {
    let domain = ValidatedDomain::from_raw("env-override-present.local");
    let master = [0x22u8; 32];
    let hex_master = hex::encode(master);

    let result = derive_key_from_env_override(Some(hex_master), &domain)
        .unwrap()
        .expect("Some(hex) must yield a derived key, not fall through");

    let expected = derive_domain_key(&master, &domain).unwrap();
    assert_eq!(result.as_ref(), expected.as_ref());
}

#[test]
fn env_override_malformed_hex_is_rejected() {
    let domain = ValidatedDomain::from_raw("env-override-malformed.local");
    let err = derive_key_from_env_override(Some("not hex".to_string()), &domain).unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

/// Hand-rolled `tracing::Subscriber` capturing event messages into a
/// shared buffer, installed only for the duration of one test closure
/// via `tracing::subscriber::with_default` — this crate has no
/// `tracing-subscriber`/`tracing-test` dev-dependency to pull in, and
/// this is small enough not to warrant adding one for a single test.
struct WarnCapture {
    messages: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl tracing::Subscriber for WarnCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct MessageVisitor(String);
        impl tracing::field::Visit for MessageVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        self.messages.lock().unwrap().push(visitor.0);
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

#[test]
fn mizu_master_key_env_path_logs_a_warning() {
    let messages: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let subscriber = WarnCapture {
        messages: messages.clone(),
    };
    let domain = ValidatedDomain::from_raw("warn-capture-test.local");
    let hex_master = hex::encode([0x33u8; 32]);

    tracing::subscriber::with_default(subscriber, || {
        let result = derive_key_from_env_override(Some(hex_master), &domain);
        assert!(result.unwrap().is_some());
    });

    let logged = messages.lock().unwrap();
    assert!(
        logged.iter().any(|m| m.contains("MIZU_MASTER_KEY")),
        "expected a warning mentioning MIZU_MASTER_KEY; got: {logged:?}"
    );
}

#[test]
fn write_then_read_roundtrip() {
    let tmp_dir = std::env::temp_dir().join("mizu_test_redb");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let path = tmp_dir.join("test.com.enc");

    let db = redb::Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let _ = write_txn.open_table(STORAGE_TABLE).unwrap();
    }
    write_txn.commit().unwrap();

    let master_key = [0x42u8; 32];
    let engine = StorageEngine {
        db,
        master_key: Zeroizing::new(master_key),
        write_batch_calls: std::sync::atomic::AtomicUsize::new(0),
    };

    let mut data: HashMap<String, Value> = HashMap::new();
    data.insert("hello".to_string(), Value::from("world"));
    data.insert("answer".to_string(), Value::Decimal(42));

    engine
        .write_batch(data.iter().map(|(k, v)| (k.as_str(), v)))
        .expect("write_batch");

    let read_data = engine.read_all().expect("read_all");

    assert_eq!(read_data.get("hello"), Some(&Value::from("world")));
    assert_eq!(read_data.get("answer"), Some(&Value::Decimal(42)));

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// Regression test for the `write_batch` nonce path:
/// a bug that produced a fixed or repeating nonce sequence would be a real AES-GCM
/// security regression (nonce reuse *under the same key* is
/// catastrophic), and only a test that inspects the raw stored nonces
/// directly — not just round-trip correctness — would catch it. Reads
/// the raw `nonce || ciphertext` blobs straight out of the `redb` table
/// (bypassing `decrypt_record`) and asserts all nonces in the batch are
/// distinct.
#[test]
fn write_batch_produces_distinct_nonces_across_records() {
    let tmp_dir = std::env::temp_dir().join("mizu_test_redb_nonce_uniqueness");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let path = tmp_dir.join("nonces.enc");

    let db = redb::Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let _ = write_txn.open_table(STORAGE_TABLE).unwrap();
    }
    write_txn.commit().unwrap();

    let master_key = [0x55u8; 32];
    let engine = StorageEngine {
        db,
        master_key: Zeroizing::new(master_key),
        write_batch_calls: std::sync::atomic::AtomicUsize::new(0),
    };

    const N: usize = 50;
    let keys: Vec<String> = (0..N).map(|i| format!("var_{i}")).collect();
    let values: Vec<Value> = (0..N).map(|i| Value::Decimal(i as i64)).collect();
    let records: Vec<(&str, &Value)> = keys.iter().map(String::as_str).zip(values.iter()).collect();

    engine.write_batch(records).expect("write_batch");

    let read_txn = engine.db.begin_read().unwrap();
    let table = read_txn.open_table(STORAGE_TABLE).unwrap();
    let mut nonces: Vec<[u8; 12]> = Vec::with_capacity(N);
    for key in &keys {
        let blob = table
            .get(key.as_str())
            .unwrap()
            .expect("record must exist")
            .value()
            .to_vec();
        assert!(
            blob.len() >= 12,
            "stored blob must be at least 12 bytes (nonce)"
        );
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&blob[..12]);
        nonces.push(nonce);
    }

    let unique: std::collections::HashSet<[u8; 12]> = nonces.iter().copied().collect();
    assert_eq!(
        unique.len(),
        N,
        "all {N} nonces in a single write_batch call must be distinct, got {} unique",
        unique.len()
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// RM-07: a single record whose stored JSON exceeds `MAX_JSON_DEPTH`
/// must not abort `read_all` for the whole domain. Before the fix, the
/// `from_json_slice(&plaintext)?` in the `Ok` branch propagated the depth-limit
/// `SecurityViolation` out of `read_all` entirely, so one over-deep
/// record made every other record in the domain unreadable too.
#[test]
fn read_all_skips_over_deep_record_but_returns_rest() {
    let tmp_dir = std::env::temp_dir().join("mizu_test_redb_deep");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let path = tmp_dir.join("deep.enc");

    let db = redb::Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let _ = write_txn.open_table(STORAGE_TABLE).unwrap();
    }
    write_txn.commit().unwrap();

    let master_key = [0x99u8; 32];
    let engine = StorageEngine {
        db,
        master_key: Zeroizing::new(master_key),
        write_batch_calls: std::sync::atomic::AtomicUsize::new(0),
    };

    // Build a Value nested well beyond MAX_JSON_DEPTH. `to_json`/`write_batch`
    // don't depth-check on the way in (only `from_json_slice`, on the way
    // out, does), so this reproduces a record that was legitimately persisted
    // but can no longer be decoded back into a `Value`.
    let mut deep = Value::Decimal(1);
    for _ in 0..300 {
        deep = Value::List(std::sync::Arc::new(vec![deep]));
    }

    let mut data: HashMap<String, Value> = HashMap::new();
    data.insert("too_deep".to_string(), deep);
    data.insert("normal".to_string(), Value::from("still here"));

    engine
        .write_batch(data.iter().map(|(k, v)| (k.as_str(), v)))
        .expect("write_batch");

    let read_data = engine
        .read_all()
        .expect("read_all must not fail for the whole domain");

    assert_eq!(read_data.get("normal"), Some(&Value::from("still here")));
    assert!(
        !read_data.contains_key("too_deep"),
        "over-deep record must be skipped (with a warning), not silently truncated or kept"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn storage_pool_reuses_cached_engine_and_writes_are_immediately_durable() {
    let tmp_dir = std::env::temp_dir().join("mizu_test_storage_pool");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let path = tmp_dir.join("pool.enc");

    let db = redb::Database::create(&path).unwrap();
    {
        let write_txn = db.begin_write().unwrap();
        {
            let _ = write_txn.open_table(STORAGE_TABLE).unwrap();
        }
        write_txn.commit().unwrap();
    }
    let engine = std::sync::Arc::new(StorageEngine::from_parts(db, [0x77u8; 32]));

    let pool = StoragePool::new();
    let domain = ValidatedDomain::from_raw("pool-test.local");
    pool.insert_for_test(&domain, engine.clone());

    // A cached domain must return the exact same Arc, never re-opening
    // the keyring/redb file — this is what makes per-write dispatch cheap.
    let fetched = pool
        .get_or_open(&domain)
        .expect("cached engine must be returned");
    assert!(
        std::sync::Arc::ptr_eq(&fetched, &engine),
        "get_or_open must reuse the cached engine, not open a new one"
    );

    // write_record persists through redb with no artificial delay: no
    // sleep is needed before the value is visible to a subsequent read.
    pool.write_record(&domain, "greeting", &Value::from("hi"))
        .expect("write_record");
    let data = engine.read_all().expect("read_all");
    assert_eq!(data.get("greeting"), Some(&Value::from("hi")));

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// INV-02: two *real, independent OS processes* opening the same redb
/// file for the same domain must be serialized safely — the second
/// opener must be rejected (not hang, not corrupt, not silently
/// succeed), and the lock must be genuinely released (not stuck) once
/// the first process closes its handle.
///
/// This re-execs the test binary itself as a child process, gated by an
/// env var, following the same pattern already established by
/// `core::types::tests::cross_function_composition_depth_guard` /
/// `measure_stack_usage_at_max_eval_depth` for other process-level
/// guarantees in this codebase — a genuine second process, not a mock.
#[test]
fn concurrent_process_open_is_serialized_by_redb_flock() {
    const CHILD_PATH_ENV: &str = "MIZU_STORAGE_LOCK_CHILD_PATH";
    const CHILD_OPENED: &str = "CHILD_OPENED_DB_OK";
    const CHILD_LOCKED_OUT: &str = "CHILD_GOT_DATABASE_ALREADY_OPEN";

    // Child mode: try to open the redb file at the path given via env
    // var, report the outcome on stdout, then exit. Real process exit,
    // real OS file lock — no simulation.
    if let Some(path) = std::env::var_os(CHILD_PATH_ENV) {
        match redb::Database::create(path) {
            Ok(_db) => println!("{CHILD_OPENED}"),
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => println!("{CHILD_LOCKED_OUT}"),
            Err(e) => println!("CHILD_OTHER_ERROR: {e}"),
        }
        return;
    }

    // Parent mode.
    let tmp_dir = std::env::temp_dir().join("mizu_test_multiprocess_lock");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let path = tmp_dir.join("locked.redb");

    let exe = std::env::current_exe().expect("current_exe");
    let spawn_child = |exe: &std::path::Path, path: &std::path::Path| {
        std::process::Command::new(exe)
            .arg("core::storage::tests::concurrent_process_open_is_serialized_by_redb_flock")
            .arg("--exact")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_PATH_ENV, path)
            .output()
            .expect("failed to spawn child test process")
    };

    // Parent opens (and holds) the database first, exactly as one
    // running `mizu-navigator` process would while a document with that
    // domain remains open.
    let db = redb::Database::create(&path).expect("parent opens db");

    // While the parent still holds it, a second, independent process
    // trying to open the exact same file must be rejected immediately —
    // not hang waiting for the lock, not corrupt the file, not silently
    // proceed as if nothing else had it open.
    let child1 = spawn_child(&exe, &path);
    let stdout1 = String::from_utf8_lossy(&child1.stdout);
    assert!(
        stdout1.contains(CHILD_LOCKED_OUT),
        "a second process opening the same redb file while the first \
         still holds it must get DatabaseAlreadyOpen; stdout: {stdout1} \
         stderr: {}",
        String::from_utf8_lossy(&child1.stderr)
    );

    // Release the parent's handle and confirm the lock was genuinely
    // released (not stuck forever) — a subsequent process must now be
    // able to open the file cleanly.
    drop(db);
    let child2 = spawn_child(&exe, &path);
    let stdout2 = String::from_utf8_lossy(&child2.stdout);
    assert!(
        stdout2.contains(CHILD_OPENED),
        "after the holder closes the database, a new process must be \
         able to open it; stdout: {stdout2} stderr: {}",
        String::from_utf8_lossy(&child2.stderr)
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
