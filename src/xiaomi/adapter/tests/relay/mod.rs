use super::*;
use crate::{device::RelayMode, xiaomi::capability::PropertyBinding};

mod control;
mod feedback;
mod publication;

fn binding(siid: u32, piid: u32) -> PropertyBinding {
    PropertyBinding { siid, piid }
}

fn panel_peer(mode: RelayMode) -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    let mode_value = match mode {
        RelayMode::Wired => 0,
        RelayMode::Wireless => 1,
    };
    for siid in [2, 3, 4] {
        peer.value("one", binding(siid, 1), json!(false));
        peer.value("one", binding(siid, 2), json!(mode_value));
    }
    peer
}

fn rig() -> Rig {
    Rig::profile_light(
        "zimi.switch.dhkg05",
        serde_json::from_str(include_str!(
            "../../../../../tests/fixtures/miot/zimi-dhkg05.json"
        ))
        .unwrap(),
    )
}
