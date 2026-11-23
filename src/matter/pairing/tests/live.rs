use super::*;
use crate::{
    configuration::Scope,
    device::{DeviceKey, FunctionKey},
    matter::{
        Advertisement, Bridge, LabelWriter, MatterConfiguration, model, storage::StoreAdapter,
    },
    storage::StorageError,
};
use futures_lite::future::{block_on, or, poll_once};
use rs_matter::{
    crypto::{Crypto, default_crypto},
    dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT},
    im::{InteractionModel, InteractionModelState},
    pairing::BasicCommData,
    transport::{exchange::MatterBuffers, network::MatterLocalService},
    utils::timers::Duration,
};
use std::cell::{Cell, RefCell};

#[test]
fn running_bridge_opens_a_new_window_on_each_unpaired_run() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let store = Store::open(dir.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let mut advertisement = Advertisement::new(0);
                let mut last_deadline = None;
                for root_version in [1, 2] {
                    let bridge = Bridge::new(&store, &identity, Rc::new(NoLabels));
                    let control = bridge.pairing_control();
                    let opened = Cell::new(0);
                    assert!(control.status().await.is_err());
                    let before = Instant::now();
                    let mut runner = Box::pin(bridge.run_with_advertisement(
                        MatterConfiguration {
                            entries: vec![],
                            root_version,
                        },
                        &mut advertisement,
                        |event| {
                            if let PairingEvent::Opened { window_seconds, .. } = event {
                                opened.set(window_seconds);
                            }
                            Ok(())
                        },
                    ));
                    assert!(poll_once(runner.as_mut()).await.is_none());
                    assert_eq!(opened.get(), 900);
                    let end = or(
                        async {
                            let status = control.status().await.unwrap();
                            assert_eq!(control.open().await.unwrap(), status);
                            let PairingStatus::Open { expires_at, .. } = status else {
                                panic!("expected the initialized Matter window");
                            };
                            expires_at
                        },
                        async { panic!("Matter runner ended: {:?}", runner.as_mut().await) },
                    )
                    .await;
                    assert!(end >= before + Duration::from_secs(900));
                    assert!(end <= Instant::now() + Duration::from_secs(900));
                    assert!(last_deadline.is_none_or(|previous| end > previous));
                    last_deadline = Some(end);
                    let mut queued = Box::pin(control.status());
                    assert!(poll_once(queued.as_mut()).await.is_none());
                    drop(runner);
                    assert!(queued.await.is_err());
                    assert!(control.open().await.is_err());
                }
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn expiry_reopens_the_same_matter_instance_and_an_added_fabric_hides_codes() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let store = Store::open(dir.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let info = basic_info(&identity);
                let passcode = 34_567_890_u32;
                let discriminator = 2_976;
                let matter = Matter::new(
                    &info,
                    BasicCommData {
                        password: passcode.to_le_bytes().into(),
                        discriminator,
                    }
                    .into(),
                    &TEST_DEV_ATT,
                    0,
                );
                let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
                let handler =
                    model::handler_from(&matter, vec![], crypto.rand().unwrap(), Rc::new(NoLabels));
                let buffers: MatterBuffers = MatterBuffers::new();
                let state: InteractionModelState = InteractionModelState::new();
                let kv = matter.kv(StoreAdapter::new(store.matter()));
                let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, kv, &state);
                let control = PairingControl::default();
                let sample = Cell::new(None);
                let events = RefCell::new(Vec::new());
                let mut service = Box::pin(control.run(
                    &matter,
                    |seconds| im.open_basic_comm_window(seconds),
                    || im.close_comm_window(),
                    || sample.take().unwrap_or_else(Instant::now),
                    |event| {
                        events.borrow_mut().push(event);
                        Ok(())
                    },
                ));
                assert!(poll_once(service.as_mut()).await.is_none());
                or(
                    async {
                        let status = control.status().await.unwrap();
                        assert_eq!(status, control.open().await.unwrap());
                        let PairingStatus::Open {
                            expires_at,
                            manual_code,
                            qr_payload,
                        } = status
                        else {
                            panic!("expected the initialized Matter window");
                        };
                        let mut buffer = [0; 1024];
                        let qr = QrPayload::parse(&qr_payload, &mut buffer).unwrap();
                        let manual = QrPayload::parse_pairing_code(&manual_code).unwrap();
                        assert_eq!(qr.passcode(), passcode);
                        assert_eq!(manual.passcode(), passcode);
                        let (first_id, first_discriminator) =
                            commissionable_service(&matter).unwrap();
                        assert_eq!(first_discriminator, qr.discriminator());
                        assert_eq!(
                            matter.with_state(|state| state.pase.comm_window_expires_at()),
                            Some(expires_at)
                        );

                        sample.set(Some(expires_at));
                        assert_eq!(control.status().await.unwrap(), PairingStatus::Closed);
                        assert!(!matter.comm_window_state().is_open());
                        assert_eq!(commissionable_service(&matter), None);

                        let before = Instant::now();
                        let mut detached = Box::pin(control.open());
                        assert!(poll_once(detached.as_mut()).await.is_none());
                        drop(detached);
                        let reopened = control.status().await.unwrap();
                        assert_eq!(control.open().await.unwrap(), reopened);
                        let PairingStatus::Open {
                            expires_at: new_deadline,
                            ..
                        } = reopened
                        else {
                            panic!("expected the reopened Matter window");
                        };
                        assert!(new_deadline > expires_at);
                        assert!(new_deadline >= before + Duration::from_secs(900));
                        let (new_id, new_discriminator) = commissionable_service(&matter).unwrap();
                        assert_ne!(new_id, first_id);
                        assert_eq!(new_discriminator, first_discriminator);
                        sample.set(Some(expires_at));
                        assert_eq!(control.status().await.unwrap(), reopened);
                        assert!(matches!(
                            events.borrow().as_slice(),
                            [PairingEvent::Opened { .. }, PairingEvent::Expired]
                        ));

                        matter.with_state(|state| {
                            state.fabrics.add_with_post_init(|_| Ok(())).unwrap();
                        });
                        assert!(matter.comm_window_state().is_open());
                        assert_eq!(control.status().await.unwrap(), PairingStatus::Paired);
                        assert_eq!(control.open().await.unwrap(), PairingStatus::Paired);
                    },
                    async { panic!("pairing service ended: {:?}", service.as_mut().await) },
                )
                .await;
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

fn commissionable_service(matter: &Matter<'_>) -> Option<(u64, u16)> {
    let mut result = None;
    matter
        .mdns_services(|service| {
            if let MatterLocalService::Commissionable {
                id, discriminator, ..
            } = service
            {
                assert!(result.replace((id, discriminator)).is_none());
            }
            Ok(())
        })
        .unwrap();
    result
}

struct NoLabels;

impl LabelWriter for NoLabels {
    fn write_label(&self, _: &DeviceKey, _: &str) -> Result<(), StorageError> {
        unreachable!()
    }

    fn relay_label(&self, _: &Scope, _: &FunctionKey) -> Option<String> {
        unreachable!()
    }

    fn write_relay_label(&self, _: &Scope, _: &FunctionKey, _: &str) -> Result<(), StorageError> {
        unreachable!()
    }
}
