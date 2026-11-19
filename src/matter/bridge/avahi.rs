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
    utils::sync::Notification,
};
use std::cell::Cell;

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
    pub(super) fn request(&self) -> bool {
        if self.phase.get() != Phase::Serving {
            // This run has made no entry group changes before its first snapshot.
            return false;
        }
        self.phase.set(Phase::RebuildRequested);
        self.requested.notify();
        true
    }

    pub(super) fn finished(&self) -> bool {
        self.phase.get() == Phase::Finished
    }

    pub(super) async fn run(
        &self,
        mdns: &mut AvahiMdns,
        matter: &dyn TransportHost,
    ) -> Result<(), Error> {
        let host = Host {
            matter,
            rebuild: self,
        };
        let result = future::or(mdns.run(&host), async {
            self.requested.wait().await;
            matter.transport().notify_mdns_changed();
            future::pending().await
        })
        .await;
        match result {
            Err(error) if self.finished() && error.code() == ErrorCode::Cancelled => Ok(()),
            result => result,
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
