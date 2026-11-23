use super::*;
use crate::{matter::model::basic_info, storage::Store};
use rs_matter::{
    dm::devices::test::{TEST_DEV_ATT, TEST_DEV_COMM},
    utils::timers::Duration,
};

mod live;

#[test]
fn pairing_event_keeps_codes_and_window_through_terminal_output() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, 0);
    let event = opened(&matter, 900).unwrap();
    let PairingEvent::Opened {
        window_seconds,
        ref qr_payload,
        ref manual_code,
    } = event
    else {
        panic!("expected pairing information");
    };
    assert_eq!(window_seconds, 900);
    let mut buf = [0; 1024];
    let qr = QrPayload::parse(qr_payload, &mut buf).unwrap();
    let manual = QrPayload::parse_pairing_code(manual_code).unwrap();
    assert_eq!(qr.passcode(), manual.passcode());
    let expected_codes = format!("Manual pairing code: {manual_code}\n{qr_payload}\n");
    let mut output = Vec::new();
    crate::terminal::pairing::write_event(&mut output, event).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Add MiGate in the Home app."), "{output}");
    assert!(output.contains("open for 15 minutes."), "{output}");
    assert!(output.contains(&expected_codes), "{output}");
    assert!(output.contains("\x1b[97;40m"), "{output}");
    assert!(output.ends_with("\x1b[0m"), "{output}");
}

#[test]
fn expired_event_points_to_the_online_command() {
    let mut output = Vec::new();
    crate::terminal::pairing::write_event(&mut output, PairingEvent::Expired).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Pairing window expired."), "{output}");
    assert!(output.contains("migate pairing open"), "{output}");
}

#[test]
fn status_output_uses_the_current_deadline_and_only_shows_codes_while_open() {
    use crate::terminal::pairing::write_status;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, 0);
    let (manual_code, qr_payload) = codes(&matter).unwrap();
    let expected_codes = format!("Manual pairing code: {manual_code}\n{qr_payload}\n");
    let mut output = Vec::new();
    write_status(
        &mut output,
        PairingStatus::Open {
            expires_at: Instant::now() + Duration::from_secs(65),
            manual_code,
            qr_payload,
        },
    )
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    let first_line = output.lines().next().unwrap();
    assert!(
        first_line.contains("The pairing window has 1m "),
        "{output}"
    );
    assert!(first_line.ends_with("s remaining."), "{output}");
    assert!(output.contains(&expected_codes), "{output}");

    for (status, description) in [
        (PairingStatus::Paired, "MiGate is already paired."),
        (PairingStatus::Closed, "Pairing window is closed."),
        (
            PairingStatus::OpenWithAdministratorCode {
                expires_at: Instant::now() + Duration::from_secs(65),
            },
            "Use the code from the Matter administrator.",
        ),
    ] {
        let mut output = Vec::new();
        write_status(&mut output, status).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(description), "{output}");
        assert!(!output.contains("Manual pairing code"), "{output}");
        assert!(!output.contains("MT:"), "{output}");
    }
}
