use crate::RuntimeError;
use futures_lite::future;
use rs_matter::{
    error::{Error, ErrorCode},
    transport::{TransportHost, network::mdns::astro::AstroMdns},
};
use std::{
    pin::pin,
    task::Poll,
    time::{Duration, Instant},
};

const RELEASE_DELAY: Duration = Duration::from_millis(200);
const QUICK_RESUME: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct Publisher {
    mdns: AstroMdns,
    started: bool,
    stopped_at: Option<Instant>,
}

#[derive(Default)]
pub(super) struct Rebuild;

impl Rebuild {
    pub(super) fn request(&self) -> Poll<()> {
        // Bonjour updates its registrations synchronously between await points.
        Poll::Ready(())
    }
}

impl Publisher {
    pub(super) async fn run(
        &mut self,
        matter: &dyn TransportHost,
        _rebuild: &Rebuild,
    ) -> Result<(), RuntimeError> {
        self.started = true;
        let quick_resume = self
            .stopped_at
            .take()
            .is_some_and(|at| at.elapsed() < QUICK_RESUME);
        let mut paired = false;
        matter.with_fabrics(&mut |_, fabrics| paired = fabrics.iter().next().is_some());
        if quick_resume && paired {
            matter.transport().notify_mdns_changed();
            let error = {
                let mut first = pin!(self.mdns.run(matter));
                match future::poll_once(first.as_mut()).await {
                    Some(Err(error)) if error.code() == ErrorCode::MdnsError => error,
                    Some(result) => return finished(result),
                    None => return finished(first.await),
                }
            };
            log::warn!("Retrying the first Bonjour advertisement after account recovery: {error}");
            async_io::Timer::after(RELEASE_DELAY).await;
            matter.transport().notify_mdns_changed();
        }
        finished(self.mdns.run(matter).await)
    }

    pub(super) async fn stop(&mut self) {
        if std::mem::take(&mut self.started) {
            self.mdns = AstroMdns::default();
            self.stopped_at = Some(Instant::now());
            // astro-dnssd signals its background thread on drop. The thread
            // checks for shutdown every 100 ms and then releases DNSServiceRef.
            // Registering the same operational record before that release can
            // leave the next DNSServiceRegister without a callback.
            async_io::Timer::after(RELEASE_DELAY).await;
        }
    }
}

fn finished(result: Result<(), Error>) -> Result<(), RuntimeError> {
    result.map_err(|error| format!("Failed to advertise Matter services with Bonjour: {error}"))?;
    Err("Matter instance ended unexpectedly".into())
}
