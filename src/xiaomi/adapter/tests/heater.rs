use super::*;
use crate::device::{HeaterCommand, HeaterMode, HeaterSnapshot};
use crate::xiaomi::capability::PropertyBinding;
use futures_lite::future::poll_once;
use serde_json::{Value, json};

const ID: &str = "heater:3";
const HEATING: PropertyBinding = PropertyBinding { siid: 3, piid: 3 };
const TARGET: PropertyBinding = PropertyBinding { siid: 3, piid: 8 };

fn rig() -> Rig {
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap();
    Rig::custom_lights(&[("one", "yeelink.bhf_light.v13", instance)])
}

fn work(c: &Control) -> Rc<super::work::DeviceWork> {
    c.context.directory.borrow().devices["one"]
        .work
        .as_ref()
        .unwrap()
        .clone()
}

fn snapshot(c: &Control) -> HeaterSnapshot {
    function(&c.context.registry, "one", ID)
        .snapshot()
        .heater()
        .unwrap()
}

fn peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    peer.value("one", HEATING, json!(false));
    peer.value("one", TARGET, json!(25));
    peer
}

fn writes(peer: &Peer) -> Vec<(Value, Value, Value)> {
    peer.state
        .borrow()
        .seen
        .iter()
        .filter(|(op, value)| op == "rpcReq" && value["did"] == "one")
        .map(|(_, value)| {
            let item = &value["rpc"]["params"][0];
            (
                item["siid"].clone(),
                item["piid"].clone(),
                item["value"].clone(),
            )
        })
        .collect()
}

async fn accept(peer: &Peer, binding: PropertyBinding) {
    peer.release_property(
        "one",
        binding,
        json!({"result":[{"did":"one","siid":binding.siid,"piid":binding.piid,"code":0}]}),
    )
    .await;
}

#[test]
fn heating_uses_standard_properties_for_feedback_and_separate_writes() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                snapshot(control).power.value == Some(false)
                    && snapshot(control).temperature.value == Some(25)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            for binding in [HEATING, TARGET] {
                assert!(
                    work(control).subscription_status(ID).iter().any(|status| {
                        status.source == PathKind::Local
                            && status.binding
                                == super::model::SubscriptionBinding::Property(binding)
                            && status.ready
                    }),
                    "{binding:?}"
                );
            }
            let function = function(&control.context.registry, "one", ID);
            let before = function.snapshot();
            peer.state.borrow_mut().hold_writes = true;
            let temperature = function
                .begin_heater(HeaterCommand::Temperature(30))
                .unwrap();
            until(|| writes(&peer).len() == 1).await;
            assert_eq!(writes(&peer)[0], (json!(3), json!(8), json!(30)));
            assert!(poll_once(temperature.accepted()).await.is_none());
            assert_eq!(function.snapshot(), before);
            accept(&peer, TARGET).await;
            temperature.accepted().await.unwrap();
            assert_eq!(function.snapshot(), before);

            let heat = function
                .begin_heater(HeaterCommand::Mode(HeaterMode::Heat))
                .unwrap();
            until(|| writes(&peer).len() == 2).await;
            assert_eq!(writes(&peer)[1], (json!(3), json!(3), json!(true)));
            accept(&peer, HEATING).await;
            heat.accepted().await.unwrap();
            assert_eq!(snapshot(control).power.value, Some(false));
            peer.notify_value_at("one", HEATING, json!(true), Some(700), false);
            peer.notify_value_at("one", TARGET, json!(30), Some(701), false);
            until(|| {
                snapshot(control).power.value == Some(true)
                    && snapshot(control).temperature.value == Some(30)
            })
            .await;
            assert_eq!(snapshot(control).temperature.source_time, Some(701));
            assert_eq!(
                snapshot(control).temperature_source,
                crate::device::TemperatureSourceState::Unavailable
            );
        })
        .await;
    }));
}
