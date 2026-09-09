use futures_lite::{future::block_on, io::Cursor};
use migate::{terminal::run_input, virtual_device::VirtualLight};

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
