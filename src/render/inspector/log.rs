//! Always-on bounded runtime log feeding the inspector's Events and Network tabs.
//!
//! Both buffers are capped ring buffers ([`LOG_CAPACITY`] entries), so logging
//! is enabled unconditionally at a small fixed memory cost: the inspector can
//! be opened *after* a problem and still show its history.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::time::Instant;

/// Maximum retained entries per ring buffer.
const LOG_CAPACITY: usize = 256;

/// Maximum characters retained for a logged detail string.
const DETAIL_MAX_CHARS: usize = 96;

/// Truncates `s` to [`DETAIL_MAX_CHARS`] characters, appending `…` when cut.
pub(crate) fn truncate_detail(s: &str) -> String {
    if s.chars().count() <= DETAIL_MAX_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(DETAIL_MAX_CHARS - 1).collect();
    format!("{cut}…")
}

/// Category of a runtime event entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A click action was dispatched to the logic worker.
    Click,
    /// A node `every` timer or a root `timer` fired.
    Timer,
    /// A form submission was dispatched.
    Submit,
    /// A variable mutation was applied to the UI store.
    Mutation,
}

impl EventKind {
    /// Short tag shown at the start of a log row.
    pub fn tag(self) -> &'static str {
        match self {
            EventKind::Click => "click ",
            EventKind::Timer => "timer ",
            EventKind::Submit => "submit",
            EventKind::Mutation => "set   ",
        }
    }
}

/// One entry in the runtime event log.
#[derive(Debug, Clone)]
pub struct EventEntry {
    /// When the event happened.
    pub at: Instant,
    /// Event category.
    pub kind: EventKind,
    /// Human-readable detail, pre-truncated.
    pub detail: String,
}

/// Outcome of a network log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetOutcome {
    /// Dispatched; completion not yet observed.
    Pending,
    /// Completed successfully.
    Ok,
    /// The server answered with a redirect.
    Redirect,
    /// Failed with a network or server error.
    Failed(String),
    /// Blocked by a client-side capability policy before leaving the machine.
    Blocked(String),
}

impl NetOutcome {
    /// Short status tag shown in the log row.
    pub fn tag(&self) -> &'static str {
        match self {
            NetOutcome::Pending => "…",
            NetOutcome::Ok => "ok",
            NetOutcome::Redirect => "->",
            NetOutcome::Failed(_) => "ERR",
            NetOutcome::Blocked(_) => "BLOCKED",
        }
    }
}

/// One entry in the network activity log.
#[derive(Debug, Clone)]
pub struct NetEntry {
    /// When the request was dispatched (or the completion observed, for
    /// entries logged without a matching start).
    pub at: Instant,
    /// Verb tag: `GET` / `POST` / `NAV` / `IMG` / `STORE` / `MEDIA`.
    pub verb: String,
    /// Target URL or storage key, pre-truncated.
    pub target: String,
    /// Current outcome; `Pending` until a completion is correlated.
    pub outcome: NetOutcome,
    /// Milliseconds between dispatch and completion, when both were observed.
    pub duration_ms: Option<u64>,
    /// Response size in bytes, when known.
    pub bytes: Option<usize>,
    /// Correlation key used to match a later completion (fetches use the
    /// bound variable name; navigations use the URL).
    pub correlation: Option<String>,
}

/// Bounded ring buffers recording runtime activity for the inspector.
#[derive(Debug)]
pub struct InspectorLog {
    /// Instant the log was created; row timestamps are shown relative to it.
    pub epoch: Instant,
    /// Runtime events, oldest first.
    pub events: VecDeque<EventEntry>,
    /// Network activity, oldest first.
    pub network: VecDeque<NetEntry>,
}

impl Default for InspectorLog {
    fn default() -> Self {
        Self::new()
    }
}

impl InspectorLog {
    /// Creates an empty log anchored at the current instant.
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            events: VecDeque::with_capacity(64),
            network: VecDeque::with_capacity(64),
        }
    }

    /// Formats an instant as seconds since the log epoch, e.g. `"  12.4s"`.
    pub fn fmt_ts(&self, at: Instant) -> String {
        format!("{:>7.1}s", at.duration_since(self.epoch).as_secs_f32())
    }

    /// Records a runtime event.
    pub fn push_event(&mut self, kind: EventKind, detail: impl AsRef<str>) {
        if self.events.len() >= LOG_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(EventEntry {
            at: Instant::now(),
            kind,
            detail: truncate_detail(detail.as_ref()),
        });
    }

    /// Records a dispatched network operation awaiting completion.
    pub fn push_net_start(&mut self, verb: &str, target: &str, correlation: Option<String>) {
        self.push_net(NetEntry {
            at: Instant::now(),
            verb: verb.to_string(),
            target: truncate_detail(target),
            outcome: NetOutcome::Pending,
            duration_ms: None,
            bytes: None,
            correlation,
        });
    }

    /// Records an operation that completed (or was resolved) instantly.
    pub fn push_net_done(&mut self, verb: &str, target: &str, outcome: NetOutcome) {
        self.push_net(NetEntry {
            at: Instant::now(),
            verb: verb.to_string(),
            target: truncate_detail(target),
            outcome,
            duration_ms: None,
            bytes: None,
            correlation: None,
        });
    }

    /// Records an operation blocked by a capability policy.
    pub fn push_net_blocked(&mut self, verb: &str, target: &str, reason: String) {
        self.push_net_done(verb, target, NetOutcome::Blocked(reason));
    }

    /// Completes the most recent `Pending` entry whose correlation key equals
    /// `correlation`, recording the outcome, elapsed time, and optional size.
    ///
    /// If no matching entry exists (e.g. the start rolled out of the ring),
    /// a standalone completed entry is appended instead so the outcome is
    /// never lost.
    pub fn complete_net(
        &mut self,
        correlation: &str,
        outcome: NetOutcome,
        bytes: Option<usize>,
    ) {
        let now = Instant::now();
        if let Some(entry) = self.network.iter_mut().rev().find(|e| {
            e.outcome == NetOutcome::Pending && e.correlation.as_deref() == Some(correlation)
        }) {
            entry.duration_ms = Some(now.duration_since(entry.at).as_millis() as u64);
            entry.outcome = outcome;
            entry.bytes = bytes;
            return;
        }
        self.push_net(NetEntry {
            at: now,
            verb: "?".to_string(),
            target: truncate_detail(correlation),
            outcome,
            duration_ms: None,
            bytes,
            correlation: None,
        });
    }

    /// Completes the most recent `Pending` entry regardless of correlation.
    ///
    /// Used for results that carry no correlation key (e.g. a server redirect,
    /// which arrives before any variable binding exists).
    pub fn complete_latest_pending(&mut self, outcome: NetOutcome) {
        let now = Instant::now();
        if let Some(entry) = self
            .network
            .iter_mut()
            .rev()
            .find(|e| e.outcome == NetOutcome::Pending)
        {
            entry.duration_ms = Some(now.duration_since(entry.at).as_millis() as u64);
            entry.outcome = outcome;
        }
    }

    fn push_net(&mut self, entry: NetEntry) {
        if self.network.len() >= LOG_CAPACITY {
            self.network.pop_front();
        }
        self.network.push_back(entry);
    }
}

#[cfg(test)]
mod tests {
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
                .any(|e| e.outcome == NetOutcome::Pending
                    && e.correlation.as_deref() == Some("other")),
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
        let long = "x".repeat(500);
        let out = truncate_detail(&long);
        assert!(out.chars().count() <= DETAIL_MAX_CHARS);
        assert!(out.ends_with('…'));
    }
}
