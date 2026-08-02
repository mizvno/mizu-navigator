//! Tests for `storage_debounce.rs`: write coalescing (RM-12) — closely
//! spaced writes to one domain land in a single `redb` transaction,
//! `max_keys` forces an immediate flush, same-key writes collapse to
//! last-write-wins, and different domains never share a batch. Also pins
//! the still-immediate, non-debounced `StoragePool::write_record` primitive
//! the debouncer is built on top of.

use super::*;

/// Verifies `StoragePool::write_record`'s own immediate-write guarantee:
/// no write-behind cache sits in front of it, so the value is visible to
/// a subsequent read with no artificial delay (no sleep between write
/// and read). RM-12: the production `NetworkCmd::StorageStore` dispatch
/// now goes through `StorageWriteDebouncer` instead of calling this
/// directly (see `storage_debounce_*` tests below) — `write_record`
/// itself is unchanged and remains available as the non-debounced,
/// immediate-write primitive.
#[test]
fn test_storage_store_writes_directly_with_no_delay() {
    let tmp_dir = std::env::temp_dir().join("mizu_test_worker_direct_write");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let path = tmp_dir.join("direct.enc");

    let db = redb::Database::create(&path).unwrap();
    {
        let write_txn = db.begin_write().unwrap();
        {
            let _ = write_txn
                .open_table(crate::core::storage::STORAGE_TABLE)
                .unwrap();
        }
        write_txn.commit().unwrap();
    }
    let engine = std::sync::Arc::new(crate::core::storage::StorageEngine::from_parts(
        db,
        [0x33u8; 32],
    ));

    let pool = crate::core::storage::StoragePool::new();
    let domain = crate::core::storage::ValidatedDomain::from_raw("direct-write-test.local");
    pool.insert_for_test(&domain, engine.clone());

    pool.write_record(
        &domain,
        "session_token",
        &crate::core::types::Value::from("abc123"),
    )
    .expect("write_record must succeed");

    let data = engine.read_all().expect("read_all");
    assert!(
        data.get("session_token").is_some_and(|v| v
            .budget_eq(
                &crate::core::types::Value::from("abc123"),
                &mut u64::MAX,
                u64::MAX
            )
            .unwrap_or(false)),
        "value must be readable immediately after write_record returns, with no debounce delay"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// RM-12 (a): several `StorageStore`-equivalent `submit` calls for the
/// same domain, issued back-to-back with no delay between them, must not
/// each open their own `redb` transaction — they must be coalesced into
/// one `write_batch` call once the debounce window elapses.
#[tokio::test]
async fn storage_debounce_batches_closely_spaced_writes_into_one_transaction() {
    let (engine, tmp_dir) = make_debounce_test_engine("batch");
    let pool = crate::core::storage::StoragePool::new();
    let domain = crate::core::storage::ValidatedDomain::from_raw("debounce-batch-test.local");
    pool.insert_for_test(&domain, engine.clone());

    let window = Duration::from_millis(60);
    let debouncer = StorageWriteDebouncer::with_params(window, 64);

    for i in 0..5 {
        debouncer.submit(
            pool.clone(),
            crate::core::storage::ValidatedDomain::from_raw("debounce-batch-test.local"),
            format!("key_{i}"),
            crate::core::types::Value::Decimal(i),
        );
    }

    // Still within the debounce window: nothing should have been
    // committed to redb yet.
    assert_eq!(
        engine.write_batch_call_count(),
        0,
        "writes must not be flushed before the debounce window elapses"
    );

    tokio::time::sleep(window + Duration::from_millis(1000)).await;

    assert_eq!(
        engine.write_batch_call_count(),
        1,
        "5 closely-spaced writes to the same domain must land in exactly 1 redb transaction, not 5"
    );

    let data = engine.read_all().expect("read_all");
    for i in 0..5 {
        assert!(
            data.get(&format!("key_{i}")).is_some_and(|v| v
                .budget_eq(
                    &crate::core::types::Value::Decimal(i),
                    &mut u64::MAX,
                    u64::MAX
                )
                .unwrap_or(false)),
            "key_{i} must be persisted and readable after the batch flush"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// RM-12 (a)/(b): once `max_keys` distinct keys are buffered for a
/// domain, the batch must flush immediately rather than waiting out the
/// (here, deliberately long) debounce window — bounding worst-case
/// latency and memory under sustained writes.
#[tokio::test]
async fn storage_debounce_max_keys_forces_immediate_flush() {
    let (engine, tmp_dir) = make_debounce_test_engine("maxkeys");
    let pool = crate::core::storage::StoragePool::new();
    let domain = crate::core::storage::ValidatedDomain::from_raw("debounce-maxkeys-test.local");
    pool.insert_for_test(&domain, engine.clone());

    // Window is long enough that this test would time out waiting for it
    // — the flush must instead be triggered by hitting max_keys.
    let debouncer = StorageWriteDebouncer::with_params(Duration::from_secs(30), 3);

    for i in 0..3 {
        debouncer.submit(
            pool.clone(),
            crate::core::storage::ValidatedDomain::from_raw("debounce-maxkeys-test.local"),
            format!("key_{i}"),
            crate::core::types::Value::Decimal(i),
        );
    }

    // Give the spawned spawn_blocking flush task a moment to run — it's
    // triggered synchronously by the 3rd `submit` call, well before the
    // 30s window would ever elapse.
    tokio::time::sleep(Duration::from_millis(1000)).await;

    assert_eq!(
        engine.write_batch_call_count(),
        1,
        "hitting max_keys must force an immediate flush without waiting for the debounce window"
    );
    let data = engine.read_all().expect("read_all");
    assert_eq!(
        data.len(),
        3,
        "all 3 keys must be persisted by the threshold-triggered flush"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// RM-12 (b): repeated writes to the *same* key within one debounce
/// window must collapse to last-write-wins and still land in a single
/// transaction — not one entry per write.
#[tokio::test]
async fn storage_debounce_same_key_last_write_wins() {
    let (engine, tmp_dir) = make_debounce_test_engine("lastwrite");
    let pool = crate::core::storage::StoragePool::new();
    let domain = crate::core::storage::ValidatedDomain::from_raw("debounce-lastwrite-test.local");
    pool.insert_for_test(&domain, engine.clone());

    let window = Duration::from_millis(60);
    let debouncer = StorageWriteDebouncer::with_params(window, 64);

    for v in 1..=3 {
        debouncer.submit(
            pool.clone(),
            crate::core::storage::ValidatedDomain::from_raw("debounce-lastwrite-test.local"),
            "counter".to_string(),
            crate::core::types::Value::Decimal(v),
        );
    }

    tokio::time::sleep(window + Duration::from_millis(1000)).await;

    assert_eq!(engine.write_batch_call_count(), 1);
    let data = engine.read_all().expect("read_all");
    assert!(
        data.get("counter").is_some_and(|v| v
            .budget_eq(
                &crate::core::types::Value::Decimal(3),
                &mut u64::MAX,
                u64::MAX
            )
            .unwrap_or(false)),
        "last write within the window must win"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// RM-12: writes to two different domains must not be merged into one
/// transaction — each domain gets its own independent batch/flush.
#[tokio::test]
async fn storage_debounce_batches_per_domain_independently() {
    let (engine_a, tmp_a) = make_debounce_test_engine("domain_a");
    let (engine_b, tmp_b) = make_debounce_test_engine("domain_b");
    let pool = crate::core::storage::StoragePool::new();
    let domain_a = crate::core::storage::ValidatedDomain::from_raw("debounce-domain-a.local");
    let domain_b = crate::core::storage::ValidatedDomain::from_raw("debounce-domain-b.local");
    pool.insert_for_test(&domain_a, engine_a.clone());
    pool.insert_for_test(&domain_b, engine_b.clone());

    let window = Duration::from_millis(60);
    let debouncer = StorageWriteDebouncer::with_params(window, 64);

    debouncer.submit(
        pool.clone(),
        crate::core::storage::ValidatedDomain::from_raw("debounce-domain-a.local"),
        "a_key".to_string(),
        crate::core::types::Value::from("a_value"),
    );
    debouncer.submit(
        pool.clone(),
        crate::core::storage::ValidatedDomain::from_raw("debounce-domain-b.local"),
        "b_key".to_string(),
        crate::core::types::Value::from("b_value"),
    );

    tokio::time::sleep(window + Duration::from_millis(1000)).await;

    assert_eq!(engine_a.write_batch_call_count(), 1);
    assert_eq!(engine_b.write_batch_call_count(), 1);
    assert!(engine_a.read_all().unwrap().get("a_key").is_some_and(|v| {
        v.budget_eq(
            &crate::core::types::Value::from("a_value"),
            &mut u64::MAX,
            u64::MAX,
        )
        .unwrap_or(false)
    }));
    assert!(engine_b.read_all().unwrap().get("b_key").is_some_and(|v| {
        v.budget_eq(
            &crate::core::types::Value::from("b_value"),
            &mut u64::MAX,
            u64::MAX,
        )
        .unwrap_or(false)
    }));

    let _ = std::fs::remove_dir_all(&tmp_a);
    let _ = std::fs::remove_dir_all(&tmp_b);
}
