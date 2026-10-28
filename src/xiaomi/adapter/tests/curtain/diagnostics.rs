use super::*;

#[test]
fn terminal_lists_each_curtain_value_source_and_subscription() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                curtain(control).current_position.value == Some(37)
                    && curtain(control).target_position.value == Some(80)
                    && curtain(control).status.value == Some(CurtainMotion::Opening)
                    && work(control).function_observer("main").is_some()
            })
            .await;
            peer.notify_value_at("one", CURRENT, json!(38), Some(201), false);
            until(|| curtain(control).current_position.source_time == Some(201)).await;
            for command in ["list", "status one"] {
                let text = terminal(control, command).await;
                for expected in [
                    "Current position: 38%",
                    "Target position: 80%",
                    "Status: opening",
                    "upstream time 201",
                    "received at",
                    "observation source local",
                    "property 2.3",
                    "local subscription ready",
                ] {
                    assert!(text.contains(expected), "missing {expected}: {text}");
                }
            }
            for command in ["on one", "off one"] {
                let text = terminal(control, command).await;
                assert!(
                    text.contains("device does not support this command"),
                    "{text}"
                );
            }
            assert_eq!(peer.count("rpcReq", "one", 0), 0);
        })
        .await;
    }));
}

#[test]
fn curtain_unknown_and_individual_read_failure_are_visible_and_clear_on_recovery() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = peer();
        peer.state
            .borrow_mut()
            .read_fail
            .insert(("one".into(), 2, 4));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                curtain(control).current_position.value == Some(37)
                    && curtain(control).status.value == Some(CurtainMotion::Opening)
                    && work(control)
                        .refresh
                        .borrow()
                        .property_problem("main", TARGET)
                        .is_some()
            })
            .await;
            assert_eq!(curtain(control).target_position.value, None);
            for command in ["list", "status one"] {
                let text = terminal(control, command).await;
                let target = text
                    .lines()
                    .find(|line| line.contains("Target position: unknown"))
                    .unwrap();
                for expected in [
                    "upstream time unknown",
                    "received at unknown",
                    "observation source unknown",
                    "property 2.4",
                    "read: one read 2.4",
                ] {
                    assert!(target.contains(expected), "missing {expected}: {text}");
                }
            }
            peer.state.borrow_mut().read_fail.clear();
            work(control).refresh_property("main", TARGET);
            until(|| {
                curtain(control).target_position.value == Some(80)
                    && work(control)
                        .refresh
                        .borrow()
                        .property_problem("main", TARGET)
                        .is_none()
            })
            .await;
        })
        .await;
    }));
}
