use super::*;
use std::sync::atomic::Ordering;

#[test]
fn fan_fast_properties_and_other_state_use_distinct_periods() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                fan(control).speed.value == Some(40)
                    && fan(control).swing.value == Some(false)
                    && work(control).function_observer("fan:2").is_some()
                    && [POWER, MODE, SPEED, SWING]
                        .iter()
                        .all(|binding| !work(control).refresh.borrow().contains("fan:2", *binding))
            })
            .await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            let before = peer.count("get", "one", 0);
            control.clock.advance(Duration::from_secs(61));
            control.clock.automatic.store(true, Ordering::Relaxed);
            until(|| {
                peer.count("get", "one", 0) >= before + 4
                    && [POWER, MODE, SPEED, SWING]
                        .iter()
                        .all(|binding| !work(control).refresh.borrow().contains("fan:2", *binding))
            })
            .await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            peer.value("one", SPEED, json!(72));
            peer.value("one", SWING, json!(true));
            control.clock.advance(Duration::from_secs(17));
            control.clock.automatic.store(true, Ordering::Relaxed);
            until(|| fan(control).speed.value == Some(72)).await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            assert_eq!(fan(control).swing.value, Some(false));

            control.clock.advance(Duration::from_secs(61));
            control.clock.automatic.store(true, Ordering::Relaxed);
            until(|| fan(control).swing.value == Some(true)).await;
            assert!(work(control).function_observer("fan:2").is_some());
        })
        .await;
    }));
}
