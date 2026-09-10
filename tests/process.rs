use std::{
    fs,
    io::Write,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;

const TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

enum Stream {
    Stdout,
    Stderr,
}

struct TestProcess {
    child: Child,
    stdout: NamedTempFile,
    stderr: NamedTempFile,
}

impl TestProcess {
    fn spawn(command: &mut Command) -> Self {
        let stdout = NamedTempFile::new().unwrap();
        let stderr = NamedTempFile::new().unwrap();
        let child = command
            .stdin(Stdio::piped())
            .stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn send(&mut self, commands: &str) {
        let input = self.child.stdin.as_mut().unwrap();
        input.write_all(commands.as_bytes()).unwrap();
        input.flush().unwrap();
    }

    fn close_input(&mut self) {
        self.child.stdin.take();
    }

    fn output(&self) -> (String, String) {
        let read = |file: &NamedTempFile| {
            String::from_utf8_lossy(&fs::read(file.path()).unwrap()).into_owned()
        };
        (read(&self.stdout), read(&self.stderr))
    }

    fn wait_for_output(&mut self, stream: Stream, expected: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let (stdout, stderr) = self.output();
            let status = self.child.try_wait().unwrap();
            assert!(
                status.is_none(),
                "unexpected early exit {status:?}: {stderr}"
            );
            let output = match stream {
                Stream::Stdout => &stdout,
                Stream::Stderr => &stderr,
            };
            if output.contains(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected:?}\nstdout: {stdout}\nstderr: {stderr}"
            );
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn interrupt(mut self) -> (String, String) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "process exited before Ctrl-C: {}",
            self.output().1
        );
        assert!(
            Command::new("kill")
                .args(["-INT", &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "Ctrl-C did not exit within {TIMEOUT:?}: {}",
                self.output().1
            );
            thread::sleep(POLL_INTERVAL);
        };
        let (stdout, stderr) = self.output();
        assert!(status.success(), "{stderr}");
        (stdout, stderr)
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        // Cleanup must also run when an assertion panics or a wait times out.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

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
        let mut process = TestProcess::spawn(
            Command::new(env!("CARGO_BIN_EXE_migate"))
                .arg("--data-dir")
                .arg(directory.path()),
        );
        process.send("status\non\n");
        if close_input {
            process.close_input();
        }
        process.wait_for_output(Stream::Stdout, "virtual-light-1: on\n");
        if close_input {
            process.wait_for_output(Stream::Stderr, "终端输入结束，桥接服务继续运行");
        }
        let (output, errors) = process.interrupt();
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

#[test]
fn test_process_is_reaped_when_an_assertion_panics() {
    let process = TestProcess::spawn(Command::new("sleep").arg("30"));
    let pid = process.child.id();
    let result = std::panic::catch_unwind(move || {
        let _process = process;
        panic!("simulated test failure");
    });
    assert!(result.is_err());
    let probe = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .unwrap();
    assert!(!probe.status.success(), "child survived test failure");
}
