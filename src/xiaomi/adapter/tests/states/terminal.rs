//! State interpretation and binding diagnostics in the terminal.
use super::*;

#[test]
fn native_state_terminal_distinguishes_event_subscriptions_and_observation_metadata() {
    block_on(motion_rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(motion_peer(), |peer| async move {
            ready(control, "motion:2:1024").await;
            event(&peer, false, Some(201));
            until(|| state(control, "motion:2:1024").source_time == Some(201)).await;
            let terminal = control.terminal();
            let text = terminal.handle_line("status one").await.unwrap().unwrap();
            for expected in [
                "motion detected",
                "native event 2.1008",
                "upstream time 201",
                "received at",
                "Observation source: hub",
                "property 2.1024",
                "event 2.1008",
                "bindings property 2.1024, event 2.1008",
            ] {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
        })
        .await;
    }));
}
