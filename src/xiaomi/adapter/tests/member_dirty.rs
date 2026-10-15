use super::support::cached;
use crate::device::PhysicalDevice;
use event_listener::Event;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::super::{
    state::{DeviceRecord, Lifetime},
    work::DeviceWork,
};

fn member_work() -> Rc<DeviceWork> {
    let cached = cached("uid", "member", false);
    let (physical, _receivers) = PhysicalDevice::new(cached.physical_description("member"));
    let record = Rc::new(DeviceRecord {
        life: Lifetime::default(),
        binding_generation: Cell::new(0),
        cached: RefCell::new(cached),
        physical,
    });
    Rc::new(DeviceWork::new(record, Rc::new(Event::new()), None))
}

#[test]
fn member_dirty_coalesces_without_invalidating_the_queued_read() {
    let work = member_work();
    let id = "main";
    let binding = crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 };
    work.request_member_read();
    let stamp = work.observations.read_stamp(&work.record, id, binding);
    for _ in 0..5 {
        work.request_member_read();
    }
    assert!(
        work.observations
            .read_is_current(&work.record, id, binding, &stamp),
        "repeated member events invalidated the queued read"
    );
    assert_eq!(work.refresh_pending.borrow().len(), 1);
}

#[test]
fn dirty_again_produces_exactly_one_follow_up_read() {
    let work = member_work();
    let id = "main";
    let binding = crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 };
    let key = super::super::lighting::key(id, binding);
    work.refresh_pending.borrow_mut().clear();
    work.dirty_again.borrow_mut().insert(key.clone());
    work.finish_dirty_read(id, binding);
    assert!(!work.dirty_again.borrow().contains(&key));
    assert_eq!(work.refresh_pending.borrow().len(), 1);
    // A second completion without another dirty event must not queue again.
    work.finish_dirty_read(id, binding);
    assert_eq!(work.refresh_pending.borrow().len(), 1);
}
