//! Tests for the history module: bounded stacks, encryption round-trip,
//! day-grouping, and sidebar state.

use super::crypto::*;
use super::*;

fn entry(url: &str) -> HistoryEntry {
    HistoryEntry {
        url: url.to_string(),
        scroll_y: 0.0,
    }
}

/// A visit `days` whole days in the past, with no title.
fn visit(url: &str, days: u64) -> VisitRecord {
    VisitRecord {
        url: url.to_string(),
        title: String::new(),
        timestamp_secs: now_secs() - days * 86_400,
    }
}

fn urls(stack: &[HistoryEntry]) -> Vec<&str> {
    stack.iter().map(|e| e.url.as_str()).collect()
}

#[test]
fn navigate_a_b_c_then_back_and_forward() {
    // Pure stack logic, no window needed: navigate A -> B -> C, then
    // walk back to A and forward to B, then a fresh navigation to D.
    let mut h = HistoryStack::default();

    // Start at A, navigate to B: push A.
    h.record_navigation(entry("A"));
    assert_eq!(urls(&h.back), vec!["A"]);
    assert!(!h.can_go_forward());

    // At B, navigate to C: push B.
    h.record_navigation(entry("B"));
    assert_eq!(urls(&h.back), vec!["A", "B"]);
    assert!(!h.can_go_forward());

    // At C, go back: target must be B; back=[A], forward=[C].
    let target = h.go_back(entry("C")).expect("back must be available");
    assert_eq!(target.url, "B");
    assert_eq!(urls(&h.back), vec!["A"]);
    assert_eq!(urls(&h.forward), vec!["C"]);

    // At B, go back again: target must be A; back=[], forward=[C,B].
    let target = h.go_back(entry("B")).expect("back must be available");
    assert_eq!(target.url, "A");
    assert!(urls(&h.back).is_empty());
    assert_eq!(urls(&h.forward), vec!["C", "B"]);

    // At A, go forward: target must be B; back=[A], forward=[C].
    let target = h.go_forward(entry("A")).expect("forward must be available");
    assert_eq!(target.url, "B");
    assert_eq!(urls(&h.back), vec!["A"]);
    assert_eq!(urls(&h.forward), vec!["C"]);

    // At B, a FRESH navigation to D: forward is cleared, back=[A,B].
    h.record_navigation(entry("B"));
    assert_eq!(urls(&h.back), vec!["A", "B"]);
    assert!(
        !h.can_go_forward(),
        "a fresh navigation must clear the forward stack"
    );
}

#[test]
fn go_back_on_empty_stack_is_a_no_op() {
    let mut h = HistoryStack::default();
    assert!(!h.can_go_back());
    let result = h.go_back(entry("current"));
    assert!(result.is_none());
    assert!(
        h.forward.is_empty(),
        "a no-op back must not push onto forward either"
    );
}

#[test]
fn go_forward_on_empty_stack_is_a_no_op() {
    let mut h = HistoryStack::default();
    assert!(!h.can_go_forward());
    let result = h.go_forward(entry("current"));
    assert!(result.is_none());
    assert!(h.back.is_empty());
}

#[test]
fn back_stack_is_capped_oldest_dropped() {
    let mut h = HistoryStack::default();
    for i in 0..(MAX_HISTORY_ENTRIES + 1) {
        h.record_navigation(entry(&format!("page-{i}")));
    }
    assert_eq!(
        h.back.len(),
        MAX_HISTORY_ENTRIES,
        "back stack must be capped"
    );
    assert_eq!(
        h.back.first().unwrap().url,
        "page-1",
        "oldest entry (page-0) must have been dropped"
    );
    assert_eq!(
        h.back.last().unwrap().url,
        format!("page-{MAX_HISTORY_ENTRIES}")
    );
}

#[test]
fn forward_stack_is_capped_oldest_dropped() {
    let mut h = HistoryStack::default();
    // Build a deep back stack, then walk it all the way back to fill forward.
    for i in 0..(MAX_HISTORY_ENTRIES + 1) {
        h.record_navigation(entry(&format!("page-{i}")));
    }
    let mut current = format!("page-{MAX_HISTORY_ENTRIES}");
    while h.can_go_back() {
        let target = h.go_back(entry(&current)).unwrap();
        current = target.url;
    }
    assert_eq!(
        h.forward.len(),
        MAX_HISTORY_ENTRIES,
        "forward stack must be capped"
    );
}

#[test]
fn scroll_y_round_trips_through_a_history_step() {
    let mut h = HistoryStack::default();
    h.record_navigation(HistoryEntry {
        url: "A".to_string(),
        scroll_y: 420.0,
    });
    let target = h
        .go_back(HistoryEntry {
            url: "B".to_string(),
            scroll_y: 0.0,
        })
        .unwrap();
    assert_eq!(target.scroll_y, 420.0);
}

// â”€â”€ HistoryLog tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[test]
fn log_push_capped_oldest_dropped() {
    let mut log = HistoryLog::default();
    for i in 0..(MAX_LOG_ENTRIES + 5) {
        log.push(visit(&format!("page-{i}"), 0));
    }
    assert_eq!(log.len(), MAX_LOG_ENTRIES);
    assert_eq!(
        log.get(0).unwrap().url,
        format!("page-{}", MAX_LOG_ENTRIES + 4),
        "the newest visit must be at index 0"
    );
}

#[test]
fn log_push_collapses_a_repeat_of_the_current_page() {
    let mut log = HistoryLog::default();
    log.push(VisitRecord::new("A".into(), String::new()));
    log.push(VisitRecord::new("A".into(), "Title".into()));
    assert_eq!(log.len(), 1, "reloading a page must not stack duplicates");
    assert_eq!(
        log.get(0).unwrap().title,
        "Title",
        "the repeat must refresh the record, not be discarded"
    );

    log.push(VisitRecord::new("B".into(), String::new()));
    log.push(VisitRecord::new("A".into(), String::new()));
    assert_eq!(log.len(), 3, "A â†’ B â†’ A is three distinct visits");
}

#[test]
fn log_clear_empties_records() {
    let mut log = HistoryLog::default();
    log.push(visit("A", 0));
    log.push(visit("B", 0));
    assert!(!log.is_empty());
    log.clear();
    assert!(log.is_empty());
    assert_eq!(log.len(), 0);
}

#[test]
fn log_groups_by_day_newest_group_first() {
    let mut log = HistoryLog::default();
    log.push(visit("old", 2));
    log.push(visit("yesterday", 1));
    log.push(visit("today-a", 0));
    log.push(visit("today-b", 0));

    let groups = log.grouped_by_day();
    let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, vec!["Today", "Yesterday", "2 days ago"]);
    assert_eq!(
        groups[0].1.len(),
        2,
        "same-day visits collapse into one group"
    );
    assert_eq!(
        groups[0].1[0].url, "today-b",
        "the newest visit leads its group"
    );
}

#[test]
fn day_label_is_singular_for_one_unit() {
    let mut log = HistoryLog::default();
    log.push(visit("a", 7));
    log.push(visit("b", 31));
    let labels: Vec<String> = log.grouped_by_day().into_iter().map(|(l, _)| l).collect();
    assert!(
        labels.contains(&"1 week ago".to_string()) && labels.contains(&"1 month ago".to_string()),
        "got {labels:?}"
    );
}

#[test]
fn encrypted_blob_round_trips_and_detects_tampering() {
    let key = [7u8; 32];
    let records = vec![
        VisitRecord {
            url: "mizu://test/a".into(),
            title: "A".into(),
            timestamp_secs: 1_000,
        },
        VisitRecord {
            url: "mizu://test/b".into(),
            title: String::new(),
            timestamp_secs: 2_000,
        },
    ];
    let plaintext = serde_json::to_vec(&records).unwrap();

    let blob = encrypt_blob(&key, &plaintext).expect("encryption must succeed");
    assert!(
        !blob.windows(5).any(|w| w == b"mizu:"),
        "URLs must not be readable in the stored blob"
    );

    let opened = decrypt_blob(&key, &blob).expect("decryption must succeed");
    let loaded = HistoryLog::from_newest_first(serde_json::from_slice(&opened).unwrap());
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded.get(0).unwrap().url, "mizu://test/a");
    assert_eq!(loaded.get(1).unwrap().timestamp_secs, 2_000);
    assert!(!loaded.dirty, "a freshly loaded log has nothing to save");

    let mut tampered = blob.clone();
    *tampered.last_mut().unwrap() ^= 0x01;
    assert!(
        decrypt_blob(&key, &tampered).is_none(),
        "a modified blob must fail authentication"
    );
    assert!(
        decrypt_blob(&[8u8; 32], &blob).is_none(),
        "the wrong key must fail authentication"
    );
    assert!(decrypt_blob(&key, b"short").is_none());
}

#[test]
fn encryption_never_reuses_a_nonce() {
    let key = [7u8; 32];
    let a = encrypt_blob(&key, b"same plaintext").unwrap();
    let b = encrypt_blob(&key, b"same plaintext").unwrap();
    assert_ne!(
        a[..NONCE_LEN],
        b[..NONCE_LEN],
        "GCM nonces must be fresh per save"
    );
}

#[test]
fn saving_is_skipped_until_the_log_changes() {
    let mut log = HistoryLog::default();
    assert!(!log.dirty, "an empty log has nothing to write");
    log.push(visit("A", 0));
    assert!(log.dirty);
}

#[test]
fn visit_display_label_prefers_title() {
    let titled = VisitRecord::new("mizu://x/page".into(), "My Page".into());
    assert_eq!(titled.display_label(), "My Page");
    let untitled = VisitRecord::new("mizu://x/other".into(), String::new());
    assert_eq!(untitled.display_label(), "mizu://x/other");
}

#[test]
fn unknown_or_future_timestamps_count_as_today() {
    let unknown = VisitRecord {
        url: "a".into(),
        title: String::new(),
        timestamp_secs: 0,
    };
    assert_eq!(unknown.day_label(), "Today");
    let future = VisitRecord {
        url: "b".into(),
        title: String::new(),
        timestamp_secs: now_secs() + 86_400,
    };
    assert_eq!(
        future.day_label(),
        "Today",
        "clock skew must not invent a group"
    );
}

#[test]
fn sidebar_state_toggle_resets_transient_state() {
    let mut state = HistorySidebarState::default();
    assert!(state.toggle(), "first toggle opens");
    state.scroll_offset = 120.0;
    state.hovered = Some(3);
    assert!(!state.toggle(), "second toggle closes");
    assert_eq!(state.scroll_offset, 0.0);
    assert_eq!(state.hovered, None);
}
