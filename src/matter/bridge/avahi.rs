use crate::RuntimeError;
use futures_lite::future;
use rs_matter::{
    basic_info::BasicInfoConfig,
    error::{Error, ErrorCode},
    fabric::Fabrics,
    groupcast::TestingBridge,
    icd::IcdAdvertisement,
    transport::{
        Transport, TransportHost,
        network::{MatterLocalService, mdns::avahi::AvahiMdns},
        session::Sessions,
    },
    utils::{sync::Notification, zbus},
};
use std::{cell::Cell, task::Poll};

#[derive(Default)]
pub(super) struct Publisher(Option<AvahiMdns>);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Starting,
    Serving,
    RebuildRequested,
    Finished,
}

#[derive(Default)]
pub(super) struct Rebuild {
    phase: Cell<Phase>,
    requested: Notification,
}

impl Rebuild {
    pub(super) fn request(&self) -> Poll<()> {
        if self.phase.get() != Phase::Serving {
            // This run has made no entry group changes before its first snapshot.
            return Poll::Ready(());
        }
        self.phase.set(Phase::RebuildRequested);
        self.requested.notify();
        Poll::Pending
    }
}

impl Publisher {
    pub(super) async fn run(
        &mut self,
        matter: &dyn TransportHost,
        rebuild: &Rebuild,
    ) -> Result<(), RuntimeError> {
        let mdns = match &mut self.0 {
            Some(mdns) => mdns,
            slot @ None => {
                let bus = zbus::Connection::system().await.map_err(|error| {
                    format!("Failed to connect to the system D-Bus for Matter mDNS: {error}")
                })?;
                slot.insert(AvahiMdns::new(bus))
            }
        };
        let host = Host { matter, rebuild };
        let result = future::or(mdns.run(&host), async {
            rebuild.requested.wait().await;
            matter.transport().notify_mdns_changed();
            future::pending().await
        })
        .await;
        match result {
            Err(error)
                if rebuild.phase.get() == Phase::Finished
                    && error.code() == ErrorCode::Cancelled =>
            {
                Ok(())
            }
            Err(error) => {
                Err(format!("Failed to advertise Matter services with Avahi: {error}").into())
            }
            Ok(()) => Err("Matter instance ended unexpectedly".into()),
        }
    }
}

struct Host<'a> {
    matter: &'a dyn TransportHost,
    rebuild: &'a Rebuild,
}

impl TransportHost for Host<'_> {
    fn transport(&self) -> &Transport {
        self.matter.transport()
    }

    fn with_fabrics(&self, f: &mut dyn FnMut(&mut Sessions, &Fabrics)) {
        self.matter.with_fabrics(f);
    }

    fn mdns_services(
        &self,
        f: &mut dyn FnMut(MatterLocalService) -> Result<(), Error>,
    ) -> Result<(), Error> {
        // AvahiMdns asks for the next snapshot after it has processed the replies
        // to all entry group calls and updated its cache. Stop at that boundary.
        if self.rebuild.phase.get() == Phase::RebuildRequested {
            self.rebuild.phase.set(Phase::Finished);
            return Err(ErrorCode::Cancelled.into());
        }
        self.rebuild.phase.set(Phase::Serving);
        self.matter.mdns_services(f)
    }

    fn dev_det(&self) -> &BasicInfoConfig<'_> {
        self.matter.dev_det()
    }

    fn port(&self) -> u16 {
        self.matter.port()
    }

    fn icd_advertisement(&self) -> Option<IcdAdvertisement> {
        self.matter.icd_advertisement()
    }

    fn groupcast_testing(&self) -> &TestingBridge {
        self.matter.groupcast_testing()
    }
}
