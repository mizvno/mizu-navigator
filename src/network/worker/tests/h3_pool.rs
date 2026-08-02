//! Tests for `h3_pool.rs`: concurrent-connect safety, the stalled-handshake
//! timeout (RM-05), LRU/idle eviction (`make_room`), and ALPN verification
//! (RM-11).

use super::*;

/// BLOCKER 2 — Verifies that concurrent `get_or_connect` calls to the same
/// domain do not deadlock or produce panic, and that failed connections are
/// not cached in the H3 pool.
///
/// Full connection-reuse verification requires an integration test with a
/// live server.  This unit test focuses on the pool's concurrent safety
/// invariants exercisable without network access:
///   • No deadlock when multiple tasks race on the same domain.
///   • Failed connections are never inserted into the pool.
///   • The pool correctly reports 0 entries after all attempts fail.
///
/// RM-05: this used to wrap `get_or_connect` in a manual
/// `tokio::time::timeout` from the test side, because production had no
/// timeout of its own — the call could otherwise hang indefinitely
/// against a non-responsive target. `get_or_connect` now enforces
/// `CONNECT_TIMEOUT` internally, so the test calls it directly (via a
/// short per-instance override so it stays fast) and that manual
/// workaround is gone — see `stalled_handshake_releases_permit_within_timeout`
/// for a test of the timeout firing itself.
#[tokio::test]
async fn test_h3_connection_pool_concurrent_safety_and_failed_eviction() {
    use std::sync::Arc;

    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let _ = provider.install_default();

    let endpoint = Arc::new(
        Endpoint::client(std::net::SocketAddr::from(([0, 0, 0, 0], 0)))
            .expect("client endpoint must be creatable"),
    );

    // Short override so the test stays fast; still exercises the real
    // production timeout code path, not a test-side wrapper.
    let short_timeout = std::time::Duration::from_millis(500);
    let pool = Arc::new(H3ConnectionPool::new_with_connect_timeout(short_timeout));

    assert_eq!(pool.len().await, 0, "pool must be empty at construction");

    // Use localhost:1 — no server is running, all connects fail (or, for
    // a non-responsive target, time out) at the QUIC handshake stage.
    let addr: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();

    let mut handles = Vec::new();
    for _ in 0..3 {
        let pool = pool.clone();
        let ep = endpoint.clone();
        handles.push(tokio::spawn(async move {
            pool.get_or_connect(&ep, addr, "no-server.mizu.local").await
        }));
    }

    for handle in handles {
        let _ = handle.await.expect("spawned task must not panic");
    }

    assert_eq!(
        pool.len().await,
        0,
        "failed connections must never be inserted into the H3 pool"
    );
}

/// RM-05 — Verifies that a server which accepts the QUIC transport
/// connection (receives and reads every packet the client sends) but
/// never completes the application (H3) handshake causes
/// `get_or_connect` to fail with a timeout error — rather than hanging
/// forever — and that a semaphore permit held across the call, exactly
/// mirroring `spawn_network_thread`'s `MAX_CONCURRENT_FETCHES` discipline
/// (acquire before I/O, release via RAII when the task exits), is
/// released once the call returns.
#[tokio::test]
async fn stalled_handshake_releases_permit_within_timeout() {
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let _ = provider.install_default();

    // A UDP socket that receives (and silently discards) every datagram
    // sent to it — the "server" accepts the transport-level connection
    // attempt (packets arrive, no ICMP port-unreachable) but never sends
    // a single byte back, so the QUIC handshake never completes.
    let blackhole = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("blackhole socket must bind");
    let blackhole_addr = blackhole.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        while blackhole.recv_from(&mut buf).await.is_ok() {
            // Deliberately never reply.
        }
    });

    // A real (not `#[cfg(insecure-dev)]`-gated) client TLS config, same
    // shape production builds, so `connect()` actually attempts the QUIC
    // handshake instead of failing synchronously with "no default client
    // config" — the blackhole never gets far enough for certificate
    // verification to matter, so accepting-anything here is fine.
    let endpoint = test_client_endpoint();
    // Short override so the test stays fast; still exercises the real
    // CONNECT_TIMEOUT code path in get_or_connect, not a mock.
    let pool = H3ConnectionPool::new_with_connect_timeout(Duration::from_millis(300));

    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let sem_clone = semaphore.clone();

    let start = std::time::Instant::now();
    let task = tokio::spawn(async move {
        // Same discipline as spawn_network_thread: acquire before I/O,
        // hold across the call, release via RAII when this task exits.
        let permit = sem_clone.acquire_owned().await.unwrap();
        let _permit = permit;
        pool.get_or_connect(&endpoint, blackhole_addr, "stalled.mizu.local")
            .await
    });

    // The outer bound is generous relative to the pool's 300ms connect
    // timeout — if the production fix regressed, this fires instead of
    // the test hanging forever.
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect(
            "get_or_connect must return well within the test's outer bound \
                 — a stalled handshake must not hang forever",
        )
        .expect("task must not panic");
    let elapsed = start.elapsed();

    match result {
        Err(MizuError::Network(_)) => {}
        Ok(_) => panic!("a stalled handshake must not succeed"),
        Err(other) => panic!("expected a Network (timeout) error, got: {other:?}"),
    }
    // Sanity check that this actually exercised the timeout path (the
    // connect attempt genuinely reached the QUIC handshake and hung
    // there) rather than failing some other, instant way.
    assert!(
        elapsed >= Duration::from_millis(250),
        "expected the 300ms connect_timeout to be what bounded this call, \
             but it returned after only {elapsed:?} — likely failed for a \
             different (non-timeout) reason"
    );

    // The permit was released when the spawned task exited (RAII drop
    // of `_permit`), so a fresh acquire is immediately available.
    assert_eq!(
        semaphore.available_permits(),
        1,
        "the semaphore permit must be released once the stalled connect times out"
    );
}

/// RM-05 — Verifies `H3ConnectionPool::make_room` — the exact function
/// `get_or_connect` calls before inserting a new entry — never lets the
/// pool grow beyond `MAX_POOL_SIZE`, even when connecting to far more
/// distinct domains than the limit allows. Exercised directly on the
/// eviction *decision* logic (generic over the stored value, `()` here)
/// rather than through `get_or_connect`, since constructing `MAX_POOL_SIZE
/// + 1` genuine live H3 connections would require that many real servers;
/// this tests the identical code path production uses.
#[test]
fn pool_never_exceeds_max_size() {
    let mut map: std::collections::HashMap<String, ((), Instant)> =
        std::collections::HashMap::new();
    let now = Instant::now();

    for i in 0..(*MAX_POOL_SIZE + 10) {
        H3ConnectionPool::make_room(&mut map, now, *QUIC_MAX_IDLE_TIMEOUT, *MAX_POOL_SIZE);
        map.insert(format!("domain-{i}.example"), ((), now));
        assert!(
            map.len() <= *MAX_POOL_SIZE,
            "pool must never exceed MAX_POOL_SIZE ({}) while \
                 inserting domain #{i}, got {}",
            *MAX_POOL_SIZE,
            map.len()
        );
    }

    assert_eq!(
        map.len(),
        *MAX_POOL_SIZE,
        "pool must be exactly at capacity after inserting more domains than it allows"
    );
}

/// RM-05 — `make_room` must also reap entries idle longer than
/// `max_idle`, independent of the size cap.
#[test]
fn pool_reaps_idle_entries() {
    let mut map: std::collections::HashMap<String, ((), Instant)> =
        std::collections::HashMap::new();
    let now = Instant::now();
    let long_idle = now - Duration::from_secs(120);

    map.insert("stale.example".to_string(), ((), long_idle));
    map.insert("fresh.example".to_string(), ((), now));

    H3ConnectionPool::make_room(&mut map, now, *QUIC_MAX_IDLE_TIMEOUT, *MAX_POOL_SIZE);

    assert!(
        !map.contains_key("stale.example"),
        "an entry idle longer than max_idle must be reaped"
    );
    assert!(
        map.contains_key("fresh.example"),
        "a recently-used entry must not be reaped"
    );
}

/// The ALPN constant must be exactly `b"mizu/3"`.
#[test]
fn test_mizu_alpn_constant_is_mizu3() {
    assert_eq!(
        MIZU_ALPN, b"mizu/3",
        "MIZU_ALPN must be exactly b\"mizu/3\""
    );
}

/// RM-11 — `verify_negotiated_alpn` must reject a server that completed
/// the QUIC handshake without ever negotiating an ALPN protocol at all
/// (the RFC 7301 gap the doc comment on `H3ConnectionPool` used to claim
/// was closed but wasn't), as well as a server that negotiated some
/// other protocol, and must accept only an exact `mizu/3` match.
#[test]
fn test_verify_negotiated_alpn_rejects_missing_or_wrong_protocol() {
    let no_protocol: Box<dyn std::any::Any> = Box::new(quinn::crypto::rustls::HandshakeData {
        protocol: None,
        server_name: None,
    });
    let result = verify_negotiated_alpn(Some(no_protocol), "no-alpn.mizu.test");
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "a handshake that negotiated no ALPN protocol at all must be rejected: {result:?}"
    );

    let wrong_protocol: Box<dyn std::any::Any> = Box::new(quinn::crypto::rustls::HandshakeData {
        protocol: Some(b"h3".to_vec()),
        server_name: None,
    });
    let result = verify_negotiated_alpn(Some(wrong_protocol), "wrong-alpn.mizu.test");
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "a handshake that negotiated a different ALPN protocol must be rejected: {result:?}"
    );

    let result = verify_negotiated_alpn(None, "no-handshake-data.mizu.test");
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "missing handshake data entirely must be rejected, not treated as trusted: {result:?}"
    );

    let correct_protocol: Box<dyn std::any::Any> = Box::new(quinn::crypto::rustls::HandshakeData {
        protocol: Some(MIZU_ALPN.to_vec()),
        server_name: None,
    });
    assert!(
        verify_negotiated_alpn(Some(correct_protocol), "ok.mizu.test").is_ok(),
        "an exact mizu/3 match must be accepted"
    );
}
