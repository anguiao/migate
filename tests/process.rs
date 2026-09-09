use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn invalid_arguments_exit_without_starting_services() {
    let result = Command::new(env!("CARGO_BIN_EXE_migate"))
        .arg("--unknown")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("用法："));
    assert!(result.stdout.is_empty());
}

#[test]
fn explicit_directory_eof_and_interrupt_keep_identity_and_reset_power() {
    let directory = tempfile::tempdir().unwrap();
    let mut identity = None;
    for close_input in [false, true] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_migate"))
            .args(["--data-dir", directory.path().to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let read = |mut pipe: Box<dyn Read + Send>| {
            thread::spawn(move || {
                let mut text = String::new();
                pipe.read_to_string(&mut text).unwrap();
                text
            })
        };
        let output = read(Box::new(stdout));
        let errors = read(Box::new(stderr));
        let mut input = child.stdin.take().unwrap();
        input.write_all(b"status\non\n").unwrap();
        input.flush().unwrap();
        if close_input {
            drop(input);
        }
        thread::sleep(Duration::from_secs(2));
        if let Some(status) = child.try_wait().unwrap() {
            panic!("unexpected early exit {status}: {}", errors.join().unwrap());
        }
        assert!(
            Command::new("kill")
                .args(["-INT", &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("Ctrl-C did not exit within 5 seconds");
            }
            thread::sleep(Duration::from_millis(20));
        };
        let errors = errors.join().unwrap();
        assert!(status.success(), "{errors}");
        let output = output.join().unwrap();
        assert!(output.contains("virtual-light-1: off"), "{output}");
        assert!(output.contains("virtual-light-1: on"), "{output}");
        assert!(errors.contains(directory.path().to_str().unwrap()));
        let restored = migate::storage::Store::open(directory.path()).unwrap();
        if let Some(identity) = &identity {
            assert_eq!(identity, restored.identity());
        } else {
            identity = Some(restored.identity().clone());
        }
    }
}
