use event_listener::Event;
use rs_matter::{
    Matter,
    acl::{AclEntry, AuthMode},
    crypto::test_only_crypto,
    dm::Privilege,
    error::Error,
    transport::{
        network::{Address, NetworkReceive, NetworkSend},
        session::{NocCatIds, ReservedSession, SessionMode},
    },
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    num::NonZeroU8,
};

const ADDRESS: Address = Address::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)));

// Exercise real Matter exchanges without binding the live bridge's UDP port.
#[derive(Default)]
pub(super) struct Pipe {
    packets: RefCell<VecDeque<Vec<u8>>>,
    ready: Event,
}

pub(super) struct SendPipe<'a>(pub(super) &'a Pipe);
pub(super) struct ReceivePipe<'a>(pub(super) &'a Pipe);

impl NetworkSend for SendPipe<'_> {
    async fn send_to(&mut self, data: &[u8], _address: Address) -> Result<(), Error> {
        self.0.packets.borrow_mut().push_back(data.to_vec());
        self.0.ready.notify(usize::MAX);
        Ok(())
    }
}

impl NetworkReceive for ReceivePipe<'_> {
    async fn wait_available(&mut self) -> Result<(), Error> {
        loop {
            let ready = self.0.ready.listen();
            if !self.0.packets.borrow().is_empty() {
                return Ok(());
            }
            ready.await;
        }
    }

    async fn recv_from(&mut self, buffer: &mut [u8]) -> Result<(usize, Address), Error> {
        self.wait_available().await?;
        let packet = self.0.packets.borrow_mut().pop_front().unwrap();
        buffer[..packet.len()].copy_from_slice(&packet);
        Ok((packet.len(), ADDRESS))
    }
}

// As in upstream's IM tests, install a test CASE session instead of testing
// commissioning. Each boot has fresh Matter instances and a new session ID.
pub(super) fn connect(matter: &Matter<'_>, local: u64, peer: u64, session_id: u16) {
    connect_at(matter, NonZeroU8::new(1).unwrap(), local, peer, session_id);
}

pub(super) fn connect_at(
    matter: &Matter<'_>,
    fabric: NonZeroU8,
    local: u64,
    peer: u64,
    session_id: u16,
) {
    matter.with_state(|state| {
        while state.fabrics.iter().count() < usize::from(fabric.get()) {
            state.fabrics.add_with_post_init(|_| Ok(())).unwrap();
        }
        let mut acl = AclEntry::new(None, Privilege::ADMIN, AuthMode::Case);
        acl.add_subject(peer).unwrap();
        state
            .fabrics
            .fabric_mut(fabric)
            .unwrap()
            .acl_add(acl)
            .unwrap();
    });
    let mut session = ReservedSession::reserve_now(matter, test_only_crypto()).unwrap();
    session
        .update(
            local,
            peer,
            session_id,
            session_id,
            ADDRESS,
            SessionMode::Case {
                fab_idx: fabric,
                cat_ids: NocCatIds::default(),
            },
            None,
            None,
            None,
            None,
        )
        .unwrap();
    session.complete();
}
