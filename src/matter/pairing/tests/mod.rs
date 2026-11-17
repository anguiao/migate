use super::*;
use crate::{matter::model::basic_info, storage::Store};

#[test]
fn pairing_event_keeps_codes_and_window_through_terminal_output() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let event = opened(&info, 900).unwrap();
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
