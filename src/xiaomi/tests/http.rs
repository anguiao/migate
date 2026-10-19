use super::support::{MockResponse, dynamic_mock_server, mock_server};
use std::{
    io::{Read, Write},
    net::TcpStream,
    thread,
    time::Duration,
};

#[test]
fn both_http_servers_wait_for_the_gate_and_notify_after_writing() {
    for dynamic in [false, true] {
        let (release, gate) = flume::bounded(1);
        let (sent, completed) = flume::bounded(1);
        let response = MockResponse::json(200, "{}").gated(gate).on_sent(move || {
            sent.send(()).unwrap();
        });
        let (base, requests) = if dynamic {
            let mut response = Some(response);
            dynamic_mock_server(1, move |_| response.take().unwrap())
        } else {
            mock_server(vec![response])
        };
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(base.strip_prefix("http://").unwrap()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let mut body = String::new();
            stream.read_to_string(&mut body).unwrap();
            body
        });
        requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(completed.try_recv().is_err());
        release.send(()).unwrap();
        completed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(client.join().unwrap().ends_with("\r\n\r\n{}"));
    }
}
