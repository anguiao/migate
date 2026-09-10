use futures_lite::{future::block_on, io::Cursor};
use migate::{
    device::Command,
    terminal::{handle_line, run_input},
    virtual_device::VirtualLight,
};

#[test]
fn terminal_recovers_and_reads_shared_state() {
    let light = VirtualLight::new();
    assert_eq!(handle_line(&light, " \n"), None);
    assert_eq!(
        handle_line(&light, " on \n").unwrap(),
        "virtual-light-1: on"
    );
    for invalid in ["toggle", "ON", "on extra", "status extra", "off extra"] {
        assert!(
            handle_line(&light, invalid)
                .unwrap()
                .contains("on, off, status")
        );
        assert!(light.snapshot().power);
    }
    light.execute(Command::Off);
    assert_eq!(
        handle_line(&light, "status").unwrap(),
        "virtual-light-1: off"
    );
    assert_eq!(handle_line(&light, "on").unwrap(), "virtual-light-1: on");
    assert_eq!(handle_line(&light, "off").unwrap(), "virtual-light-1: off");
}

#[test]
fn input_recovers_from_unknown_commands_and_returns_at_eof() {
    let light = VirtualLight::new();
    let mut output = Vec::new();
    block_on(run_input(
        &light,
        Cursor::new(b" \non\nbad\nstatus\noff"),
        &mut output,
    ))
    .unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "virtual-light-1: on\n可用命令：on, off, status\nvirtual-light-1: on\nvirtual-light-1: off\n"
    );
    assert!(!light.snapshot().power);
}

#[test]
fn input_io_errors_propagate() {
    let light = VirtualLight::new();
    let mut output = Vec::new();
    let error = block_on(run_input(&light, Cursor::new(b"\xff\n"), &mut output)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn output_io_errors_propagate() {
    let light = VirtualLight::new();
    let mut output = Cursor::new(&mut [0u8; 0][..]);
    assert!(block_on(run_input(&light, Cursor::new(b"status\n"), &mut output)).is_err());
}
