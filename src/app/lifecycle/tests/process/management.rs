use super::{Stream, TestProcess, process_support::TIMEOUT};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
};

struct Session(BufReader<UnixStream>);

impl Session {
    fn connect(directory: &Path) -> Self {
        let stream = UnixStream::connect(directory.join("migate.sock")).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut reader = BufReader::new(stream);
        let mut greeting = String::new();
        reader.read_line(&mut greeting).unwrap();
        assert_eq!(greeting, "migate/1\n");
        Self(reader)
    }

    fn send(&mut self, words: &[&str]) {
        let writer = self.0.get_mut();
        serde_json::to_writer(&mut *writer, words).unwrap();
        writer.write_all(b"\n").unwrap();
    }

    fn result(&mut self) -> (bool, String) {
        let mut output = String::new();
        loop {
            let mut line = String::new();
            self.0.read_line(&mut line).unwrap();
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            if let Some(success) = event["done"].as_bool() {
                return (success, output);
            }
            output.push_str(event["output"].as_str().unwrap());
        }
    }
}

#[test]
fn management_query_and_accepted_control_have_independent_lifetimes() {
    let directory = tempfile::tempdir().unwrap();
    let mut bridge = TestProcess::start_gated(directory.path(), 0, false);
    let mut first = Session::connect(directory.path());
    let mut second = Session::connect(directory.path());
    first.send(&["on", "test-outlet", "outlet:2"]);
    let (ok, error) = first.result();
    assert!(!ok);
    assert!(error.contains("initializing"), "{error}");

    let hold = directory.path().join("source-hold-command");
    fs::write(&hold, b"hold").unwrap();
    fs::write(directory.path().join("source-ready"), b"ready").unwrap();
    bridge.wait_for_output(Stream::Stderr, "Fake source initial read: off");
    first.send(&["on", "test-outlet", "outlet:2"]);
    bridge.wait_for_output(Stream::Stderr, "Fake source control received: true");

    second.send(&["status", "test-outlet"]);
    let (ok, status) = second.result();
    assert!(ok);
    assert!(status.contains("outlet:2 (主电源): off"), "{status}");
    second.send(&["off", "test-outlet", "outlet:2"]);
    drop(second);

    // An incomplete frame does not constitute a submitted command.
    let mut incomplete = Session::connect(directory.path());
    incomplete
        .0
        .get_mut()
        .write_all(br#"["on","test-outlet","outlet:2"]"#)
        .unwrap();
    drop(incomplete);
    fs::remove_file(&hold).unwrap();
    assert_eq!(
        first.result(),
        (true, "test-outlet: command accepted\n".into())
    );
    bridge.wait_for_output(Stream::Stderr, "terminal test-outlet off: accepted");

    // A reconnect only establishes a new session, with no responses from the old one.
    let mut second = Session::connect(directory.path());
    second.send(&["status", "test-outlet"]);
    let (ok, status) = second.result();
    assert!(ok);
    assert!(status.contains("outlet:2 (主电源): off"), "{status}");
    assert!(!status.contains("command accepted"), "{status}");
    second.send(&["map-type", "test-outlet", "outlet:2", "unknown"]);
    assert_eq!(
        second.result(),
        (false, "Unknown mapping type; use switch or light.\n".into())
    );
    let (_, log) = bridge.interrupt();
    assert_eq!(
        log.matches("Fake source control received: true").count(),
        1,
        "{log}"
    );
    assert_eq!(
        log.matches("Fake source control received: false").count(),
        1,
        "{log}"
    );
}

#[test]
fn pairing_on_the_same_management_socket_uses_the_current_matter_window() {
    let directory = tempfile::tempdir().unwrap();
    let mut bridge = TestProcess::start_gated(directory.path(), 0, false);
    let mut session = Session::connect(directory.path());
    for command in [&["pairing"][..], &["pairing", "open"][..]] {
        session.send(command);
        assert_eq!(
            session.result(),
            (false, "Waiting for initial Xiaomi device reads.\n".into())
        );
    }

    fs::write(directory.path().join("source-ready"), b"ready").unwrap();
    for run in 1..=2 {
        bridge.wait_for_output_matching(
            Stream::Stdout,
            "a newly opened Matter window",
            TIMEOUT,
            |output| output.matches("open for 15 minutes.").count() == run,
        );
        let (output, _) = bridge.output();
        let current_code = output
            .lines()
            .rfind(|line| line.starts_with("Manual pairing code: "))
            .unwrap();
        let current_payload = output
            .lines()
            .rfind(|line| line.starts_with("MT:"))
            .unwrap();
        for command in [&["pairing"][..], &["pairing", "open"][..]] {
            session.send(command);
            let (ok, pairing) = session.result();
            assert!(ok, "{pairing}");
            assert!(pairing.contains(current_code), "{pairing}");
            assert!(pairing.contains(current_payload), "{pairing}");
            assert!(pairing.contains("remaining."), "{pairing}");
        }
        if run == 1 {
            bridge.send("pairing\npairing open\n");
            bridge.wait_for_output_matching(
                Stream::Stdout,
                "both pairing replies",
                TIMEOUT,
                |output| output.matches(current_payload).count() == 3,
            );
            fs::write(directory.path().join("source-remove"), b"remove").unwrap();
        }
    }
    bridge.stop("-TERM");
}
