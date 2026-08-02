//! Tests for the log module.

use super::*;

#[test]
fn ring_buffer_is_bounded() {
    let mut log = InspectorLog::new();
    for i in 0..(LOG_CAPACITY + 50) {
        log.push_event(EventKind::Mutation, format!("x = {i}"));
    }
    assert_eq!(log.events.len(), LOG_CAPACITY, "event log must stay capped");
    // Oldest entries must have been evicted: the first retained one is #50.
    assert!(
        log.events
            .front()
            .map(|e| e.detail.contains("x = 50"))
            .unwrap_or(false),
        "oldest entries must be evicted first"
    );
}

#[test]
fn complete_net_matches_by_correlation() {
    let mut log = InspectorLog::new();
    log.push_net_start("GET", "mizu://a/x", Some("status".to_string()));
    log.push_net_start("GET", "mizu://a/y", Some("other".to_string()));
    log.complete_net("status", NetOutcome::Ok, Some(42));

    let entry = log
        .network
        .iter()
        .find(|e| e.correlation.as_deref() == Some("status"))
        .cloned();
    let entry = match entry {
        Some(e) => e,
        None => panic!("entry with correlation 'status' must exist"),
    };
    assert_eq!(entry.outcome, NetOutcome::Ok);
    assert_eq!(entry.bytes, Some(42));
    assert!(entry.duration_ms.is_some(), "duration must be recorded");
    // The other request must still be pending.
    assert!(
        log.network
            .iter()
            .any(|e| e.outcome == NetOutcome::Pending && e.correlation.as_deref() == Some("other")),
        "unrelated pending entry must not be completed"
    );
}

#[test]
fn complete_net_without_start_appends_standalone_entry() {
    let mut log = InspectorLog::new();
    log.complete_net("ghost", NetOutcome::Failed("boom".into()), None);
    assert_eq!(log.network.len(), 1, "outcome must never be lost");
}

#[test]
fn truncate_detail_caps_length() {
    let long = "x".repeat(DETAIL_MAX_CHARS * 2);
    let out = truncate_detail(&long);
    assert!(out.chars().count() <= DETAIL_MAX_CHARS);
    assert!(out.ends_with('…'));
}

/// The cap exists to bound memory, not to fit the panel. A URL or an
/// error message long enough to overflow the panel must still reach the
/// paint pass intact, so that eliding it is a rendering decision that can
/// be revisited rather than data thrown away at the source.
#[test]
fn ordinary_long_details_survive_the_log_intact() {
    let url = format!("mizu://example.test/{}/leaf.json", "segment/".repeat(30));
    assert!(url.chars().count() > 200, "a realistically long target");
    assert_eq!(truncate_detail(&url), url);
}
