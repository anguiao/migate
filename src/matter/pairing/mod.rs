use crate::RuntimeError;
use async_channel::Sender;
use futures_lite::future;
use rs_matter::{
    Matter,
    error::Error,
    pairing::{
        DiscoveryCapabilities,
        qr::{CommFlowType, QrPayload, no_optional_data},
    },
    sc::pase::{CommWindow, CommWindowType},
    utils::timers::{Instant, Timer},
};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

const WINDOW_SECONDS: u16 = 900;
const NOT_READY: &str = "Matter is not ready";

pub enum PairingEvent {
    Opened {
        window_seconds: u16,
        manual_code: String,
        qr_payload: String,
    },
    Expired,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PairingStatus {
    Paired,
    Closed,
    Open {
        expires_at: Instant,
        manual_code: String,
        qr_payload: String,
    },
    OpenWithAdministratorCode {
        expires_at: Instant,
    },
}

struct Request {
    open: bool,
    reply: Sender<Result<PairingStatus, Error>>,
}

/// Routes requests only to the currently running Matter instance.
#[derive(Clone, Default)]
pub(crate) struct PairingControl(Rc<RefCell<Weak<Sender<Request>>>>);

impl PairingControl {
    pub(crate) async fn status(&self) -> Result<PairingStatus, RuntimeError> {
        self.request(false).await
    }

    pub(crate) async fn open(&self) -> Result<PairingStatus, RuntimeError> {
        self.request(true).await
    }

    async fn request(&self, open: bool) -> Result<PairingStatus, RuntimeError> {
        let requests = self.0.borrow().upgrade().ok_or(NOT_READY)?;
        let (reply, result) = async_channel::bounded(1);
        requests
            .send(Request { open, reply })
            .await
            .map_err(|_| NOT_READY)?;
        let status = future::or(
            async { result.recv().await.map_err(|_| NOT_READY) },
            async {
                requests.closed().await;
                Err(NOT_READY)
            },
        )
        .await?;
        Ok(status?)
    }

    pub(super) async fn run(
        &self,
        matter: &Matter<'_>,
        open_window: impl Fn(u16) -> Result<(), Error>,
        close_window: impl Fn() -> Result<bool, Error>,
        now: impl Fn() -> Instant,
        report: impl Fn(PairingEvent) -> std::io::Result<()>,
    ) -> Result<(), RuntimeError> {
        if !matter.has_fabrics() {
            open_window(WINDOW_SECONDS)?;
            report(opened(matter, WINDOW_SECONDS)?)?;
        }
        let (sender, requests) = async_channel::bounded(1);
        let sender = Rc::new(sender);
        *self.0.borrow_mut() = Rc::downgrade(&sender);

        let mut last_deadline: Option<Instant> = None;
        let mut request: Option<Request> = None;
        loop {
            let changed = matter.transport().wait_comm_window_changed();
            let mut window = Window::read(matter);
            let now = now();
            let expired = match window {
                Window::Open { expires_at, .. } if expires_at <= now => {
                    close_window()?;
                    window = Window::Closed;
                    true
                }
                // The interaction model can close the window before we are polled.
                Window::Closed => last_deadline.is_some_and(|deadline| deadline <= now),
                _ => false,
            };
            if expired {
                report(PairingEvent::Expired)?;
            }

            if let Some(request) = request.take() {
                let status = if request.open && window == Window::Closed {
                    open_window(WINDOW_SECONDS).and_then(|()| {
                        window = Window::read(matter);
                        window.status(matter)
                    })
                } else {
                    window.status(matter)
                };
                // The caller can leave after the request has been accepted.
                let _ = request.reply.try_send(status);
            }

            last_deadline = match window {
                Window::Open { expires_at, .. } => Some(expires_at),
                _ => None,
            };
            request = future::or(
                async {
                    Some(
                        requests
                            .recv()
                            .await
                            .expect("pairing runner holds a sender"),
                    )
                },
                async {
                    let timeout = async {
                        if let Some(deadline) = last_deadline {
                            Timer::at(deadline).await;
                        } else {
                            future::pending::<()>().await;
                        }
                    };
                    future::or(changed, timeout).await;
                    None
                },
            )
            .await;
        }
    }
}

#[derive(PartialEq, Eq)]
enum Window {
    Paired,
    Closed,
    Open {
        expires_at: Instant,
        kind: CommWindowType,
    },
}

impl Window {
    fn read(matter: &Matter<'_>) -> Self {
        matter.with_state(|state| {
            if state.fabrics.iter().next().is_some() {
                return Self::Paired;
            }
            let expires_at = state.pase.comm_window_expires_at();
            let kind = state.pase.comm_window().map(CommWindow::comm_window_type);
            match expires_at.zip(kind) {
                Some((expires_at, kind)) => Self::Open { expires_at, kind },
                None => Self::Closed,
            }
        })
    }

    fn status(&self, matter: &Matter<'_>) -> Result<PairingStatus, Error> {
        Ok(match *self {
            Self::Paired => PairingStatus::Paired,
            Self::Closed => PairingStatus::Closed,
            Self::Open {
                expires_at,
                kind: CommWindowType::Basic,
            } => {
                let (manual_code, qr_payload) = codes(matter)?;
                PairingStatus::Open {
                    expires_at,
                    manual_code,
                    qr_payload,
                }
            }
            Self::Open {
                expires_at,
                kind: CommWindowType::Enhanced,
            } => PairingStatus::OpenWithAdministratorCode { expires_at },
        })
    }
}

fn opened(matter: &Matter<'_>, window_seconds: u16) -> Result<PairingEvent, Error> {
    let (manual_code, qr_payload) = codes(matter)?;
    Ok(PairingEvent::Opened {
        window_seconds,
        manual_code,
        qr_payload,
    })
}

fn codes(matter: &Matter<'_>) -> Result<(String, String), Error> {
    let info = matter.dev_det();
    let comm = matter
        .dev_comm()
        .passcode()
        .expect("MiGate uses passcode commissioning data");
    let payload = QrPayload::new(
        DiscoveryCapabilities::IP,
        CommFlowType::Standard,
        comm.clone(),
        info.vid,
        info.pid,
        info.serial_no,
        no_optional_data,
    );
    let mut buffer = [0; 1024];
    let (text, _) = payload.as_str(&mut buffer)?;
    Ok((comm.compute_pairing_code().to_string(), text.to_owned()))
}

#[cfg(test)]
mod tests;
