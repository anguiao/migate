use super::{Token, wire};
use async_io::{Async, Timer};
use futures_lite::future;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    fmt,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    rc::Rc,
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(10);
static IDS: OnceLock<AtomicU32> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendStage {
    BeforeSend,
    Sent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Failure {
    Io,
    Timeout,
    Rejected(i64),
    Identity,
    InvalidResponse,
    Withdrawn,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RequestError {
    pub(crate) stage: SendStage,
    pub(crate) failure: Failure,
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.failure {
            Failure::Io => formatter.write_str("LAN socket error"),
            Failure::Timeout => formatter.write_str("LAN request timed out"),
            Failure::Rejected(code) => write!(formatter, "device rejected LAN request ({code})"),
            Failure::Identity => formatter.write_str("LAN device identity does not match"),
            Failure::InvalidResponse => formatter.write_str("invalid LAN business response"),
            Failure::Withdrawn => formatter.write_str("LAN request withdrawn before send"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Client(Rc<Inner>);

struct Inner {
    did: u64,
    address: SocketAddr,
    token: Token,
    timestamp: RefCell<Option<(u32, Instant)>>,
    timeout: Duration,
}

impl Client {
    pub(crate) fn new(did: u64, address: SocketAddr, token: Token) -> Self {
        Self(Rc::new(Inner {
            did,
            address,
            token,
            timestamp: RefCell::new(None),
            timeout: TIMEOUT,
        }))
    }

    #[cfg(test)]
    pub(crate) fn set_timeout(&mut self, timeout: Duration) {
        Rc::get_mut(&mut self.0).unwrap().timeout = timeout;
    }

    /// One attempt has one deadline and never retransmits an emitted request.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, RequestError> {
        self.request_guarded(method, params, || true, || {}).await
    }

    /// Recheck authority immediately before the business datagram and report
    /// its successful send before waiting for a response.
    pub(crate) async fn request_guarded(
        &self,
        method: &str,
        params: Value,
        guard: impl Fn() -> bool,
        on_sent: impl FnOnce(),
    ) -> Result<Value, RequestError> {
        self.request_with_timeout_guarded(method, params, self.0.timeout, guard, on_sent)
            .await
    }

    pub(crate) async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        self.request_with_timeout_guarded(method, params, timeout, || true, || {})
            .await
    }

    async fn request_with_timeout_guarded(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        guard: impl Fn() -> bool,
        on_sent: impl FnOnce(),
    ) -> Result<Value, RequestError> {
        let stage = Cell::new(SendStage::BeforeSend);
        let result = future::or(
            async { self.exchange(method, params, &stage, guard, on_sent).await },
            async {
                Timer::after(timeout.min(TIMEOUT)).await;
                Err(RequestError {
                    stage: stage.get(),
                    failure: Failure::Timeout,
                })
            },
        )
        .await;
        if result
            .as_ref()
            .is_err_and(|error| matches!(error.failure, Failure::Io | Failure::Timeout))
        {
            self.0.timestamp.borrow_mut().take();
        }
        result
    }

    async fn exchange(
        &self,
        method: &str,
        params: Value,
        stage: &Cell<SendStage>,
        guard: impl Fn() -> bool,
        on_sent: impl FnOnce(),
    ) -> Result<Value, RequestError> {
        let withdrawn = || RequestError {
            stage: SendStage::BeforeSend,
            failure: Failure::Withdrawn,
        };
        if !guard() {
            return Err(withdrawn());
        }
        let io = || RequestError {
            stage: stage.get(),
            failure: Failure::Io,
        };
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
            .and_then(Async::new)
            .map_err(|_| io())?;
        let mut buffer = [0; 65535];
        if self.0.timestamp.borrow().is_none() {
            socket
                .send_to(&wire::HELLO, self.0.address)
                .await
                .map_err(|_| io())?;
            loop {
                let (length, peer) = socket.recv_from(&mut buffer).await.map_err(|_| io())?;
                if peer == self.0.address
                    && let Some((did, stamp)) = wire::hello(&buffer[..length])
                    && did == self.0.did
                {
                    *self.0.timestamp.borrow_mut() = Some((stamp, Instant::now()));
                    break;
                }
            }
        }
        let (stamp, started) = self.0.timestamp.borrow().unwrap();
        let timestamp = stamp.wrapping_add(started.elapsed().as_secs() as u32);
        let sequence = IDS
            .get_or_init(|| AtomicU32::new(rand::random()))
            .fetch_add(1, Ordering::Relaxed);
        let id = request_id(sequence);
        let payload = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))
            .expect("JSON values serialize");
        if payload.len() > u16::MAX as usize - 48 {
            return Err(RequestError {
                stage: SendStage::BeforeSend,
                failure: Failure::InvalidResponse,
            });
        }
        let packet = wire::encode(self.0.did, timestamp, &self.0.token, &payload);
        let mut denied = false;
        let send = socket
            .write_with(|socket| {
                if !guard() {
                    denied = true;
                    return Err(std::io::ErrorKind::Other.into());
                }
                socket.send_to(&packet, self.0.address)
            })
            .await;
        if denied {
            return Err(withdrawn());
        }
        send.map_err(|_| io())?;
        stage.set(SendStage::Sent);
        on_sent();
        loop {
            let (length, peer) = socket.recv_from(&mut buffer).await.map_err(|_| io())?;
            if peer != self.0.address {
                continue;
            }
            let Some((did, stamp, mut payload)) = wire::decode(&buffer[..length], &self.0.token)
            else {
                continue;
            };
            if did != self.0.did {
                continue;
            }
            while payload.last() == Some(&0) {
                payload.pop();
            }
            let Ok(value) = serde_json::from_slice::<Value>(&payload) else {
                continue;
            };
            if value.get("id").and_then(Value::as_u64) != Some(u64::from(id)) {
                continue;
            }
            let result = if let Some(result) = value.get("result") {
                Ok(result.clone())
            } else if let Some(code) = value.pointer("/error/code").and_then(Value::as_i64) {
                Err(RequestError {
                    stage: SendStage::Sent,
                    failure: Failure::Rejected(code),
                })
            } else {
                continue;
            };
            *self.0.timestamp.borrow_mut() = Some((stamp, Instant::now()));
            return result;
        }
    }
}

fn request_id(sequence: u32) -> u32 {
    sequence % 0x8000_0000 + 1
}

#[cfg(test)]
#[path = "tests/client.rs"]
mod tests;
