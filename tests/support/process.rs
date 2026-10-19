//! Shared process I/O, signal, timeout and panic cleanup for CLI and application tests.
// Each test binary uses the subset appropriate to its boundary.
#![allow(dead_code)]
use std::{
    fs,
    io::Write,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;

pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(20);

pub(crate) enum Stream {
    Stdout,
    Stderr,
}

pub(crate) struct TestProcess {
    pub(crate) child: Child,
    stdout: NamedTempFile,
    stderr: NamedTempFile,
}

impl TestProcess {
    pub(crate) fn spawn(command: &mut Command) -> Self {
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

    pub(crate) fn send(&mut self, commands: &str) {
        let input = self.child.stdin.as_mut().unwrap();
        input.write_all(commands.as_bytes()).unwrap();
        input.flush().unwrap();
    }

    pub(crate) fn close_input(&mut self) {
        self.child.stdin.take();
    }

    pub(crate) fn output(&self) -> (String, String) {
        let read = |file: &NamedTempFile| {
            String::from_utf8_lossy(&fs::read(file.path()).unwrap()).into_owned()
        };
        (read(&self.stdout), read(&self.stderr))
    }

    pub(crate) fn wait_for_output(&mut self, stream: Stream, expected: &str) {
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

    pub(crate) fn interrupt(self) -> (String, String) {
        self.stop("-INT")
    }

    pub(crate) fn stop(self, signal: &str) -> (String, String) {
        let (status, stdout, stderr) = self.signal(signal);
        assert!(status.success(), "{stderr}");
        (stdout, stderr)
    }

    pub(crate) fn interrupt_failure(self) -> (String, String) {
        let (status, stdout, stderr) = self.signal("-INT");
        assert!(!status.success(), "auth command unexpectedly succeeded");
        (stdout, stderr)
    }

    fn signal(mut self, signal: &str) -> (std::process::ExitStatus, String, String) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "process exited before {signal}: {}",
            self.output().1
        );
        assert!(
            Command::new("kill")
                .args([signal, &self.child.id().to_string()])
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
                "{signal} did not exit within {TIMEOUT:?}: {}",
                self.output().1
            );
            thread::sleep(POLL_INTERVAL);
        };
        let (stdout, stderr) = self.output();
        (status, stdout, stderr)
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        // Cleanup must also run when an assertion panics or a wait times out.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
