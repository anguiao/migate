use crate::{
    device::Rgb,
    lighting::{
        color::Task as ColorTask,
        level::{Direction, Task},
    },
};

use super::plan as lighting_plan;
use super::{
    Lighting, Task as LightingTask,
    handlers::{ColorHandler, LevelHandler},
};
use crate::lighting::clock::Clock;
use crate::{
    device::{
        Availability, CommandError, DeviceRequest, Function, LightColorTarget, LightDimension,
        LightMode, LightPatch, LightRequest,
    },
    matter::{
        model,
        storage::StoreAdapter,
        tests::support::{self, Context},
    },
    storage::Store,
};
use futures_lite::future::{block_on, poll_once};
use rs_matter::error::ErrorCode;
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        Dataver,
        clusters::decl::{color_control as c, level_control as l},
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
    },
    im::{InteractionModel, InteractionModelState},
    tlv::TLVElement,
    transport::exchange::MatterBuffers,
};
use std::{
    cell::Cell,
    future::Future,
    pin::{Pin, pin},
    rc::Rc,
};

async fn execute_color(light: &Lighting, task: ColorTask) -> Result<(), rs_matter::error::Error> {
    light
        .begin(LightingTask::Color(task), None)?
        .accepted()
        .await
        .map_err(super::command_error)?;
    light.refresh();
    Ok(())
}

/// Lightweight lighting fixture: a plain published Function and a controllable
/// clock, without a database or Matter node.
fn light_fixture(
    rgb: bool,
) -> (
    Lighting,
    Function,
    crate::device::CommandReceiver,
    Rc<TestClock>,
) {
    let (device, receiver) = support::light(rgb);
    let function = device.functions()[0].clone();
    device.set_availability(Availability::Available);
    let clock = Rc::new(TestClock::default());
    let light = Lighting::with_clock(
        function.clone(),
        2,
        Dataver::new(1),
        Dataver::new(2),
        Dataver::new(3),
        clock.clone(),
    )
    .unwrap();
    (light, function, receiver, clock)
}

/// Collect the compact notification list the report owner would emit.
fn collect_reports(light: &Lighting) -> Vec<(u32, u32)> {
    let mut changes = Vec::new();
    light.report.reports(
        &light.function,
        &light.scheduler,
        light.profile,
        |cluster, attribute| changes.push((cluster, attribute)),
    );
    changes
}

#[derive(Default)]
struct TestClock {
    millis: Cell<u64>,
    changed: event_listener::Event,
}
impl TestClock {
    fn set(&self, millis: u64) {
        self.millis.set(millis);
        self.changed.notify(usize::MAX);
    }
}
impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.millis.get()
    }
    fn wait_until(&self, deadline: u64) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(async move {
            loop {
                let changed = self.changed.listen();
                if self.now() >= deadline {
                    return;
                }
                changed.await;
            }
        })
    }
}
macro_rules! setup {
    ($rgb:expr,$function:ident,$receiver:ident,$light:ident,$clock:ident,$ctx:ident) => {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();

        let (device, mut $receiver) = support::light($rgb);
        let $function = device.functions()[0].clone();
        device.set_availability(Availability::Available);

        let handler = support::model_handler(vec![support::single_entry(&device)]);

        let info = model::basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();

        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));

        let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
        let $ctx = Context::new(&im, 8, 0);

        let $clock = Rc::new(TestClock::default());
        let $light = Lighting::with_clock(
            $function.clone(),
            2,
            Dataver::new(1),
            Dataver::new(2),
            Dataver::new(3),
            $clock.clone(),
        )
        .unwrap();
    };
}
async fn frame(receiver: &mut crate::device::CommandReceiver) -> LightRequest {
    match receiver.next_operation().await.unwrap() {
        DeviceRequest::Light(frame) => frame,
        DeviceRequest::Power(_) => panic!("expected lighting frame"),
    }
}
fn accept(function: &Function, request: LightRequest) {
    request.mark_sent().unwrap();
    let target = request.target;
    let mut patch = LightPatch {
        power: target.power(),
        brightness_percent: target.brightness_percent(),
        ..Default::default()
    };
    match target.color_target() {
        Some(LightColorTarget::Temperature(value)) => {
            patch.temperature_kelvin = Some(value.round() as u32);
            patch.mode = Some(LightMode::Temperature);
        }
        Some(LightColorTarget::Rgb(value)) => {
            patch.color = Some(value);
            patch.mode = Some(LightMode::Rgb);
        }
        None => {}
    }
    function.observe_light(patch).unwrap();
    request.accept().unwrap();
}

mod hs_handlers;

mod hs_lifecycle;

mod hs_endpoints;

mod hs_async;

mod color_reports;

mod plan;

mod report;

mod units;

mod baseline_reports;
mod commands;
mod lifecycle;
