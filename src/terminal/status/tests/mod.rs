use super::*;

use crate::terminal::bridge;
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
        futures_lite::future::block_on(bridge::handle_line(
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
    let help = futures_lite::future::block_on(bridge::handle_line(
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
            did: "one".into(),
            usable_control_paths: vec![PathKind::Local, PathKind::Cloud],
            selected_control: Some(PathKind::Cloud),
            notification_source: Some(PathKind::Local),
            unavailable_reason: None,
            sync_limited_reason: Some("cloud property subscriptions pending".into()),
            last_switch_reason: Some("cloud preferred after local timeout".into()),
            commands: vec![ChannelCommands {
                channel: "main".into(),
                inflight: Some(CommandStatus {
                    id: 42,
                    binding: PropertyBinding { siid: 2, piid: 1 },
                    target: true,
                    actual_path: Some(PathKind::Local),
                    result: None,
                }),
                last: Some(CommandStatus {
                    id: 41,
                    binding: PropertyBinding { siid: 2, piid: 1 },
                    target: false,
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
            did: "one".into(),
            usable_control_paths: Vec::new(),
            selected_control: None,
            notification_source: None,
            unavailable_reason: Some("token expired".into()),
            sync_limited_reason: Some("no notification source".into()),
            last_switch_reason: None,
            commands: vec![ChannelCommands {
                channel: "main".into(),
                inflight: Some(CommandStatus {
                    id: 8,
                    binding: PropertyBinding { siid: 3, piid: 1 },
                    target: false,
                    actual_path: None,
                    result: None,
                }),
                last: Some(CommandStatus {
                    id: 7,
                    binding: PropertyBinding { siid: 3, piid: 1 },
                    target: true,
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
