use super::super::*;
use crate::{
    device::{DeviceKey, FunctionKey, RelayMode},
    storage::Store,
    xiaomi::capability::tests::source::SourceFixture,
};
use std::collections::BTreeMap;

fn fixture(id: &str, single: bool, name: &str) -> SourceFixture {
    let (model, json) = if single {
        (
            "zimi.switch.dhkg01",
            include_str!("../../../tests/fixtures/miot/zimi-dhkg01.json"),
        )
    } else {
        (
            "zimi.switch.dhkg05",
            include_str!("../../../tests/fixtures/miot/zimi-dhkg05.json"),
        )
    };
    let mut fixture = SourceFixture::new(id, model, serde_json::from_str(json).unwrap());
    fixture.raw.name = Some(name.into());
    fixture
}

fn make_plan(
    current: &CommittedConfiguration,
    fixture: SourceFixture,
    home: &str,
    modes: &[(u32, RelayMode)],
    splits: Vec<SplitDevice>,
) -> Plan {
    let key = fixture.raw.key.clone();
    let facts = SourceFacts {
        scope: Some(Scope {
            uid: "account".into(),
            home_id: home.into(),
        }),
        devices: vec![SourceDevice {
            description: fixture.into(),
            fresh: true,
        }],
        relations: vec![],
        split_devices: splits,
        relay_modes: modes
            .iter()
            .map(|(siid, mode)| {
                (
                    FunctionKey::new(key.clone(), format!("relay:{siid}")),
                    *mode,
                )
            })
            .collect(),
    };
    let identities = if current.device(&key).is_none() {
        BTreeMap::from([(key, "0123456789abcdef0123456789abcdef".into())])
    } else {
        BTreeMap::new()
    };
    plan(
        PlanInput {
            current,
            current_derived: &current.derive(),
            facts,
            revocations: vec![],
            new_identities: identities,
        },
        |functions, battery| {
            crate::matter::fixed_shape_with_battery(functions.iter().copied(), battery)
        },
    )
    .unwrap()
}

#[test]
fn labels_identity_and_confirmed_modes_survive_storage_refresh_and_home_changes() {
    let name = "很长的客厅墙壁开关名称继续延长";
    let multi = fixture("panel", false, name);
    let splits = vec![SplitDevice {
        did: "split-left".into(),
        parent_did: "panel".into(),
        siid: 2,
        name: Some("Left light".into()),
    }];
    let first = make_plan(
        &CommittedConfiguration::empty(),
        multi.clone(),
        "home-a",
        &[],
        splits.clone(),
    );
    assert!(first.derived.published_relays.is_empty());
    assert!(
        !first
            .derived
            .published
            .contains(&DeviceKey::new("xiaomi", "panel"))
    );
    assert_eq!(first.configuration.relay_channels.len(), 3);
    let get = |configuration: &CommittedConfiguration, siid: u32| {
        configuration
            .relay_channels
            .iter()
            .find(|channel| {
                channel.scope.home_id == "home-a" && channel.key.id == format!("relay:{siid}")
            })
            .unwrap()
            .clone()
    };
    assert_eq!(get(&first.configuration, 2).label, "Left light");
    let middle = get(&first.configuration, 3);
    let right = get(&first.configuration, 4);
    assert!(middle.label.len() <= 32 && middle.label.ends_with(" (3)"));
    assert!(right.label.len() <= 32 && right.label.ends_with(" (4)"));
    assert_ne!(middle.label, right.label);
    let wired = [
        (2, RelayMode::Wired),
        (3, RelayMode::Wired),
        (4, RelayMode::Wired),
    ];
    let second = make_plan(
        &first.configuration,
        multi.clone(),
        "home-a",
        &wired,
        splits.clone(),
    );
    assert_eq!(second.derived.published_relays.len(), 3);
    assert!(second.rebuild);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .devices()
        .commit_configuration(&second.configuration, &[])
        .unwrap();
    let stored = store.devices().load_configuration().unwrap();
    assert_eq!(stored, second.configuration);
    let refresh = make_plan(
        &stored,
        fixture("panel", false, "New panel name"),
        "home-a",
        &[],
        vec![],
    );
    assert_eq!(refresh.derived.published_relays.len(), 3);
    assert_eq!(get(&refresh.configuration, 3), get(&stored, 3));
    let hidden = make_plan(
        &refresh.configuration,
        multi.clone(),
        "home-a",
        &[(3, RelayMode::Wireless)],
        splits.clone(),
    );
    assert_eq!(hidden.derived.published_relays.len(), 2);
    assert_eq!(get(&hidden.configuration, 3).endpoint, middle.endpoint);
    let all_hidden = make_plan(
        &hidden.configuration,
        multi.clone(),
        "home-a",
        &[(2, RelayMode::Wireless), (4, RelayMode::Wireless)],
        splits.clone(),
    );
    assert!(all_hidden.derived.published_relays.is_empty());
    assert!(all_hidden.derived.published.is_empty());
    let restored = make_plan(
        &all_hidden.configuration,
        multi.clone(),
        "home-a",
        &wired,
        splits.clone(),
    );
    assert_eq!(restored.derived.published_relays.len(), 3);
    assert_eq!(get(&restored.configuration, 3).unique_id, middle.unique_id);
    assert_eq!(get(&restored.configuration, 3).label, middle.label);
    let home_b = make_plan(
        &restored.configuration,
        multi.clone(),
        "home-b",
        &wired,
        splits.clone(),
    );
    let b = home_b
        .configuration
        .relay_channels
        .iter()
        .find(|channel| channel.scope.home_id == "home-b" && channel.key.id == "relay:3")
        .unwrap();
    assert_ne!(b.endpoint, middle.endpoint);
    assert_ne!(b.unique_id, middle.unique_id);
    store
        .devices()
        .commit_configuration(&home_b.configuration, &[])
        .unwrap();
    let loaded = store.devices().load_configuration().unwrap();
    let back = make_plan(&loaded, multi, "home-a", &[], splits);
    assert_eq!(get(&back.configuration, 3).endpoint, middle.endpoint);
    assert_eq!(get(&back.configuration, 3).unique_id, middle.unique_id);
}

#[test]
fn single_relay_uses_panel_name_even_when_a_split_name_exists() {
    let split = SplitDevice {
        did: "alias".into(),
        parent_did: "single".into(),
        siid: 2,
        name: Some("Other name".into()),
    };
    let plan = make_plan(
        &CommittedConfiguration::empty(),
        fixture("single", true, "Entrance"),
        "home-a",
        &[(2, RelayMode::Wired)],
        vec![split],
    );
    assert_eq!(plan.configuration.relay_channels[0].label, "Entrance");
    assert_eq!(plan.derived.published_relays.len(), 1);
}

#[test]
fn multi_key_panel_with_only_one_valid_relay_keeps_split_or_numbered_name() {
    let mut instance = fixture("panel", false, "Panel").raw.instance;
    for service in instance["services"].as_array_mut().unwrap() {
        if [3, 4].contains(&service["iid"].as_u64().unwrap()) {
            service["properties"][1]["access"] = serde_json::json!(["write", "notify"]);
        }
    }
    let mut multi = SourceFixture::new("panel", "zimi.switch.dhkg05", instance);
    multi.raw.name = Some("Panel".into());
    assert_eq!(multi.device().functions().len(), 1);
    assert_eq!(
        multi
            .device()
            .recognition()
            .diagnostics()
            .iter()
            .filter(|issue| issue.target().starts_with("relay:"))
            .count(),
        2
    );
    let split = SplitDevice {
        did: "left".into(),
        parent_did: "panel".into(),
        siid: 2,
        name: Some("Left load".into()),
    };
    let named = make_plan(
        &CommittedConfiguration::empty(),
        multi.clone(),
        "home-a",
        &[],
        vec![split],
    );
    assert_eq!(named.configuration.relay_channels[0].label, "Left load");
    let unnamed = make_plan(
        &CommittedConfiguration::empty(),
        multi,
        "home-a",
        &[],
        vec![],
    );
    assert_eq!(unnamed.configuration.relay_channels[0].label, "Panel (2)");
}

#[test]
fn storage_rejects_invalid_relay_identity_mode_and_missing_fields() {
    let plan = make_plan(
        &CommittedConfiguration::empty(),
        fixture("panel", false, "Panel"),
        "home-a",
        &[(2, RelayMode::Wired)],
        vec![],
    );
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store
        .devices()
        .commit_configuration(&plan.configuration, &[])
        .unwrap();
    let db = rusqlite::Connection::open(store.path()).unwrap();
    let raw: String = db
        .query_row(
            "SELECT json FROM bridge_configuration WHERE id=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let original: serde_json::Value = serde_json::from_str(&raw).unwrap();
    for corruption in 0..7 {
        let mut value = original.clone();
        match corruption {
            0 => {
                value.as_object_mut().unwrap().remove("relay_channels");
            }
            1 => {
                value["relay_channels"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("mode");
            }
            2 => value["relay_channels"][0]["mode"] = serde_json::json!("unsupported"),
            3 => value["relay_channels"][0]["unique_id"] = serde_json::json!("invalid"),
            4 => value["relay_channels"][0]["label"] = serde_json::json!("x".repeat(33)),
            5 => value["relay_channels"][0]["key"]["function_id"] = serde_json::json!("relay:99"),
            6 => {
                let duplicate = value["relay_channels"][0].clone();
                value["relay_channels"]
                    .as_array_mut()
                    .unwrap()
                    .push(duplicate);
            }
            _ => unreachable!(),
        }
        db.execute(
            "UPDATE bridge_configuration SET json=?1 WHERE id=1",
            [value.to_string()],
        )
        .unwrap();
        assert!(
            store.devices().load_configuration().is_err(),
            "corruption {corruption}"
        );
    }
}
