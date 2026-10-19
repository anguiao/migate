use super::*;
use std::time::{Duration, Instant};

#[test]
fn ping_wait_precedes_the_next_keepalive_and_expires() {
    let now = Instant::now();
    let interval = Some(Duration::from_secs(60));
    let mut keepalive = Keepalive::new(now);
    assert_eq!(
        keepalive.next_deadline(interval),
        Some(now + Duration::from_secs(60))
    );
    assert!(!keepalive.ping_expired(now));
    assert_eq!(
        keepalive.begin_ping(
            now + Duration::from_secs(59),
            interval,
            Duration::from_secs(10)
        ),
        None
    );
    let ping = keepalive
        .begin_ping(
            now + Duration::from_secs(60),
            interval,
            Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(ping, now + Duration::from_secs(70));
    assert_eq!(keepalive.next_deadline(interval), Some(ping));
    assert_eq!(
        keepalive.begin_ping(
            now + Duration::from_secs(61),
            interval,
            Duration::from_secs(10)
        ),
        None,
        "an outstanding ping wait is not duplicated"
    );
    assert!(!keepalive.ping_expired(now + Duration::from_secs(69)));
    assert!(keepalive.ping_expired(ping));
    keepalive.pong();
    assert_eq!(
        keepalive.next_deadline(interval),
        Some(now + Duration::from_secs(60)),
        "the next deadline still counts from the last completed send"
    );
    keepalive.sent(now + Duration::from_secs(5));
    assert_eq!(
        keepalive.next_deadline(interval),
        Some(now + Duration::from_secs(65)),
        "a completed send moves the next keepalive deadline"
    );
}

#[test]
fn completed_sends_move_the_keepalive_origin() {
    let now = Instant::now();
    let interval = Some(Duration::from_secs(30));
    let mut keepalive = Keepalive::new(now);
    keepalive.sent(now + Duration::from_secs(10));
    assert_eq!(
        keepalive.next_deadline(interval),
        Some(now + Duration::from_secs(40))
    );
    assert_eq!(
        keepalive.begin_ping(
            now + Duration::from_secs(39),
            interval,
            Duration::from_secs(5)
        ),
        None
    );
}

#[test]
fn no_interval_means_no_protocol_deadline() {
    let now = Instant::now();
    let mut keepalive = Keepalive::new(now);
    assert_eq!(keepalive.next_deadline(None), None);
    assert_eq!(
        keepalive.begin_ping(now, None, Duration::from_secs(5)),
        None
    );
}
