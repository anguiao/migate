use super::*;
use serde_json::json;

const BRIGHTNESS: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
const TEMPERATURE: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };

#[test]
fn delayed_repeated_and_out_of_order_reports_match_without_extending_the_deadline() {
    let mut feedback = LightFeedback::default();
    feedback.sent(&[(TEMPERATURE, json!(3378))]);
    feedback.complete([TEMPERATURE].into_iter(), Duration::ZERO);
    feedback.sent(&[(TEMPERATURE, json!(3390))]);
    feedback.complete([TEMPERATURE].into_iter(), Duration::ZERO);
    for value in [3378, 3390, 3378, 3390] {
        assert!(feedback.matches(TEMPERATURE, &json!(value), Duration::from_millis(2999)));
    }
    assert!(!feedback.matches(TEMPERATURE, &json!(3500), Duration::from_millis(2999)));
    assert!(!feedback.matches(TEMPERATURE, &json!(3378), Duration::from_secs(3)));
    assert!(!feedback.matches(TEMPERATURE, &json!(3390), Duration::from_secs(3)));
}

#[test]
fn reports_before_receipt_do_not_shorten_the_window_started_by_success() {
    let mut feedback = LightFeedback::default();
    feedback.sent(&[(BRIGHTNESS, json!(20))]);
    assert!(feedback.matches(BRIGHTNESS, &json!(20), Duration::from_secs(1)));
    assert!(feedback.matches(BRIGHTNESS, &json!(20), Duration::from_secs(2)));
    feedback.complete([BRIGHTNESS].into_iter(), Duration::from_secs(2));
    assert!(feedback.matches(BRIGHTNESS, &json!(20), Duration::from_millis(4999)));
    assert!(!feedback.matches(BRIGHTNESS, &json!(20), Duration::from_secs(5)));
}

#[test]
fn a_new_accepted_write_of_the_same_value_has_its_own_deadline() {
    let mut feedback = LightFeedback::default();
    feedback.sent(&[(BRIGHTNESS, json!(20))]);
    feedback.complete([BRIGHTNESS].into_iter(), Duration::ZERO);
    feedback.sent(&[(BRIGHTNESS, json!(20))]);
    feedback.complete([BRIGHTNESS].into_iter(), Duration::from_secs(2));
    assert!(feedback.matches(BRIGHTNESS, &json!(20), Duration::from_secs(4)));
    assert!(!feedback.matches(BRIGHTNESS, &json!(20), Duration::from_secs(5)));
}

#[test]
fn only_accepted_properties_survive_completion_and_permit_release() {
    let mut feedback = LightFeedback::default();
    feedback.sent(&[(BRIGHTNESS, json!(20)), (TEMPERATURE, json!(3000))]);
    feedback.complete([TEMPERATURE].into_iter(), Duration::ZERO);
    feedback.cancel();
    assert!(!feedback.matches(BRIGHTNESS, &json!(20), Duration::ZERO));
    assert!(feedback.matches(TEMPERATURE, &json!(3000), Duration::ZERO));
    feedback.sent(&[(BRIGHTNESS, json!(40))]);
    feedback.cancel();
    assert!(!feedback.matches(BRIGHTNESS, &json!(40), Duration::ZERO));
    assert!(feedback.matches(TEMPERATURE, &json!(3000), Duration::ZERO));
}

#[test]
fn reports_cannot_create_history_for_unsent_values_or_other_properties() {
    let mut feedback = LightFeedback::default();
    feedback.sent(&[(BRIGHTNESS, json!(20))]);
    feedback.complete([BRIGHTNESS].into_iter(), Duration::ZERO);
    assert!(!feedback.matches(TEMPERATURE, &json!(20), Duration::ZERO));
    assert!(!feedback.matches(BRIGHTNESS, &json!(40), Duration::ZERO));
    assert!(!feedback.matches(BRIGHTNESS, &json!(40), Duration::from_secs(1)));
}

#[test]
fn missing_notifications_cannot_grow_history_without_bound() {
    let mut feedback = LightFeedback::default();
    for value in 0..=CAPACITY {
        feedback.sent(&[(TEMPERATURE, json!(2700 + value))]);
        feedback.complete([TEMPERATURE].into_iter(), Duration::ZERO);
    }
    assert!(!feedback.matches(TEMPERATURE, &json!(2700), Duration::ZERO));
    assert!(feedback.matches(TEMPERATURE, &json!(2701), Duration::ZERO));
    assert!(feedback.matches(TEMPERATURE, &json!(2700 + CAPACITY), Duration::ZERO));
    assert!(feedback.is_empty(Duration::from_secs(3)));
}

#[test]
fn native_echo_tickets_survive_overlapping_pending_and_scope_ack_and_clear() {
    let mut feedback = LightFeedback::default();
    let first = feedback.expect_pending(&[(BRIGHTNESS, json!(20))]);
    let second = feedback.expect_pending(&[(BRIGHTNESS, json!(40))]);
    // Pending expectations do not expire before their own receipt, even far
    // past the short post-ack window.
    assert!(feedback.matches(BRIGHTNESS, &json!(20), Duration::from_secs(30)));
    assert!(feedback.matches(BRIGHTNESS, &json!(40), Duration::from_secs(30)));
    feedback.expect_ack(first, Duration::ZERO);
    assert!(!feedback.matches(BRIGHTNESS, &json!(20), Duration::from_secs(30)));
    assert!(feedback.matches(BRIGHTNESS, &json!(40), Duration::from_secs(30)));
    feedback.clear_expected(second);
    assert!(!feedback.matches(BRIGHTNESS, &json!(40), Duration::from_secs(30)));
}
