    use super::*;
    use super::auth::*;
    use super::fetch::*;
    use super::h3_pool::*;
    use super::storage_debounce::*;

    /// A `rustls` certificate verifier that accepts anything — test-only,
    /// never compiled into production (unlike the `insecure-dev`-gated
    /// `LocalOrWebPkiVerifier`, which still validates non-local hosts).
    /// Used by [`test_client_endpoint`] to build a real client TLS config so
    /// tests can drive an actual QUIC handshake attempt against a local
    /// listener without needing a certificate trusted by WebPKI.
    #[derive(Debug)]
    struct AcceptAnyCertVerifier;

    impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// Builds a client `Endpoint` with a real (test-only) TLS config —
    /// `mizu/3` ALPN, certificate verification skipped — so `connect()`
    /// actually attempts a QUIC handshake instead of failing synchronously
    /// with "no default client config" the way a bare `Endpoint::client(...)`
    /// does. Requires a crypto provider to already be installed (callers
    /// already do this for other reasons, e.g. building the H3 pool).
    fn test_client_endpoint() -> Endpoint {
        let mut endpoint = Endpoint::client(std::net::SocketAddr::from(([0, 0, 0, 0], 0)))
            .expect("client endpoint must be creatable");
        let mut client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertVerifier))
            .with_no_client_auth();
        client_config.alpn_protocols = vec![MIZU_ALPN.to_vec()];
        let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(client_config)
            .expect("test QuicClientConfig must build");
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_config)));
        endpoint
    }

    // — lossy UTF-8 body decoding (parse_body_value)

    #[test]
    fn test_parse_body_value_valid_utf8() {
        let val = parse_body_value(b"hello world");
        assert_eq!(
            val,
            crate::core::types::Value::from("hello world".to_string())
        );
    }

    #[test]
    fn test_parse_body_value_invalid_utf8_replaced_with_replacement_char() {
        // 0xFF is not valid UTF-8 — must be replaced with U+FFFD, not panic.
        let val = parse_body_value(b"hello \xff world");
        match val {
            crate::core::types::Value::String(s) => {
                assert!(
                    s.contains('\u{FFFD}'),
                    "invalid bytes must be replaced with U+FFFD, got: {s:?}"
                );
                assert!(s.contains("hello"), "valid prefix must be preserved");
            }
            other => panic!("expected Value::String, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_body_value_empty_body() {
        let val = parse_body_value(b"");
        assert_eq!(val, crate::core::types::Value::from(String::new()));
    }

    #[test]
    fn test_parse_body_value_all_bytes_no_panic() {
        // Full 0..=255 range — must return Value::String without panicking.
        let body: Vec<u8> = (0u8..=255u8).collect();
        let val = parse_body_value(&body);
        assert!(
            matches!(val, crate::core::types::Value::String(_)),
            "arbitrary byte payloads must yield Value::String"
        );
    }

    // — response body size ceiling (check_response_body_budget)

    #[test]
    fn test_response_body_budget_allows_under_limit() {
        assert!(check_response_body_budget(0, 1024).is_ok());
        assert!(check_response_body_budget(MAX_RESPONSE_BODY_BYTES - 1, 1).is_ok());
    }

    #[test]
    fn test_response_body_budget_rejects_over_limit() {
        let result = check_response_body_budget(MAX_RESPONSE_BODY_BYTES, 1);
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "exceeding the body ceiling must yield SecurityViolation (non-retryable): {result:?}"
        );
    }

    #[test]
    fn test_response_body_budget_no_overflow_panic() {
        // usize::MAX incoming must saturate, not wrap around to a small value.
        let result = check_response_body_budget(1, usize::MAX);
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "saturating add must still reject: {result:?}"
        );
    }

    #[test]
    fn test_parse_body_value_multibyte_utf8_preserved() {
        // Valid multi-byte UTF-8 (e.g. Japanese) must round-trip without replacement.
        let text = "こんにちは世界";
        let val = parse_body_value(text.as_bytes());
        match val {
            crate::core::types::Value::String(s) => {
                assert_eq!(s.as_ref(), text, "valid UTF-8 must be preserved exactly");
                assert!(!s.contains('\u{FFFD}'), "no replacement chars expected");
            }
            other => panic!("expected Value::String, got {other:?}"),
        }
    }

    /// An attacker cannot inject a `Mizu-Auth-Set` token by embedding the
    /// header syntax in the *body* of a 200 response: the parser only reads
    /// the HTTP header map, never the body.
    #[test]
    fn test_prevent_token_injection_in_payload() {
        let mut headers = http::HeaderMap::new();
        // No `Mizu-Auth-Set` header — only the body contains the injection attempt.
        let body = b"Payload data containing Mizu-Auth-Set: hacker_token SCOPE=GET EXP=9999999999";
        let result = parse_http_response(http::StatusCode::OK, &headers, body, "test_domain.local");

        assert!(result.is_ok(), "200 response must succeed: {result:?}");
        // Body-injected token must NOT have reached the vault.
        let td = crate::core::storage::ValidatedDomain::from_raw("test_domain.local");
        if let Ok(Some(entry)) = VaultEntry::load(&td) {
            assert_ne!(
                entry.token, "hacker_token",
                "body-injected token must not be stored in the vault"
            );
        }

        // Also verify that a Mizu-Auth-Set header WITH the hacker token in the
        // header map IS processed, but only when sent as an actual HTTP header.
        let future_exp = std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_secs() + 3600)
            .unwrap_or(9_999_999_999);
        let auth_val = format!("legit_token SCOPE=GET EXP={future_exp}");
        headers.insert(
            http::HeaderName::from_static("mizu-auth-set"),
            http::HeaderValue::from_str(&auth_val).unwrap(),
        );
        let result2 =
            parse_http_response(http::StatusCode::OK, &headers, b"ok", "test_domain2.local");
        assert!(
            result2.is_ok(),
            "valid Mizu-Auth-Set header must not error: {result2:?}"
        );
    }

    #[test]
    fn test_expired_token_is_not_sent() {
        // Expiry detection is pure logic — no keyring needed.
        let past_exp = VaultEntry {
            token: "must_not_be_sent".to_string(),
            allowed_methods: vec!["GET".to_string()],
            exp: 1, // 1970-01-01 — definitively in the past
        };
        assert!(
            past_exp.is_expired(),
            "entry with exp=1 must be detected as expired"
        );

        let future_exp_secs = std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_secs() + 3600)
            .unwrap_or(9_999_999_999);
        let fresh = VaultEntry {
            token: "ok".to_string(),
            allowed_methods: vec!["GET".to_string()],
            exp: future_exp_secs,
        };
        assert!(
            !fresh.is_expired(),
            "entry with future exp must not be expired"
        );

        // If the keyring round-trips in this environment, verify end-to-end eviction.
        let domain_raw = "expired-send-test.mizu.test";
        let vd = crate::core::storage::ValidatedDomain::from_raw(domain_raw);
        VaultEntry::save(&vd, &past_exp).expect("save must not error");
        let roundtrip = VaultEntry::load(&vd)
            .ok()
            .flatten()
            .map(|e| e.token == "must_not_be_sent")
            .unwrap_or(false);

        if roundtrip {
            // load_valid_entry must reject with SecurityViolation and evict the token.
            let result = load_valid_entry(&vd, "GET");
            assert!(
                matches!(result, Err(MizuError::SecurityViolation(_))),
                "expired token must cause SecurityViolation: {result:?}"
            );
            let after = VaultEntry::load(&vd).expect("load after eviction must not error");
            assert!(
                after.is_none(),
                "expired token must be evicted from vault: {after:?}"
            );
        } else {
            VaultEntry::delete(&vd).ok();
        }
    }

    #[test]
    fn test_uri_parsing_for_navigate() {
        let uri = MizuUri::parse("mizu://localhost/index.mizu").unwrap();
        assert_eq!(uri.domain, "localhost");
        assert_eq!(uri.path, "/index.mizu");
    }

    #[tokio::test]
    async fn test_file_scheme_always_rejected_by_h3_fetch() {
        // handle_fetch_raw must never serve file:// — those go through
        // handle_fetch_file (sandbox-enforced).
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let _ = provider.install_default();
        let endpoint = Endpoint::client(std::net::SocketAddr::from(([0, 0, 0, 0], 0))).unwrap();
        let pool = H3ConnectionPool::new();
        let dns = crate::network::opennic::build_opennic_resolver();

        for is_remote_origin in [false, true] {
            let result = handle_fetch_raw(
                &endpoint,
                &pool,
                &dns,
                "GET",
                "file:///etc/passwd",
                is_remote_origin,
                None,
            )
            .await;
            assert!(
                matches!(result, Err(MizuError::SecurityViolation(_))),
                "file:// must be rejected by the H3 fetch path \
                 (is_remote_origin={is_remote_origin}): {result:?}"
            );
        }
    }

    #[test]
    fn test_file_url_path_traversal_blocked_in_fetch_file() {
        // handle_fetch_file must block traversal attempts even when sandbox_base is provided.
        let result = handle_fetch_file(
            "file:///home/user/app/../../etc/passwd",
            Some("home/user/app"),
        );
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "path traversal must be blocked by file_sandbox_contains, got: {result:?}"
        );
    }

    #[test]
    fn test_file_fetch_no_sandbox_base_blocked() {
        // No sandbox_base configured → all file:// access denied.
        let result = handle_fetch_file("file:///home/user/app/image.png", None);
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "file:// with no sandbox_base must be denied: {result:?}"
        );
    }

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
                let _ = write_txn.open_table(crate::core::storage::STORAGE_TABLE).unwrap();
            }
            write_txn.commit().unwrap();
        }
        let engine = std::sync::Arc::new(
            crate::core::storage::StorageEngine::from_parts(db, [0x33u8; 32])
        );

        let pool = crate::core::storage::StoragePool::new();
        let domain = crate::core::storage::ValidatedDomain::from_raw("direct-write-test.local");
        pool.insert_for_test(&domain, engine.clone());

        pool.write_record(&domain, "session_token", &crate::core::types::Value::from("abc123"))
            .expect("write_record must succeed");

        let data = engine.read_all().expect("read_all");
        assert_eq!(
            data.get("session_token"),
            Some(&crate::core::types::Value::from("abc123")),
            "value must be readable immediately after write_record returns, with no debounce delay"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// Builds a fresh temp-file-backed `redb`-based `StorageEngine`
    /// (`write_batch_call_count()` starts at 0) for the `storage_debounce_*`
    /// tests below. Returns the engine (wrapped in `Arc`, matching how
    /// `StoragePool` stores it) and the temp directory, so callers can clean
    /// up when done.
    fn make_debounce_test_engine(
        name: &str,
    ) -> (std::sync::Arc<crate::core::storage::StorageEngine>, std::path::PathBuf) {
        let tmp_dir = std::env::temp_dir().join(format!("mizu_test_storage_debounce_{name}"));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let path = tmp_dir.join("test.enc");
        let db = redb::Database::create(&path).unwrap();
        {
            let write_txn = db.begin_write().unwrap();
            {
                let _ = write_txn.open_table(crate::core::storage::STORAGE_TABLE).unwrap();
            }
            write_txn.commit().unwrap();
        }
        let engine = std::sync::Arc::new(crate::core::storage::StorageEngine::from_parts(
            db,
            [0x55u8; 32],
        ));
        (engine, tmp_dir)
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
                crate::core::types::Value::Int(i),
            );
        }

        // Still within the debounce window: nothing should have been
        // committed to redb yet.
        assert_eq!(
            engine.write_batch_call_count(),
            0,
            "writes must not be flushed before the debounce window elapses"
        );

        tokio::time::sleep(window + Duration::from_millis(100)).await;

        assert_eq!(
            engine.write_batch_call_count(),
            1,
            "5 closely-spaced writes to the same domain must land in exactly 1 redb transaction, not 5"
        );

        let data = engine.read_all().expect("read_all");
        for i in 0..5 {
            assert_eq!(
                data.get(&format!("key_{i}")),
                Some(&crate::core::types::Value::Int(i)),
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
                crate::core::types::Value::Int(i),
            );
        }

        // Give the spawned spawn_blocking flush task a moment to run — it's
        // triggered synchronously by the 3rd `submit` call, well before the
        // 30s window would ever elapse.
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(
            engine.write_batch_call_count(),
            1,
            "hitting max_keys must force an immediate flush without waiting for the debounce window"
        );
        let data = engine.read_all().expect("read_all");
        assert_eq!(data.len(), 3, "all 3 keys must be persisted by the threshold-triggered flush");

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
                crate::core::types::Value::Int(v),
            );
        }

        tokio::time::sleep(window + Duration::from_millis(100)).await;

        assert_eq!(engine.write_batch_call_count(), 1);
        let data = engine.read_all().expect("read_all");
        assert_eq!(
            data.get("counter"),
            Some(&crate::core::types::Value::Int(3)),
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

        tokio::time::sleep(window + Duration::from_millis(100)).await;

        assert_eq!(engine_a.write_batch_call_count(), 1);
        assert_eq!(engine_b.write_batch_call_count(), 1);
        assert_eq!(
            engine_a.read_all().unwrap().get("a_key"),
            Some(&crate::core::types::Value::from("a_value"))
        );
        assert_eq!(
            engine_b.read_all().unwrap().get("b_key"),
            Some(&crate::core::types::Value::from("b_value"))
        );

        let _ = std::fs::remove_dir_all(&tmp_a);
        let _ = std::fs::remove_dir_all(&tmp_b);
    }

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

        for i in 0..(MAX_POOL_SIZE + 10) {
            H3ConnectionPool::make_room(&mut map, now, QUIC_MAX_IDLE_TIMEOUT, MAX_POOL_SIZE);
            map.insert(format!("domain-{i}.example"), ((), now));
            assert!(
                map.len() <= MAX_POOL_SIZE,
                "pool must never exceed MAX_POOL_SIZE ({MAX_POOL_SIZE}) while \
                 inserting domain #{i}, got {}",
                map.len()
            );
        }

        assert_eq!(
            map.len(),
            MAX_POOL_SIZE,
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

        H3ConnectionPool::make_room(&mut map, now, QUIC_MAX_IDLE_TIMEOUT, MAX_POOL_SIZE);

        assert!(
            !map.contains_key("stale.example"),
            "an entry idle longer than max_idle must be reaped"
        );
        assert!(
            map.contains_key("fresh.example"),
            "a recently-used entry must not be reaped"
        );
    }

    /// MAJOR 2 — Verifies that dot-path interpolation correctly falls through to
    /// the global store when the overlay contains the root key but the full
    /// nested path is absent.
    ///
    /// Pre-fix behaviour: `{user.email}` resolves to the literal `{user.email}`
    /// because `handled` was set to `true` as soon as the overlay contained any
    /// `user` key, even though `resolve_dot_path` returned `None`.
    ///
    /// Post-fix behaviour: `handled` is `false` when `resolve_dot_path` returns
    /// `None`, so Phase 2 (global store) is consulted and the correct email is
    /// returned.
    #[test]
    fn test_dot_path_cascade_to_global_store_when_overlay_lacks_leaf() {
        use std::collections::{BTreeMap, HashMap};
        use std::sync::Arc;

        // Global store: user record that has both `name` and `email`.
        let mut store = crate::core::types::VariableStore::new();
        let mut global_user = Vec::<(std::sync::Arc<str>, crate::core::types::Value)>::new();
        global_user.push((Arc::from("name"), crate::core::types::Value::from("Alice")));
        global_user.push((Arc::from("email"), crate::core::types::Value::from("alice@example.com")));
        store.set(
            "user",
            { global_user.sort_by(|a, b| a.0.cmp(&b.0)); crate::core::types::Value::Record(Arc::from(global_user)) },
        );

        // Overlay: user record that only has `name` — no `email` field.
        let mut overlay_user = Vec::<(std::sync::Arc<str>, crate::core::types::Value)>::new();
        overlay_user.push((Arc::from("name"), crate::core::types::Value::from("Bob")));
        let mut overlay: HashMap<String, crate::core::types::Value> = HashMap::new();
        overlay.insert(
            "user".to_string(),
            { overlay_user.sort_by(|a, b| a.0.cmp(&b.0)); crate::core::types::Value::Record(Arc::from(overlay_user)) },
        );

        // Interpolating `{user.name}` should resolve from the overlay (Bob).
        let name_result = store
            .interpolate_with_overlay("{user.name}", &overlay)
            .expect("interpolation must not error");
        assert_eq!(
            name_result, "Bob",
            "overlay must win for a path it fully resolves ({{user.name}})"
        );

        // Interpolating `{user.email}` must cascade to the global store because
        // the overlay's user record lacks the `email` field.
        //
        // Pre-fix:  returns "{user.email}" (raw placeholder) — handled was true
        //           even though resolve_dot_path returned None.
        // Post-fix: returns "alice@example.com" from the global store.
        let email_result = store
            .interpolate_with_overlay("{user.email}", &overlay)
            .expect("interpolation must not error");
        assert_eq!(
            email_result, "alice@example.com",
            "global store must be consulted when overlay root exists but path is incomplete"
        );

        // Confirm that a path absent from both overlay AND global store still
        // renders the raw placeholder (unchanged from the pre-fix behaviour).
        let missing_result = store
            .interpolate_with_overlay("{user.phone}", &overlay)
            .expect("interpolation must not error");
        assert_eq!(
            missing_result, "{user.phone}",
            "path absent from both overlay and global store must render as raw placeholder"
        );
    }


    /// 200 OK with no auth header must return Ok(None) — success, no redirect.
    #[test]
    fn test_http_200_is_success() {
        let headers = http::HeaderMap::new();
        let result = parse_http_response(http::StatusCode::OK, &headers, b"hello", "x.local");
        assert_eq!(result.unwrap(), None, "200 must yield Ok(None)");
    }

    /// 4xx responses must map to MizuError::Network with the body as message.
    #[test]
    fn test_http_404_is_error() {
        let headers = http::HeaderMap::new();
        let result =
            parse_http_response(http::StatusCode::NOT_FOUND, &headers, b"not found", "x.local");
        assert!(
            matches!(result, Err(MizuError::Network(ref msg)) if msg == "not found"),
            "404 must yield MizuError::Network with body text: {result:?}"
        );
    }

    /// 500 responses must also map to MizuError::Network.
    #[test]
    fn test_http_500_is_error() {
        let headers = http::HeaderMap::new();
        let result = parse_http_response(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            &headers,
            b"server exploded",
            "x.local",
        );
        assert!(
            matches!(result, Err(MizuError::Network(_))),
            "500 must yield MizuError::Network: {result:?}"
        );
    }

    /// 3xx responses must return Ok(Some(url)) with the body as the new URL.
    #[test]
    fn test_http_301_absolute_redirect() {
        let headers = http::HeaderMap::new();
        let result = parse_http_response(
            http::StatusCode::MOVED_PERMANENTLY,
            &headers,
            b"mizu://other.local/page",
            "origin.local",
        );
        assert_eq!(
            result.unwrap(),
            Some("mizu://other.local/page".to_string()),
            "absolute redirect URL must pass through unchanged"
        );
    }

    /// Relative redirect (no scheme) must be prepended with `mizu://<domain>`.
    #[test]
    fn test_http_302_relative_redirect_gets_domain_prefix() {
        let headers = http::HeaderMap::new();
        let result = parse_http_response(
            http::StatusCode::FOUND,
            &headers,
            b"/new/path",
            "example.local",
        );
        assert_eq!(
            result.unwrap(),
            Some("mizu://example.local/new/path".to_string()),
            "relative redirect must be prefixed with mizu://<domain>"
        );
    }

    /// Redirect via Location header must be preferred over the body.
    #[test]
    fn test_http_302_redirect_via_location_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::LOCATION,
            http::HeaderValue::from_static("/header-path"),
        );
        let result = parse_http_response(
            http::StatusCode::FOUND,
            &headers,
            b"/body-path",
            "example.local",
        );
        assert_eq!(
            result.unwrap(),
            Some("mizu://example.local/header-path".to_string()),
            "Location header must take precedence over body"
        );
    }

    /// Redirection with empty location and body must yield a Network error.
    #[test]
    fn test_http_302_empty_redirect_yields_error() {
        let headers = http::HeaderMap::new();
        let result = parse_http_response(
            http::StatusCode::FOUND,
            &headers,
            b"",
            "example.local",
        );
        assert!(
            matches!(result, Err(MizuError::Network(ref msg)) if msg.contains("Empty redirect destination")),
            "empty redirect must yield MizuError::Network: {result:?}"
        );
    }

    /// `parse_mizu_auth_set_header` must correctly parse a well-formed value.
    #[test]
    fn test_mizu_auth_set_header_parsed_ok() {
        let auth = parse_mizu_auth_set_header("tok123 SCOPE=GET,POST EXP=9999999999")
            .expect("valid header must parse");
        assert_eq!(auth.token, "tok123");
        assert_eq!(auth.scope, vec!["GET", "POST"]);
        assert_eq!(auth.exp, Some(9_999_999_999));
    }

    /// Auth header with no EXP field must be stored without exp.
    #[test]
    fn test_mizu_auth_set_header_missing_exp_is_none() {
        let auth = parse_mizu_auth_set_header("tok SCOPE=GET").expect("should parse");
        assert_eq!(auth.exp, None);
    }

    /// `process_mizu_auth_set` must reject a header without EXP.
    #[test]
    fn test_mizu_auth_set_missing_exp_rejected() {
        let result = process_mizu_auth_set("tok SCOPE=GET", "no-exp.local");
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "missing EXP must yield SecurityViolation: {result:?}"
        );
    }

    /// `process_mizu_auth_set` must reject already-expired tokens.
    #[test]
    fn test_mizu_auth_set_expired_token_rejected() {
        let result = process_mizu_auth_set("tok SCOPE=GET EXP=1", "expired.local");
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "expired token (EXP=1) must yield SecurityViolation: {result:?}"
        );
    }

    /// `process_mizu_auth_set` must reject tokens whose entire scope is outside
    /// the permitted-methods ceiling.
    #[test]
    fn test_mizu_auth_set_scope_ceiling_rejects_unknown_methods() {
        let future_exp = std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_secs() + 3600)
            .unwrap_or(9_999_999_999);
        let header = format!("tok SCOPE=HACK,TRACE EXP={future_exp}");
        let result = process_mizu_auth_set(&header, "ceiling.local");
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "scope with only forbidden methods must yield SecurityViolation: {result:?}"
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

        let wrong_protocol: Box<dyn std::any::Any> =
            Box::new(quinn::crypto::rustls::HandshakeData {
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

        let correct_protocol: Box<dyn std::any::Any> =
            Box::new(quinn::crypto::rustls::HandshakeData {
                protocol: Some(MIZU_ALPN.to_vec()),
                server_name: None,
            });
        assert!(
            verify_negotiated_alpn(Some(correct_protocol), "ok.mizu.test").is_ok(),
            "an exact mizu/3 match must be accepted"
        );
    }
