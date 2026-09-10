use super::LIGHT_ENDPOINT;
use crate::{
    device::Command,
    virtual_device::{Changes, VirtualLight},
};
use rs_matter::{
    dm::{
        AsyncHandler, Cluster, HandlerContext, InvokeContext, InvokeReply, LifecycleOp,
        MatchContext, ReadContext, ReadReply, WriteContext,
        clusters::{
            app::{
                level_control::LevelControlHooks,
                on_off::{self, EffectVariantEnum, OnOffHooks, OutOfBandMessage, StartUpOnOffEnum},
            },
            scenes::SceneInvalidator,
        },
    },
    error::{Error, ErrorCode},
    tlv::{FromTLV, Nullable, TLVElement},
};
use std::cell::{Cell, RefCell};

fn validate_command(id: u32, data: &TLVElement<'_>) -> Result<(), Error> {
    data.structure()?;
    data.raw_value()?;
    match id {
        0..=2 | 0x41 => (),
        0x40 => {
            let request = on_off::OffWithEffectRequest::from_tlv(data)?;
            request.effect_identifier()?;
            request.effect_variant()?;
        }
        0x42 => {
            let request = on_off::OnWithTimedOffRequest::from_tlv(data)?;
            request.on_off_control()?;
            request.on_time()?;
            request.off_wait_time()?;
        }
        _ => return Err(ErrorCode::CommandNotFound.into()),
    }
    Ok(())
}

fn basic_command(light: &VirtualLight, id: u32, data: &TLVElement<'_>) -> Result<(), Error> {
    let command = match id {
        0 => Command::Off,
        1 => Command::On,
        2 => Command::Toggle,
        _ => return Err(ErrorCode::CommandNotFound.into()),
    };
    // All three commands carry an empty structure. Validate before touching the device.
    validate_command(id, data)?;
    let snapshot = light.execute(command);
    log::info!(
        "Matter {} {:?} -> {}",
        snapshot.id,
        command,
        if snapshot.power { "on" } else { "off" }
    );
    Ok(())
}

/// Synchronize device-side changes with the Lighting state machine and scenes.
/// Writes from that state machine are marked so they do not loop back as external updates.
pub(super) struct LightHooks<'a> {
    light: &'a VirtualLight,
    changes: RefCell<Option<Changes<'a>>>,
    internal_update: Cell<Option<u64>>,
    scenes: Option<&'a dyn SceneInvalidator>,
}
impl<'a> LightHooks<'a> {
    pub fn with_scenes(mut self, scenes: &'a dyn SceneInvalidator) -> Self {
        self.scenes = Some(scenes);
        self
    }
    pub fn new(light: &'a VirtualLight) -> Self {
        Self {
            light,
            changes: RefCell::new(Some(light.subscribe())),
            internal_update: Cell::new(None),
            scenes: None,
        }
    }
}
impl OnOffHooks for LightHooks<'_> {
    const CLUSTER: Cluster<'static> = on_off::test::TestOnOffDeviceLogic::CLUSTER;
    fn on_off(&self) -> bool {
        self.light.snapshot().power
    }
    fn set_on_off(&self, on: bool) {
        let (_, revision) =
            self.light
                .execute_with_revision(if on { Command::On } else { Command::Off });
        // This is only a notification-origin marker; reads always use the device.
        self.internal_update.set(Some(revision));
    }
    fn start_up_on_off(&self) -> Nullable<StartUpOnOffEnum> {
        Nullable::some(StartUpOnOffEnum::Off)
    }
    fn set_start_up_on_off(&self, value: Nullable<StartUpOnOffEnum>) -> Result<(), Error> {
        if value == Nullable::some(StartUpOnOffEnum::Off) {
            Ok(())
        } else {
            Err(ErrorCode::ConstraintError.into())
        }
    }
    async fn run<F: Fn(OutOfBandMessage)>(&self, notify: F) {
        let mut changes = self.changes.borrow_mut().take().expect("hooks run once");
        loop {
            let (_, revision) = changes.changed_with_revision().await;
            if self.internal_update.take() != Some(revision) {
                if let Some(scenes) = self.scenes {
                    scenes.scenable_attribute_changed(LIGHT_ENDPOINT);
                }
                notify(OutOfBandMessage::Update);
            }
        }
    }
    async fn handle_off_with_effect(&self, _effect: EffectVariantEnum) {}
}

/// Route Matter operations to the device and report device state changes.
/// Reports also cover state-machine writes, so this subscribes separately from
/// the external-change handling in `LightHooks`.
pub(super) struct LightHandler<'a, LH: LevelControlHooks> {
    inner: &'a on_off::OnOffHandler<'a, LightHooks<'a>, LH>,
    light: &'a VirtualLight,
    changes: RefCell<Option<Changes<'a>>>,
}
impl<'a, LH: LevelControlHooks> LightHandler<'a, LH> {
    pub fn new(
        inner: &'a on_off::OnOffHandler<'a, LightHooks<'a>, LH>,
        light: &'a VirtualLight,
    ) -> Self {
        Self {
            inner,
            light,
            changes: RefCell::new(Some(light.subscribe())),
        }
    }
}
impl<LH: LevelControlHooks> AsyncHandler for LightHandler<'_, LH> {
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        on_off::HandlerAsyncAdaptor(self.inner)
            .read(ctx, reply)
            .await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        on_off::HandlerAsyncAdaptor(self.inner).write(ctx).await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if ctx.cmd().cmd_id <= 2 {
            basic_command(self.light, ctx.cmd().cmd_id, ctx.data())
        } else {
            validate_command(ctx.cmd().cmd_id, ctx.data())?;
            on_off::HandlerAsyncAdaptor(self.inner)
                .invoke(ctx, reply)
                .await
        }
    }
    fn bump_dataver(&self, ctx: impl MatchContext) {
        on_off::HandlerAsyncAdaptor(self.inner).bump_dataver(ctx);
    }
    fn lifecycle(&self, ctx: impl HandlerContext, op: LifecycleOp) -> Result<(), Error> {
        on_off::HandlerAsyncAdaptor(self.inner).lifecycle(ctx, op)
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.changes.borrow_mut().take().expect("handler runs once");
        let report = async {
            loop {
                changes.changed().await;
                ctx.notify_attr_changed(
                    LIGHT_ENDPOINT,
                    LightHooks::CLUSTER.id,
                    on_off::AttributeId::OnOff as _,
                );
            }
        };
        futures_lite::future::or(on_off::HandlerAsyncAdaptor(self.inner).run(&ctx), report).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::handle_line;
    use futures_lite::future::{block_on, poll_once};
    use std::pin::pin;

    #[test]
    fn commands_confirm_device_state() {
        let light = VirtualLight::new();
        for (id, power) in [(1, true), (0, false), (2, true), (2, false)] {
            basic_command(&light, id, &rs_matter::tlv::TLVElement::new(&[0x15, 0x18])).unwrap();
            assert_eq!(light.snapshot().power, power);
        }
    }

    #[test]
    fn lighting_payload_is_validated_before_dispatch() {
        let missing_on_time = TLVElement::new(&[0x15, 0x24, 0, 0, 0x25, 2, 1, 0, 0x18]);
        assert!(validate_command(0x42, &missing_on_time).is_err());
        assert!(validate_command(0x41, &TLVElement::new(&[0x15])).is_err());
    }
    #[test]
    fn malformed_requests_never_change_state() {
        let light = VirtualLight::new();
        for bytes in [&[][..], &[0x15], &[0x15, 0x24], &[0x15, 0x35, 0], &[0x09]] {
            assert!(
                basic_command(&light, 1, &TLVElement::new(bytes)).is_err(),
                "accepted {bytes:?}"
            );
            assert!(!light.snapshot().power);
        }
        assert!(basic_command(&light, 99, &TLVElement::new(&[0x15, 0x18])).is_err());
        basic_command(&light, 1, &TLVElement::new(&[0x15, 0x18])).unwrap();
        assert_eq!(
            handle_line(&light, "status").unwrap(),
            "virtual-light-1: on"
        );
    }
    #[test]
    fn terminal_reads_and_notifications_share_the_adapter_device() {
        block_on(async {
            let light = VirtualLight::new();
            let hooks = LightHooks::new(&light);
            let count = std::cell::Cell::new(0);
            // Subscribe during construction, before the background task is first polled.
            handle_line(&light, "on");
            assert!(hooks.on_off());
            let mut run = pin!(hooks.run(|_| count.set(count.get() + 1)));
            assert!(poll_once(&mut run).await.is_none());
            assert_eq!(count.get(), 1);
            handle_line(&light, "on");
            assert!(poll_once(&mut run).await.is_none());
            assert_eq!(count.get(), 1);
            basic_command(&light, 2, &TLVElement::new(&[0x15, 0x18])).unwrap();
            assert!(poll_once(&mut run).await.is_none());
            assert_eq!(count.get(), 2);
            assert!(!hooks.on_off());
        });
    }
    #[test]
    fn protocol_writes_do_not_interrupt_the_lighting_state_machine() {
        block_on(async {
            let light = VirtualLight::new();
            let hooks = LightHooks::new(&light);
            let count = std::cell::Cell::new(0);
            let mut run = pin!(hooks.run(|_| count.set(count.get() + 1)));
            hooks.set_on_off(true);
            assert!(poll_once(&mut run).await.is_none());
            assert_eq!(count.get(), 0);
            handle_line(&light, "off");
            assert!(poll_once(&mut run).await.is_none());
            assert_eq!(count.get(), 1);
        });
    }
    #[test]
    fn coalesced_external_changes_are_not_mistaken_for_an_internal_write() {
        block_on(async {
            let light = VirtualLight::new();
            let hooks = LightHooks::new(&light);
            let count = Cell::new(0);
            hooks.set_on_off(true);
            handle_line(&light, "off");
            handle_line(&light, "on");
            let mut run = pin!(hooks.run(|_| count.set(count.get() + 1)));
            assert!(poll_once(&mut run).await.is_none());
            assert_eq!(count.get(), 1);
        });
    }
    #[test]
    fn startup_policy_accepts_only_off() {
        let light = VirtualLight::new();
        let hooks = LightHooks::new(&light);
        assert!(
            hooks
                .set_start_up_on_off(Nullable::some(StartUpOnOffEnum::Off))
                .is_ok()
        );
        assert!(
            hooks
                .set_start_up_on_off(Nullable::some(StartUpOnOffEnum::On))
                .is_err()
        );
        assert!(hooks.set_start_up_on_off(Nullable::none()).is_err());
    }
}
