use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DnType, IsCa, Issuer,
    KeyPair,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};
use time::OffsetDateTime;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;

pub(crate) struct ReceivedRequest {
    pub target: String,
    pub headers: String,
    pub body: String,
}

pub(crate) struct MockResponse {
    wire: String,
    delay: Duration,
    split_at: Option<usize>,
}

impl MockResponse {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            wire: format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
            delay: Duration::ZERO,
            split_at: None,
        }
    }

    pub fn raw(wire: impl Into<String>) -> Self {
        Self {
            wire: wire.into(),
            delay: Duration::ZERO,
            split_at: None,
        }
    }

    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn delayed_body(status: u16, body: &str, delivered: usize, delay: Duration) -> Self {
        let mut response = Self::json(status, body);
        let header_end = response.wire.find("\r\n\r\n").unwrap() + 4;
        response.split_at = Some(header_end + delivered.min(body.len()));
        response.delay = delay;
        response
    }
}

pub(crate) fn mock_server(responses: Vec<MockResponse>) -> (String, Receiver<ReceivedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for response in responses {
            let Some(mut stream) = accept_until(&listener, Instant::now() + Duration::from_secs(2))
            else {
                return;
            };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            let _ = sender.send(request);
            if let Some(split_at) = response.split_at {
                let _ = stream.write_all(&response.wire.as_bytes()[..split_at]);
                thread::sleep(response.delay);
                let _ = stream.write_all(&response.wire.as_bytes()[split_at..]);
            } else {
                thread::sleep(response.delay);
                let _ = stream.write_all(response.wire.as_bytes());
            }
        }
    });
    (format!("http://{address}"), receiver)
}

fn accept_until(listener: &TcpListener, deadline: Instant) -> Option<TcpStream> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Some(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<ReceivedRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let (header_end, content_length) = loop {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 || bytes.len().checked_add(read)? > MAX_REQUEST_BYTES {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..header_end + 4]).ok()?;
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if header_end + 4 + content_length > MAX_REQUEST_BYTES {
                return None;
            }
            break (header_end, content_length);
        }
    };
    while bytes.len() < header_end + 4 + content_length {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 || bytes.len().checked_add(read)? > MAX_REQUEST_BYTES {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let headers = String::from_utf8(bytes[..header_end].to_vec()).ok()?;
    let target = headers
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .to_owned();
    let body =
        String::from_utf8(bytes[header_end + 4..header_end + 4 + content_length].to_vec()).ok()?;
    Some(ReceivedRequest {
        target,
        headers,
        body,
    })
}

pub(crate) fn sign_csr(
    csr_pem: &str,
    not_before: i64,
    not_after: i64,
) -> Result<String, rcgen::Error> {
    let request = CertificateSigningRequestParams::from_pem(csr_pem)?;
    let issuer_key = KeyPair::generate()?;
    let mut issuer_params = CertificateParams::new(Vec::<String>::new())?;
    issuer_params
        .distinguished_name
        .push(DnType::CommonName, "MiGate test CA");
    issuer_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let issuer = Issuer::new(issuer_params, issuer_key);
    let mut request = request;
    request.params.not_before = OffsetDateTime::from_unix_timestamp(not_before)
        .map_err(|_| rcgen::Error::InvalidNameType)?;
    request.params.not_after = OffsetDateTime::from_unix_timestamp(not_after)
        .map_err(|_| rcgen::Error::InvalidNameType)?;
    request
        .signed_by(&issuer)
        .map(|certificate| certificate.pem())
}
