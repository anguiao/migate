//! Current source subscriptions projected through the real diagnostic and terminal exits.
use super::*;
use crate::xiaomi::capability::PropertyBinding;

#[test]
fn outlet_and_measurement_diagnostics_keep_both_source_results_without_duplicate_bindings() {
    for (rig, did, id, binding) in [
        (
            Rig::new(true, &["one"]),
            "one",
            "outlet:2",
            PropertyBinding { siid: 2, piid: 1 },
        ),
        (
            Rig::sensors(&["sensor"]),
            "sensor",
            "temperature:3:1001",
            PropertyBinding {
                siid: 3,
                piid: 1001,
            },
        ),
    ] {
        let (rig, listener, cloud) = with_cloud_rig(rig);
        block_on(rig.run(|c| async move {
            future::or(cloud.run(&listener), async {
                until(|| cloud_active(&c, did)).await;
                c.announce();
                let local = Rc::new(Peer::default());
                local.state.borrow_mut().sub_reject.insert((
                    did.into(),
                    binding.siid,
                    binding.piid,
                ));
                let control = &c;
                c.peer(local, |local| async move {
                    until(|| {
                        local.count("subscribe", did, binding.siid) >= 4
                            && control.context.directory.borrow().devices[did]
                                .admitted
                                .as_ref()
                                .unwrap()
                                .test_local_exhausted()
                    })
                    .await;
                    control
                        .clock
                        .automatic
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    let status = control.context.diagnostics();
                    let device = status
                        .devices
                        .iter()
                        .find(|device| device.did == did)
                        .unwrap();
                    let function = device
                        .functions
                        .iter()
                        .find(|function| function.function == id)
                        .unwrap();
                    let subscriptions = function
                        .subscriptions
                        .iter()
                        .chain(
                            function
                                .properties
                                .iter()
                                .flat_map(|property| &property.subscriptions),
                        )
                        .collect::<Vec<_>>();
                    assert!(
                        subscriptions
                            .iter()
                            .any(|subscription| subscription.source == PathKind::Local
                                && !subscription.ready
                                && subscription.problem.is_some()),
                        "{function:?}"
                    );
                    assert!(
                        subscriptions
                            .iter()
                            .any(|subscription| subscription.source == PathKind::Cloud
                                && subscription.ready
                                && subscription.problem.is_none()),
                        "{function:?}"
                    );
                    for subscription in &subscriptions {
                        assert_eq!(
                            subscriptions
                                .iter()
                                .filter(|other| other.binding == subscription.binding
                                    && other.source == subscription.source)
                                .count(),
                            1
                        );
                    }
                    assert_eq!(control.context.diagnostics(), status);
                    assert_eq!(control.context.diagnostics(), status);
                    let text = control
                        .terminal()
                        .handle_line(&format!("status {did}"))
                        .await
                        .unwrap()
                        .unwrap();
                    for expected in [
                        format!("hub property {}.{}: not ready", binding.siid, binding.piid),
                        format!("cloud property {}.{}: ready", binding.siid, binding.piid),
                    ] {
                        assert_eq!(text.matches(&expected).count(), 1, "{text}");
                    }
                })
                .await;
            })
            .await;
        }));
    }
}
