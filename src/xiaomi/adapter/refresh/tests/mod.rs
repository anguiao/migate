use super::*;

#[test]
fn dirty_again_produces_exactly_one_follow_up_read() {
    let mut state = RefreshState::default();
    let binding = PropertyBinding { siid: 2, piid: 1 };
    let key = super::super::lighting::key("main", binding);
    state.dirty_again.insert(key.clone());
    assert!(state.finish_dirty("main", binding));
    assert!(!state.dirty_again.contains(&key));
    assert_eq!(state.pending.len(), 1);
    assert!(!state.finish_dirty("main", binding));
    assert_eq!(state.pending.len(), 1);
}
