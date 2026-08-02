//! Tests for `event_loop.rs`: background-tab timer throttling.

#[test]
fn background_timers_are_throttled_but_still_fire() {
    use super::super::event_loop::background_timer_period;
    assert_eq!(
        background_timer_period(100),
        1000,
        "a hidden document must not wake the loop 10x a second"
    );
    assert_eq!(
        background_timer_period(5000),
        5000,
        "a slower timer keeps its own period; the clamp is a floor, not a rewrite"
    );
}
