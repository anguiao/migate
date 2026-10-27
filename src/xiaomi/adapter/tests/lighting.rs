use super::*;
use crate::device::{LightColorTarget, LightDimension, LightMode, LightTarget, Rgb};
use crate::xiaomi::capability::PropertyBinding;

fn profile_instance(brightness: bool, temperature: bool) -> Value {
    let mut instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"
    ))
    .unwrap();
    instance["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|property| {
            let kind = property["type"].as_str().unwrap_or_default();
            kind.contains(":on:")
                || (brightness && kind.contains(":brightness:"))
                || (temperature && kind.contains(":color-temperature:"))
        });
    instance
}

const BRIGHTNESS: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
const TEMPERATURE: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };
const COLOR: PropertyBinding = PropertyBinding { siid: 4, piid: 1 };
const MODE: PropertyBinding = PropertyBinding { siid: 4, piid: 2 };
const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };
fn peer_values(c: &Control) -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    let bindings = c.data[0].device().functions()[0].lighting().unwrap();
    let brightness = bindings.brightness().unwrap();
    let temperature = bindings.temperature().unwrap();
    peer.value("one", POWER, json!(true));
    peer.value(
        "one",
        brightness.property(),
        json!(brightness.range().min()),
    );
    peer.value(
        "one",
        temperature.property(),
        json!(temperature.range().min()),
    );
    if let Some(color) = bindings.color() {
        peer.value("one", color.property(), json!(0x123456));
        peer.value("one", bindings.mode().unwrap().property(), json!(0));
    }
    peer
}
async fn ready_light(c: &Control) -> crate::device::Function {
    until(|| c.context.active_notification("one") == Some(PathKind::Local)).await;
    let function = function(&c.context.registry, "one", "main");
    until(|| {
        let snapshot = function.snapshot();
        let light = snapshot.lighting().unwrap();
        snapshot
            .power()
            .and_then(crate::device::Power::value)
            .is_some()
            && light.brightness_percent.is_some()
            && light.temperature_kelvin.is_some()
            && light.mode.is_some()
            && (c.data[0].device().functions()[0]
                .lighting()
                .unwrap()
                .color()
                .is_none()
                || light.color.is_some())
    })
    .await;
    function
}
fn brightness(percent: f64) -> LightTarget {
    LightTarget::brightness(None, percent).expect("valid test target")
}
fn color_target(color: LightColorTarget) -> LightTarget {
    match color {
        LightColorTarget::Temperature(kelvin) => {
            LightTarget::temperature(None, kelvin).expect("valid test target")
        }
        LightColorTarget::Rgb(rgb) => LightTarget::rgb(None, rgb),
    }
}
fn freeze_light_clock(c: &Control) {
    c.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
}
async fn ready_replacement_light(c: &Control) -> crate::device::Function {
    let before = c.context.registry.get(&DeviceKey::new("xiaomi", "one"));
    future::or(
        async {
            until(|| {
                c.context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .is_some_and(|now| before.as_ref().is_none_or(|old| !old.same_instance(&now)))
                    && c.context.active_notification("one")
                        == Some(super::super::model::PathKind::Local)
            })
            .await;
            ready_light(c).await
        },
        async {
            // Replacement waits 10 ms for subscriptions, then reads one property
            // per 200 ms round. Bound recovery below the accepted feedback window.
            for _ in 0..100 {
                async_io::Timer::after(Duration::from_millis(1)).await;
                c.clock.advance(Duration::from_millis(10));
            }
            panic!("replacement light did not become ready within one simulated second");
        },
    )
    .await
}
async fn received_light_frame(c: &Control, target: LightTarget) {
    until(|| {
        c.context.diagnostics().devices[0].commands[0]
            .last
            .as_ref()
            .is_some_and(|status| {
                status.target == super::super::model::CommandTarget::Light(target)
                    && status.result == Some(Ok(()))
            })
    })
    .await;
}
fn accepted_items(items: &[(PropertyBinding, i64)]) -> Value {
    json!({"result":items.iter().map(|(binding,code)|json!({"did":"one","siid":binding.siid,"piid":binding.piid,"code":code})).collect::<Vec<_>>()})
}

mod commands;
mod feedback;
mod lifecycle;
mod observations;
