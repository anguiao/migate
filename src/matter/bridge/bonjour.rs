use crate::RuntimeError;
use rs_matter::transport::{TransportHost, network::mdns::astro::AstroMdns};
use std::task::Poll;

#[derive(Default)]
pub(super) struct Publisher(AstroMdns);

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
        self.0.run(matter).await.map_err(|error| {
            format!("Failed to advertise Matter services with Bonjour: {error}")
        })?;
        Err("Matter instance ended unexpectedly".into())
    }
}
