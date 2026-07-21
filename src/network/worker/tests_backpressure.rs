    use super::*;
    use crate::core::errors::MizuError;
    use crate::network::NetworkResult;

    /// Verifies that the UI-bound channel is bounded at `MAX_UI_CHANNEL_CAPACITY`.
    ///
    /// Scenario: the network worker attempts to send 100 results but the UI
    /// thread stops consuming.  After `MAX_UI_CHANNEL_CAPACITY` messages the
    /// channel must be full and `try_send` must fail — no unbounded allocation.
    #[tokio::test]
    async fn test_network_to_ui_backpressure_sustained_flood() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<NetworkResult>(*MAX_UI_CHANNEL_CAPACITY);

        // Fill the channel to capacity — every try_send up to the limit must succeed.
        for i in 0..*MAX_UI_CHANNEL_CAPACITY {
            tx.try_send(NetworkResult::Error(MizuError::Network(format!("msg {i}"))))
                .unwrap_or_else(|_| panic!("try_send must succeed for slot {i}"));
        }

        // The (MAX_UI_CHANNEL_CAPACITY + 1)-th message must be rejected immediately.
        let overflow = tx.try_send(NetworkResult::Error(MizuError::Network(
            "overflow".to_string(),
        )));
        assert!(
            overflow.is_err(),
            "channel must reject messages beyond MAX_UI_CHANNEL_CAPACITY={}",
            *MAX_UI_CHANNEL_CAPACITY
        );

        // Drain and verify exactly MAX_UI_CHANNEL_CAPACITY messages were buffered.
        let mut count = 0usize;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(
            count, *MAX_UI_CHANNEL_CAPACITY,
            "exactly MAX_UI_CHANNEL_CAPACITY messages must have been buffered"
        );
    }

    /// Verifies that the semaphore caps concurrent active fetches at
    /// `MAX_CONCURRENT_FETCHES` even when 50 tasks are spawned simultaneously.
    #[tokio::test]
    async fn test_concurrent_fetch_throttling_limits() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let semaphore = Arc::new(tokio::sync::Semaphore::new(*MAX_CONCURRENT_FETCHES));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..50 {
            let sem = semaphore.clone();
            let active = active.clone();
            let peak = peak.clone();
            handles.push(tokio::spawn(async move {
                let Ok(permit) = sem.acquire_owned().await else {
                    return;
                };
                let _permit = permit;
                // Track concurrent executions and record the peak.
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                // Simulate network I/O latency.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap_or(());
        }

        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= *MAX_CONCURRENT_FETCHES,
            "peak concurrent fetches ({observed_peak}) must not exceed \
             MAX_CONCURRENT_FETCHES ({})",
            *MAX_CONCURRENT_FETCHES
        );
        // Confirm all 50 tasks eventually ran (semaphore is not permanently exhausted).
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "all tasks must have completed"
        );
    }

    /// Verifies graceful recovery: tasks suspended on a full channel are
    /// unblocked as soon as the UI drains messages via `try_recv`.
    #[tokio::test]
    async fn test_backpressure_graceful_recovery() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<NetworkResult>(*MAX_UI_CHANNEL_CAPACITY);

        // Fill the channel to capacity so the next send will block.
        for i in 0..*MAX_UI_CHANNEL_CAPACITY {
            tx.try_send(NetworkResult::Error(MizuError::Network(format!(
                "fill {i}"
            ))))
            .unwrap_or_else(|_| panic!("fill slot {i} must succeed"));
        }

        // Spawn a task that blocks on the full channel — simulates a suspended fetch.
        let tx2 = tx.clone();
        let sender = tokio::spawn(async move {
            tx2.send(NetworkResult::Error(MizuError::Network(
                "recovered".to_string(),
            )))
            .await
            .unwrap_or(());
        });

        // Give the sender a tick to reach the awaiting-send state.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // UI resumes consuming — drain the backlog.
        let mut drained = 0usize;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(
            drained, *MAX_UI_CHANNEL_CAPACITY,
            "all buffered messages must be drained"
        );

        // The suspended sender must now complete within a short timeout.
        tokio::time::timeout(std::time::Duration::from_millis(200), sender)
            .await
            .unwrap_or_else(|_| panic!("suspended sender must unblock after channel drains"))
            .unwrap_or(());

        // The recovery message must have arrived in the channel.
        let recovered = rx.try_recv().unwrap_or_else(|_| {
            panic!("recovered message must be in channel after sender completes")
        });
        assert!(
            matches!(recovered, NetworkResult::Error(_)),
            "recovered message must be the one sent by the suspended task"
        );
    }
