mod logging;

use super::*;

use crate::terminal::bridge::tests::support;
use crate::xiaomi::auth::AuthReport;

#[test]
fn sign_in_required_preserves_the_login_hint() {
    let report = AuthReport::for_test(
        AuthenticationState::SignInRequired(
            crate::xiaomi::auth::FailureReason::AccountIdentityMismatch,
        ),
        None,
        CertificateUpdate::NotNeeded,
        0,
    );
    assert!(
        format_report(&report, 0, Path::new("/bin/migate"), Path::new("/data"))
            .contains("auth login")
    );
}

#[test]
fn connected_gateway_keeps_authentication_and_catalog_diagnostics_visible() {
    use crate::xiaomi::adapter::{AdapterStatus, GatewayStatus};
    let status = AuthStatus::new(
        AuthReport::for_test(
            AuthenticationState::Checking,
            None,
            CertificateUpdate::NotNeeded,
            0,
        ),
        "/bin/migate".into(),
        "/data".into(),
    );
    status.update_adapter(AdapterStatus {
        authentication: AuthReport::for_test(
            AuthenticationState::SignInRequired(
                crate::xiaomi::auth::FailureReason::AccountIdentityMismatch,
            ),
            None,
            CertificateUpdate::NotNeeded,
            0,
        ),
        gateway: GatewayStatus::Connected("gateway".into()),
        catalog_problem: Some("refresh failed".into()),
        pending: Vec::new(),
        routing: Default::default(),
        devices: Vec::new(),
    });
    let report = status.render(0);
    assert!(report.contains("Xiaomi: sign-in required"));
    assert!(report.contains("auth login"));
    assert!(report.contains("Gateway: connected (gateway)."));
    assert!(report.contains("Device description: refresh failed."));
}

#[test]
fn certificate_dates_are_recomputed_for_each_render() {
    let report = AuthReport::for_test(
        AuthenticationState::Authenticated,
        Some(CertificateValidity {
            not_before: 1_700_000_000,
            not_after: 1_700_259_201,
        }),
        CertificateUpdate::NotNeeded,
        1_700_000_000,
    );
    assert_eq!(
        format_report(
            &report,
            1_700_000_000,
            Path::new("/bin/migate"),
            Path::new("/data")
        ),
        "Xiaomi: authenticated (cn).\nGateway certificate: valid until 2023-11-17T22:13:21Z."
    );
    assert_eq!(
        format_report(
            &report,
            1_700_000_001,
            Path::new("/bin/migate"),
            Path::new("/data")
        ),
        "Xiaomi: authenticated (cn).\nGateway certificate: valid until 2023-11-17T22:13:21Z; renewal due."
    );
}

#[test]
fn bridge_help_reads_the_latest_report() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let registry = crate::device::DeviceRegistry::new();
    let checking = AuthReport::for_test(
        AuthenticationState::Checking,
        None,
        CertificateUpdate::NotNeeded,
        1_700_000_000,
    );
    let status = AuthStatus::new(checking, "/bin/migate".into(), "/data".into());
    assert!(
        futures_lite::future::block_on(support::handle_line(
            &registry,
            &store.devices(),
            &status,
            "help",
            1_700_000_000
        ))
        .unwrap()
        .unwrap()
        .contains("Xiaomi: checking authentication (cn)...")
    );

    let authenticated = AuthReport::for_test(
        AuthenticationState::Authenticated,
        Some(CertificateValidity {
            not_before: 1_699_000_000,
            not_after: 1_800_000_000,
        }),
        CertificateUpdate::NotNeeded,
        1_700_000_000,
    );
    assert!(status.replace(authenticated));
    let help = futures_lite::future::block_on(support::handle_line(
        &registry,
        &store.devices(),
        &status,
        "help",
        1_700_000_000,
    ))
    .unwrap()
    .unwrap();
    assert!(help.contains("Xiaomi: authenticated (cn)."), "{help}");
    assert!(!help.contains("checking authentication"), "{help}");

    let same_visible_status = AuthReport::for_test(
        AuthenticationState::Authenticated,
        Some(CertificateValidity {
            not_before: 1_699_000_000,
            not_after: 1_800_000_000,
        }),
        CertificateUpdate::NotNeeded,
        1_700_000_001,
    );
    assert!(!status.replace(same_visible_status));
}

#[test]
fn routing_status_distinguishes_selected_control_from_the_actual_command_path() {
    use crate::{
        device::CommandError,
        xiaomi::{adapter::*, capability::PropertyBinding},
    };
    let report = AuthReport::for_test(
        AuthenticationState::Authenticated,
        None,
        CertificateUpdate::NotNeeded,
        0,
    );
    let status = AuthStatus::new(report.clone(), "/bin/migate".into(), "/data".into());
    status.update_adapter(AdapterStatus {
        authentication: report,
        gateway: GatewayStatus::Connected("gateway".into()),
        catalog_problem: None,
        pending: Vec::new(),
        routing: RoutingStatus {
            priority: PathPriority::CloudFirst,
            local_connected: true,
            business_degraded: true,
            recovery: RecoveryStatus::Scheduled(1_700_000_060),
            last_switch_reason: Some("local business response timed out".into()),
        },
        devices: vec![DeviceStatus {
            functions: vec![],
            did: "one".into(),
            read_only: false,
            usable_control_paths: vec![PathKind::Local, PathKind::Cloud],
            selected_control: Some(PathKind::Cloud),
            usable_read_paths: Vec::new(),
            selected_read: None,
            physical_online: None,
            notification_source: Some(PathKind::Local),
            unavailable_reason: None,
            sync_limited_reason: Some("cloud property subscriptions pending".into()),
            battery_problem: None,
            last_switch_reason: Some("cloud preferred after local timeout".into()),
            commands: vec![ChannelCommands {
                light_actions: None,
                channel: "main".into(),
                inflight: Some(CommandStatus {
                    id: 42,
                    binding: PropertyBinding { siid: 2, piid: 1 },
                    target: crate::xiaomi::adapter::CommandTarget::Power(true),
                    properties: vec![(PropertyBinding { siid: 2, piid: 1 }, true.into())],
                    fan_paths: Vec::new(),
                    actual_path: Some(PathKind::Local),
                    result: None,
                }),
                last: Some(CommandStatus {
                    id: 41,
                    binding: PropertyBinding { siid: 2, piid: 1 },
                    target: crate::xiaomi::adapter::CommandTarget::Power(false),
                    properties: vec![(PropertyBinding { siid: 2, piid: 1 }, false.into())],
                    fan_paths: Vec::new(),
                    actual_path: Some(PathKind::Cloud),
                    result: Some(Err(CommandError::Timeout)),
                }),
            }],
        }],
    });
    let output = status.render_devices(1_700_000_000, None);
    assert!(
        output.contains("Routing: cloud-first; local connected; business degraded."),
        "{output}"
    );
    assert!(
        output.contains("Local recovery: scheduled for 2023-11-14T22:14:20Z."),
        "{output}"
    );
    assert!(
        output.contains("Usable control paths: local, cloud."),
        "{output}"
    );
    assert!(output.contains("Selected control: cloud."), "{output}");
    assert!(output.contains("Notification source: local."), "{output}");
    assert!(output.contains("main in-flight command #42: target on; property 2.1; actual path local; awaiting result."), "{output}");
    assert!(output.contains("main last command #41: target off; property 2.1; actual path cloud; command acceptance timed out."), "{output}");
    assert!(
        output.contains("State synchronization limited: cloud property subscriptions pending."),
        "{output}"
    );
    assert!(
        output.contains("Device last switch: cloud preferred after local timeout."),
        "{output}"
    );
    let help = status.render(1_700_000_000);
    assert!(help.contains("Routing: cloud-first"));
    assert!(!help.contains("command #"));
    assert!(!help.contains("one routing:"));
}

#[test]
fn routing_status_displays_running_recovery_unsent_work_and_previous_acceptance() {
    use crate::xiaomi::{adapter::*, capability::PropertyBinding};
    let report = AuthReport::for_test(
        AuthenticationState::Authenticated,
        None,
        CertificateUpdate::NotNeeded,
        0,
    );
    let status = AuthStatus::new(report.clone(), "/bin/migate".into(), "/data".into());
    status.update_adapter(AdapterStatus {
        authentication: report,
        gateway: GatewayStatus::Unavailable("transport disconnected".into()),
        catalog_problem: None,
        pending: Vec::new(),
        routing: RoutingStatus {
            recovery: RecoveryStatus::Running,
            ..Default::default()
        },
        devices: vec![DeviceStatus {
            functions: vec![],
            did: "one".into(),
            read_only: false,
            usable_control_paths: Vec::new(),
            selected_control: None,
            usable_read_paths: Vec::new(),
            selected_read: None,
            physical_online: None,
            notification_source: None,
            unavailable_reason: Some("token expired".into()),
            sync_limited_reason: Some("no notification source".into()),
            battery_problem: None,
            last_switch_reason: None,
            commands: vec![ChannelCommands {
                light_actions: None,
                channel: "main".into(),
                inflight: Some(CommandStatus {
                    id: 8,
                    binding: PropertyBinding { siid: 3, piid: 1 },
                    target: crate::xiaomi::adapter::CommandTarget::Power(false),
                    properties: vec![(PropertyBinding { siid: 3, piid: 1 }, false.into())],
                    fan_paths: Vec::new(),
                    actual_path: None,
                    result: None,
                }),
                last: Some(CommandStatus {
                    id: 7,
                    binding: PropertyBinding { siid: 3, piid: 1 },
                    target: crate::xiaomi::adapter::CommandTarget::Power(true),
                    properties: vec![(PropertyBinding { siid: 3, piid: 1 }, true.into())],
                    fan_paths: Vec::new(),
                    actual_path: Some(PathKind::Local),
                    result: Some(Ok(())),
                }),
            }],
        }],
    });
    let output = status.render_devices(0, Some("one"));
    assert!(output.contains("Local recovery: running."), "{output}");
    assert!(output.contains("Usable control paths: none."), "{output}");
    assert!(output.contains("Selected control: none."), "{output}");
    assert!(output.contains("Notification source: none."), "{output}");
    assert!(
        output.contains("Control unavailable: token expired."),
        "{output}"
    );
    assert!(output.contains("main in-flight command #8: target off; property 3.1; actual path not sent; awaiting result."), "{output}");
    assert!(
        output.contains(
            "main last command #7: target on; property 3.1; actual path local; accepted."
        ),
        "{output}"
    );
}

#[test]
fn lighting_command_records_show_explicit_targets_actual_batch_and_no_write_results() {
    use crate::{
        device::{CommandError, LightTarget, Rgb},
        xiaomi::{adapter::CommandTarget, capability::PropertyBinding},
    };

    let mut command = CommandStatus {
        id: 42,
        binding: PropertyBinding { siid: 99, piid: 9 },
        target: CommandTarget::Light(
            LightTarget::temperature(Some(false), 4000.0).expect("valid test target"),
        ),
        properties: vec![
            (PropertyBinding { siid: 2, piid: 1 }, false.into()),
            (PropertyBinding { siid: 2, piid: 2 }, 13.into()),
            (PropertyBinding { siid: 2, piid: 3 }, 4000.into()),
            (PropertyBinding { siid: 4, piid: 2 }, 0.into()),
        ],
        fan_paths: Vec::new(),
        actual_path: Some(PathKind::Local),
        result: Some(Ok(())),
    };
    assert_eq!(
        format_command("main", "last", &command),
        "main last command #42: target power=off, temperature=4000 K; properties 2.1=false, 2.2=13, 2.3=4000, 4.2=0; actual path local; accepted."
    );
    command.target = CommandTarget::Light(LightTarget::rgb(
        None,
        Rgb {
            red: 255,
            green: 128,
            blue: 64,
        },
    ));
    command.properties = vec![
        (PropertyBinding { siid: 4, piid: 1 }, 0xff8040.into()),
        (PropertyBinding { siid: 4, piid: 2 }, 1.into()),
    ];
    command.actual_path = Some(PathKind::Cloud);
    command.result = None;
    assert_eq!(
        format_command("main", "in-flight", &command),
        "main in-flight command #42: target RGB=#FF8040; properties 4.1=16744512, 4.2=1; actual path cloud; awaiting result."
    );
    command.properties.clear();
    command.actual_path = None;
    command.result = Some(Ok(()));
    let no_write = format_command("main", "last", &command);
    assert_eq!(
        no_write,
        "main last command #42: target RGB=#FF8040; properties none; actual path not sent; unchanged (no write)."
    );
    assert!(!no_write.contains("accepted"));
    assert!(!no_write.contains("99.9"));
    command.target = CommandTarget::Power(true);
    assert_eq!(
        format_command("main", "last", &command),
        "main last command #42: target on; property none; actual path not sent; unchanged (no write)."
    );
    command.target =
        CommandTarget::Light(LightTarget::temperature(None, 4000.0).expect("valid test target"));
    for (error, expected) in [
        (CommandError::Superseded, "superseded"),
        (CommandError::Stopped, "stopped"),
        (
            CommandError::Timeout,
            "failed: command acceptance timed out",
        ),
    ] {
        command.result = Some(Err(error));
        assert!(format_command("main", "last", &command).ends_with(&format!("{expected}.")));
    }
}

fn light_diagnostics_fixture() -> (crate::device::Function, crate::device::CommandReceiver) {
    use crate::device::*;
    let (physical, mut receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "light"),
            "Light",
            vec![FunctionDescription {
                id: "main".into(),
                label: "Light".into(),
                kind: FunctionKind::Light(LightProfile::ColorTemperature(
                    TemperatureInterval::new(2700, 6500).unwrap(),
                )),
            }],
        )
        .unwrap(),
    );
    physical.set_availability(Availability::Available);
    (physical.functions()[0].clone(), receivers.remove(0))
}

fn action_report(function: &crate::device::Function) -> String {
    format_device(&DeviceStatus {
        functions: vec![],
        did: "light".into(),
        read_only: false,
        usable_control_paths: vec![PathKind::Cloud],
        selected_control: Some(PathKind::Cloud),
        usable_read_paths: Vec::new(),
        selected_read: None,
        physical_online: None,
        notification_source: Some(PathKind::Local),
        unavailable_reason: None,
        sync_limited_reason: None,
        battery_problem: None,
        last_switch_reason: None,
        commands: vec![ChannelCommands {
            channel: "main".into(),
            light_actions: function.light_action_diagnostics(),
            inflight: None,
            last: None,
        }],
    })
}

#[test]
fn lighting_actions_show_undequeued_superseded_and_stopped_independently_of_frames() {
    use crate::device::{LightDimension, LightTarget};

    let (function, _receiver) = light_diagnostics_fixture();
    function
        .begin_light_action(
            LightTarget::brightness(None, 25.0).expect("valid test target"),
            false,
        )
        .unwrap();
    function
        .begin_light_action(
            LightTarget::brightness(None, 50.0).expect("valid test target"),
            false,
        )
        .unwrap();
    let superseded = action_report(&function);
    assert!(superseded.contains("main actions: brightness active; color idle; awaiting startup receipt none; last brightness superseded."), "{superseded}");
    assert!(!superseded.contains("command #"));
    assert!(!superseded.contains("ActionToken"));
    function
        .begin_light_action(
            LightTarget::temperature(None, 4000.0).expect("valid test target"),
            false,
        )
        .unwrap();
    function.stop_light(LightDimension::Brightness).unwrap();
    let stopped = action_report(&function);
    assert!(stopped.contains("main actions: brightness idle; color active; awaiting startup receipt none; last brightness stopped."), "{stopped}");
}

#[test]
fn lighting_actions_distinguish_original_startup_receipt_acceptance_and_failure() {
    use crate::device::{CommandError, DeviceRequest, LightDimension, LightTarget};
    use futures_lite::future::block_on;

    block_on(async {
        let (function, mut receiver) = light_diagnostics_fixture();
        let target = LightTarget::brightness(None, 25.0).expect("valid test target");
        function.begin_light_action(target, false).unwrap();
        let DeviceRequest::Light(original) = receiver.next_operation().await.unwrap() else {
            panic!("expected startup frame")
        };
        original.mark_sent().unwrap();
        assert!(
            action_report(&function).contains("awaiting startup receipt brightness; last none.")
        );
        function.stop_light(LightDimension::Brightness).unwrap();
        function.begin_light_action(target, false).unwrap();
        let waiting = action_report(&function);
        assert!(waiting.contains("brightness active; color idle; awaiting startup receipt brightness (previous action); last brightness stopped."), "{waiting}");
        original.accept().unwrap();
        assert!(
            action_report(&function)
                .contains("awaiting startup receipt none; last brightness stopped.")
        );
        let DeviceRequest::Light(next) = receiver.next_operation().await.unwrap() else {
            panic!("expected replacement frame")
        };
        next.mark_sent().unwrap();
        next.accept().unwrap();
        let accepted = action_report(&function);
        assert!(accepted.contains("brightness active; color idle; awaiting startup receipt none; last brightness accepted."), "{accepted}");
        assert!(!accepted.contains("finished"));
        function.begin_light_action(target, false).unwrap();
        let DeviceRequest::Light(failed) = receiver.next_operation().await.unwrap() else {
            panic!("expected failing frame")
        };
        failed.reject(CommandError::Rejected("denied".into()));
        let rejected = action_report(&function);
        assert!(rejected.contains("brightness idle; color idle; awaiting startup receipt none; last brightness failed: command rejected: denied."), "{rejected}");
    });
}

#[test]
fn unchanged_transition_startup_does_not_claim_acceptance() {
    use crate::device::{DeviceRequest, LightTarget};
    use futures_lite::future::{block_on, poll_once};

    block_on(async {
        let (function, mut receiver) = light_diagnostics_fixture();
        let action = function
            .begin_light_action(
                LightTarget::brightness(None, 50.0).expect("valid test target"),
                false,
            )
            .unwrap();
        let DeviceRequest::Light(frame) = receiver.next_operation().await.unwrap() else {
            panic!("expected repeated frame")
        };
        frame.accept_unchanged().unwrap();
        assert!(poll_once(action.accepted()).await.is_none());
        assert!(poll_once(action.ended()).await.is_none());
        let report = action_report(&function);
        assert!(
            report.contains(
                "brightness active; color idle; awaiting startup receipt none; last none."
            ),
            "{report}"
        );
        assert!(!report.contains("accepted"));
    });
}

#[test]
fn a_notification_limited_read_only_channel_is_reported_without_claiming_physical_online() {
    use crate::xiaomi::adapter::{ChannelCommands, DeviceStatus, PathKind};
    let report = super::format_device(&DeviceStatus {
        functions: vec![],
        did: "sensor".into(),
        read_only: true,
        usable_control_paths: Vec::new(),
        selected_control: None,
        usable_read_paths: vec![PathKind::Cloud],
        selected_read: Some(PathKind::Cloud),
        physical_online: None,
        notification_source: None,
        unavailable_reason: None,
        sync_limited_reason: Some("notifications disconnected or subscriptions not ready".into()),
        battery_problem: None,
        last_switch_reason: None,
        commands: Vec::<ChannelCommands>::new(),
    });
    assert!(report.contains("Usable read paths: cloud."), "{report}");
    assert!(report.contains("Selected read: cloud."), "{report}");
    assert!(report.contains("Physical online: unknown."), "{report}");
    assert!(report.contains("State synchronization limited"), "{report}");
    assert!(!report.contains("Control unavailable"), "{report}");
}

#[test]
fn battery_diagnostics_are_separate_from_physical_reachability() {
    let report = format_device(&DeviceStatus {
        functions: vec![],
        did: "sensor".into(),
        read_only: true,
        usable_control_paths: Vec::new(),
        selected_control: None,
        usable_read_paths: vec![PathKind::Local],
        selected_read: Some(PathKind::Local),
        physical_online: None,
        notification_source: Some(PathKind::Local),
        unavailable_reason: None,
        sync_limited_reason: None,
        battery_problem: Some("sensor subscribe 2.1003: denied".into()),
        last_switch_reason: None,
        commands: Vec::new(),
    });
    assert!(report.contains("Physical online: unknown."), "{report}");
    assert!(
        report.contains("Battery: sensor subscribe 2.1003: denied."),
        "{report}"
    );
    assert!(!report.contains("Read unavailable"), "{report}");
}
