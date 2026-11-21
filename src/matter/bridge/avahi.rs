use crate::RuntimeError;
use futures_lite::{StreamExt, future};
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
    utils::{
        sync::Notification,
        zbus::{self, fdo},
    },
};
use std::{cell::Cell, future::Future, pin::Pin, task::Poll};

const AVAHI_NAME: &str = "org.freedesktop.Avahi";
const ENTRY_GROUP_INTERFACE: &str = "org.freedesktop.Avahi.EntryGroup";
const ENTRY_GROUP_COLLISION: i32 = 3;
const ENTRY_GROUP_FAILURE: i32 = 4;

#[derive(Default)]
pub(super) struct Publisher(Option<Client>);

struct Client {
    mdns: AvahiMdns,
    // Keep the subscriptions and any in-flight sender check across Matter rebuilds.
    watch: Pin<Box<dyn Future<Output = Result<(), RuntimeError>>>>,
}

impl Client {
    async fn connect() -> Result<Self, RuntimeError> {
        let connection = zbus::Connection::system().await.map_err(|error| {
            format!("Failed to connect to the system D-Bus for Matter mDNS: {error}")
        })?;
        let dbus = fdo::DBusProxy::new(&connection).await?;
        let owner_changes = dbus
            .receive_name_owner_changed_with_args(&[(0, AVAHI_NAME)])
            .await?;
        let destination = connection
            .unique_name()
            .ok_or("The system D-Bus did not assign a client name")?;
        // Avahi directs entry group state changes to the connection that created the group.
        let group_rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(AVAHI_NAME)?
            .interface(ENTRY_GROUP_INTERFACE)?
            .member("StateChanged")?
            .destination(destination.as_str())?
            .build();
        let group_changes =
            zbus::MessageStream::for_match_rule(group_rule, &connection, None).await?;
        let watch = Box::pin(future::or(
            watch_avahi_owner(owner_changes),
            watch_entry_groups(group_changes, dbus),
        ));
        Ok(Self {
            mdns: AvahiMdns::new(connection),
            watch,
        })
    }
}

async fn watch_avahi_owner(mut changes: fdo::NameOwnerChangedStream) -> Result<(), RuntimeError> {
    while let Some(signal) = changes.next().await {
        if signal.args()?.old_owner().is_some() {
            return Err("Avahi D-Bus name owner changed while publishing Matter services".into());
        }
    }
    Err("Avahi D-Bus name owner stream ended".into())
}

async fn watch_entry_groups(
    mut changes: zbus::MessageStream,
    dbus: fdo::DBusProxy<'_>,
) -> Result<(), RuntimeError> {
    let avahi = zbus::names::BusName::try_from(AVAHI_NAME)?;
    while let Some(message) = changes.try_next().await? {
        let body = message.body();
        let Ok((state, reason)) = body.deserialize::<(i32, &str)>() else {
            continue;
        };
        let state = match state {
            ENTRY_GROUP_COLLISION => "collision",
            ENTRY_GROUP_FAILURE => "failure",
            _ => continue,
        };
        // zbus cannot compare a message's unique sender to a well-known name in a match rule.
        let owner = match dbus.get_name_owner(avahi.clone()).await {
            Ok(owner) => owner,
            Err(fdo::Error::NameHasNoOwner(_)) => continue,
            Err(error) => return Err(error.into()),
        };
        if message
            .header()
            .sender()
            .is_some_and(|sender| sender.as_str() == owner.as_str())
        {
            return Err(format!("Avahi Matter entry group reported {state}: {reason}").into());
        }
    }
    Err("Avahi D-Bus entry group state stream ended".into())
}

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
        let client = match &mut self.0 {
            Some(client) => client,
            slot @ None => slot.insert(Client::connect().await?),
        };
        let host = Host { matter, rebuild };
        let publish = async {
            match client.mdns.run(&host).await {
                Err(error)
                    if rebuild.phase.get() == Phase::Finished
                        && error.code() == ErrorCode::Cancelled =>
                {
                    Ok(())
                }
                Err(error) => Err(error.into()),
                Ok(()) => Err("Matter instance ended unexpectedly".into()),
            }
        };
        let notify_rebuild = async {
            rebuild.requested.wait().await;
            matter.transport().notify_mdns_changed();
            future::pending::<Result<(), RuntimeError>>().await
        };
        future::or(client.watch.as_mut(), future::or(publish, notify_rebuild))
            .await
            .map_err(|error| {
                format!("Failed to advertise Matter services with Avahi: {error}").into()
            })
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
