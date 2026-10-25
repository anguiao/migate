//! Parent and function endpoint topology for sensor combinations.

use super::*;

#[test]
fn a_single_measurement_keeps_a_physical_parent_and_battery() {
    for kind in [
        FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
        FunctionKind::Humidity(MeasurementRange::new(0.0, 100.0).unwrap()),
    ] {
        let (device, _) = PhysicalDevice::new(
            PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", "single"),
                "Single",
                vec![FunctionDescription {
                    id: "measurement".into(),
                    label: "Measurement".into(),
                    kind,
                }],
            )
            .unwrap()
            .with_battery(),
        );
        let handler = model_handler(vec![entry(&device, 2, &[("measurement", 3)])]);
        handler.access(|node| {
            let parent = node
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == 2)
                .unwrap();
            assert!(parent.clusters.iter().any(|cluster| cluster.id == 0x2f));
            let child = node
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == 3)
                .unwrap();
            assert_eq!(child.device_types.len(), 1);
            assert!(
                child
                    .clusters
                    .iter()
                    .any(|cluster| cluster.id == 0x402 || cluster.id == 0x405)
            );
        });
    }
}

#[test]
fn mixed_light_children_publish_their_real_light_profiles() {
    use crate::device::{LightProfile, TemperatureInterval};
    let interval = TemperatureInterval::new(2700, 6500).unwrap();
    for (profile, device_type, expected_clusters) in [
        (LightProfile::OnOff, 0x0100, vec![6]),
        (LightProfile::Dimmable, 0x0101, vec![6, 8]),
        (
            LightProfile::ColorTemperature(interval),
            0x010c,
            vec![6, 8, 0x300],
        ),
        (
            LightProfile::ExtendedColor(interval),
            0x010d,
            vec![6, 8, 0x300],
        ),
    ] {
        let (device, _) = PhysicalDevice::new(
            PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", "mixed"),
                "Mixed",
                vec![
                    FunctionDescription {
                        id: "main".into(),
                        label: "Light".into(),
                        kind: FunctionKind::Light(profile),
                    },
                    FunctionDescription {
                        id: "temperature".into(),
                        label: "Temperature".into(),
                        kind: FunctionKind::Temperature(
                            MeasurementRange::new(-30.0, 100.0).unwrap(),
                        ),
                    },
                ],
            )
            .unwrap(),
        );
        let handler = model_handler(vec![entry(&device, 2, &[("main", 3), ("temperature", 4)])]);
        handler.access(|node| {
            let light = node
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == 3)
                .unwrap();
            assert_eq!(
                light
                    .device_types
                    .iter()
                    .map(|kind| kind.dtype)
                    .collect::<Vec<_>>(),
                [device_type]
            );
            for cluster in [6, 8, 0x300] {
                assert_eq!(
                    light.clusters.iter().any(|metadata| metadata.id == cluster),
                    expected_clusters.contains(&cluster)
                );
            }
            assert!(!light.clusters.iter().any(|cluster| cluster.id == 0x39));
            assert!(light.clusters.iter().any(|cluster| cluster.id == 0x1d));
            assert!(light.clusters.iter().any(|cluster| cluster.id == 3));
        });
    }
}

#[test]
fn sensor_publishes_parent_and_two_measurement_child_endpoints() {
    setup!(device, handler, _im);
    handler.access(|node| {
        let parent = node
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == 2)
            .unwrap();
        assert_eq!(
            parent
                .device_types
                .iter()
                .map(|kind| (kind.dtype, kind.drev))
                .collect::<Vec<_>>(),
            [(0x0013, 1)]
        );
        let parent_clusters = parent
            .clusters
            .iter()
            .map(|cluster| cluster.id)
            .collect::<Vec<_>>();
        assert!(parent_clusters.contains(&0x1d));
        assert!(parent_clusters.contains(&0x39));
        assert!(!parent_clusters.contains(&0x402));
        assert!(!parent_clusters.contains(&0x405));

        for (endpoint_id, device_type, device_revision, cluster_id, revision) in
            [(3, 0x0302, 3, 0x0402, 4), (4, 0x0307, 2, 0x0405, 3)]
        {
            let endpoint = node
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == endpoint_id)
                .unwrap();
            assert_eq!(
                endpoint
                    .device_types
                    .iter()
                    .map(|kind| (kind.dtype, kind.drev))
                    .collect::<Vec<_>>(),
                [(device_type, device_revision)]
            );
            let cluster = endpoint
                .clusters
                .iter()
                .find(|cluster| cluster.id == cluster_id)
                .unwrap();
            assert_eq!(cluster.revision, revision);
            assert_eq!(cluster.feature_map, 0);
            for attribute in [0, 1, 2, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd] {
                assert!(cluster.attribute(attribute).is_some(), "{attribute}");
            }
            assert!(cluster.attribute(3).is_none(), "no fake Tolerance");
            assert!(cluster.command(0).is_none());
            assert!(endpoint.clusters.iter().any(|cluster| cluster.id == 3));
            assert!(endpoint.clusters.iter().any(|cluster| cluster.id == 0x1d));
            assert!(!endpoint.clusters.iter().any(|cluster| cluster.id == 6));
            assert!(
                !endpoint
                    .clusters
                    .iter()
                    .any(|cluster| matches!(cluster.id, 8 | 0x300 | 4))
            );
        }
    });
}

#[test]
fn all_fifteen_measurement_combinations_publish_matching_parent_child_descriptors() {
    for mask in 1..16 {
        let functions = [
            (
                "temperature",
                FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
                0x0302,
                3,
                0x0402,
            ),
            (
                "humidity",
                FunctionKind::Humidity(MeasurementRange::new(0.0, 100.0).unwrap()),
                0x0307,
                2,
                0x0405,
            ),
            (
                "co2",
                FunctionKind::CarbonDioxide(MeasurementRange::new(400.0, 5000.0).unwrap()),
                0x002c,
                1,
                0x040d,
            ),
            (
                "illuminance",
                FunctionKind::Illuminance(MeasurementRange::new(0.0, 10000.0).unwrap()),
                0x0106,
                3,
                0x0400,
            ),
        ]
        .into_iter()
        .enumerate()
        .filter_map(|(index, item)| (mask & (1 << index) != 0).then_some(item))
        .collect::<Vec<_>>();
        let description = PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "combined"),
            "Combined",
            functions
                .iter()
                .map(|(id, kind, _, _, _)| FunctionDescription {
                    id: (*id).into(),
                    label: (*id).into(),
                    kind: *kind,
                })
                .collect(),
        )
        .unwrap()
        .with_battery();
        setup!(@device device, handler, im, PhysicalDevice::new(description).0);
        handler.access(|node| {
            assert_eq!(
                node.endpoints
                    .iter()
                    .filter(|endpoint| endpoint.id >= 2)
                    .count(),
                functions.len() + 1
            );
            for (index, (_, _, dtype, revision, cluster)) in functions.iter().enumerate() {
                let endpoint = node
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.id == index as u16 + 3)
                    .unwrap();
                assert_eq!(
                    endpoint
                        .device_types
                        .iter()
                        .map(|kind| (kind.dtype, kind.drev))
                        .collect::<Vec<_>>(),
                    [(*dtype, *revision)]
                );
                assert!(
                    endpoint
                        .clusters
                        .iter()
                        .any(|metadata| metadata.id == *cluster)
                );
                assert!(
                    !endpoint
                        .clusters
                        .iter()
                        .any(|metadata| metadata.id == 0x39 || metadata.id == 0x2f)
                );
                let identify = endpoint
                    .clusters
                    .iter()
                    .find(|metadata| metadata.id == 3)
                    .unwrap();
                assert!(identify.commands.iter().any(|command| command.id == 0));
                assert_eq!(endpoint.semantic_tags.len(), 1);
            }
        });
        block_on(async {
            let expected = (3..3 + functions.len() as u16).collect::<Vec<_>>();
            let parts = Context::at(&im, 2, 0x1d, 3);
            assert_eq!(
                report_value(&read_bytes(&handler, &parts).await.unwrap())
                    .array()
                    .unwrap()
                    .iter()
                    .map(|value| value.unwrap().u16().unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
            let power = Context::at(&im, 2, 0x2f, 31);
            assert_eq!(
                report_value(&read_bytes(&handler, &power).await.unwrap())
                    .array()
                    .unwrap()
                    .iter()
                    .map(|value| value.unwrap().u16().unwrap())
                    .collect::<Vec<_>>(),
                (2..3 + functions.len() as u16).collect::<Vec<_>>()
            );
        });
    }
}

#[test]
fn a_member_with_hidden_control_lists_only_visible_measurement_endpoints_for_shared_battery() {
    let (device, _) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "member"),
            "Member",
            vec![
                FunctionDescription {
                    id: "main".into(),
                    label: "Light".into(),
                    kind: FunctionKind::Light(crate::device::LightProfile::OnOff),
                },
                FunctionDescription {
                    id: "illuminance".into(),
                    label: "Illuminance".into(),
                    kind: FunctionKind::Illuminance(MeasurementRange::new(0.0, 10000.0).unwrap()),
                },
            ],
        )
        .unwrap()
        .with_battery(),
    );
    setup!(_baseline, _model, im);
    let handler = model_handler(vec![entry(&device, 2, &[("illuminance", 4)])]);
    handler.access(|node| {
        assert!(node.endpoints.iter().any(|endpoint| endpoint.id == 2));
        assert!(node.endpoints.iter().any(|endpoint| endpoint.id == 4));
        assert!(!node.endpoints.iter().any(|endpoint| endpoint.id == 3));
    });
    block_on(async {
        let context = Context::at(&im, 2, 0x2f, 31);
        assert_eq!(
            report_value(&read_bytes(&handler, &context).await.unwrap())
                .array()
                .unwrap()
                .iter()
                .map(|value| value.unwrap().u16().unwrap())
                .collect::<Vec<_>>(),
            [2, 4]
        );
        let parts = Context::at(&im, 2, 0x1d, 3);
        assert_eq!(
            report_value(&read_bytes(&handler, &parts).await.unwrap())
                .array()
                .unwrap()
                .iter()
                .map(|value| value.unwrap().u16().unwrap())
                .collect::<Vec<_>>(),
            [4]
        );
    });
}
