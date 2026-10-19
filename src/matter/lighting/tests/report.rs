use super::super::report::RemainingTime;

#[test]
fn long_moves_saturate_at_the_cluster_maximum_without_wrapping() {
    let mut color = RemainingTime::default();
    assert!(color.write(65_279_000, true, 0xfffe));
    assert_eq!(color.value, 0xfffe);
    assert!(!color.write(1000, false, 0xfffe));
    assert!(color.write(0, false, 0xfffe));
    let mut level = RemainingTime::default();
    assert!(level.write(65_279_000, true, u16::MAX));
    assert_eq!(level.value, u16::MAX);
}

#[test]
fn short_remaining_time_is_readable_without_subscription_reports() {
    let mut remaining = RemainingTime::default();
    assert!(!remaining.write(100, true, 0xfffe));
    assert_eq!(remaining.value, 1);
    assert!(!remaining.write(0, false, 0xfffe));
    assert!(!remaining.write(1000, true, 0xfffe));
    assert!(remaining.write(0, false, 0xfffe));
    assert!(remaining.write(2000, true, 0xfffe));
    assert!(!remaining.write(100, false, 0xfffe));
    assert!(remaining.write(0, false, 0xfffe));
}
