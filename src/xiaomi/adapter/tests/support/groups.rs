use super::*;

pub(in super::super) fn group_record(
    record_did: &str,
    group_did: &str,
    members: &[&str],
    physical: bool,
) -> Value {
    let member_ship = members
        .iter()
        .map(|did| ((*did).to_string(), serde_json::json!("1")))
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "did": record_did,
        "name": "Native group",
        "model": "mijia.light.group3",
        "spec_type": "urn:miot-spec-v2:device:light:0000A001:mijia-group3:3:0000C802",
        "isOnline": true,
        "extra": {
            "member_list": {
                "group_did": group_did,
                "status": "1",
                "display_member_switch": 0,
                "member_ship": member_ship,
                "is_physical": physical
            }
        }
    })
}

pub(in super::super) fn group_instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap()
}

pub(in super::super) fn ls2307_instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/devcea-light-ls2307.json"
    ))
    .unwrap()
}

pub(in super::super) const MEMBER_COLOR: crate::xiaomi::capability::PropertyBinding =
    crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 1 };
pub(in super::super) const MEMBER_MODE: crate::xiaomi::capability::PropertyBinding =
    crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 2 };

/// Two ls2307 members plus one native group3 relation. `member_mode` and
/// `member_power` seed the confirmed member observations.
pub(in super::super) fn repair_rig(member_mode: u64, member_power: bool) -> Rig {
    let rig = Rig::lights(&[
        ("member-a", "devcea.light.ls2307"),
        ("member-b", "devcea.light.ls2307"),
    ]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups = vec![group_record(
            "group-record",
            "group-1",
            &["member-a", "member-b"],
            false,
        )];
        cloud.catalog_instances.insert(
            group_instance()["type"].as_str().unwrap().into(),
            group_instance(),
        );
        cloud
            .online
            .extend(["group-1", "member-a", "member-b"].map(str::to_string));
        for did in ["group-1", "member-a", "member-b"] {
            for (siid, piid, value) in [
                (2, 1, json!(member_power)),
                (2, 2, json!(50)),
                (2, 3, json!(2700)),
            ] {
                cloud.raw_values.insert((did.into(), siid, piid), value);
            }
        }
        for did in ["member-a", "member-b"] {
            cloud.raw_values.insert(
                (did.into(), MEMBER_COLOR.siid, MEMBER_COLOR.piid),
                json!(0x112233),
            );
            cloud.raw_values.insert(
                (did.into(), MEMBER_MODE.siid, MEMBER_MODE.piid),
                json!(member_mode),
            );
        }
    }
    rig
}

pub(in super::super) async fn repair_ready(c: &Control) {
    c.announce();
    until(|| {
        c.context
            .live
            .borrow()
            .devices
            .get("group-1")
            .is_some_and(|slot| slot.admitted.is_some())
    })
    .await;
    until(|| {
        ["member-a", "member-b"].iter().all(|did| {
            c.context
                .live
                .borrow()
                .devices
                .get(*did)
                .is_some_and(|slot| slot.admitted.is_some() && slot.admitted.is_some())
        })
    })
    .await;
    until(|| {
        c.context
            .registry
            .get(&DeviceKey::new("xiaomi", "group-1"))
            .is_some_and(|physical| physical.availability() == Availability::Available)
    })
    .await;
    until(|| {
        ["member-a", "member-b"].iter().all(|did| {
            function(&c.context.registry, did, "main")
                .snapshot()
                .lighting()
                .is_some_and(|state| state.mode.is_some() && state.color.is_some())
        })
    })
    .await;
    until(|| {
        let state = function(&c.context.registry, "group-1", "main").snapshot();
        state.power() != Some(Power::Unknown)
            && state.lighting().is_some_and(|light| {
                light.brightness_percent.is_some() && light.temperature_kelvin.is_some()
            })
    })
    .await;
}

pub(in super::super) fn confirmed_for(
    c: &Control,
    did: &str,
    model: &str,
    instance: &Value,
) -> NativeDescription {
    let _ = c;
    NativeDescription::new(
        model.into(),
        instance["type"].as_str().unwrap().into(),
        crate::xiaomi::capability::parse_device_for_model(
            DeviceKey::new("xiaomi", did),
            model,
            instance,
        )
        .unwrap(),
    )
    .unwrap()
}
