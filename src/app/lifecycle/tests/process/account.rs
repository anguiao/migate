use super::{advertisement::store_test_fabric, matter_address};
use crate::{
    app::{
        App, BridgeOptions,
        lifecycle::account::BridgeAccount,
        tests::support::{outlet, status},
    },
    configuration::Scope,
    device::{
        Availability, Command, CommandError, DeviceKey, DeviceRequest, PhysicalDeviceDescription,
        Power,
    },
    matter::{PairingEvent, tests::support::running_parts},
    storage::{InstanceLock, Store, XiaomiStore, tests::xiaomi_credentials},
    terminal::{
        bridge::Management,
        management::{Client, Listener},
    },
    xiaomi::adapter::tests::app_source::AppSource,
};
use event_listener::Event;
use futures_lite::{
    future::{self, block_on, poll_once},
    io::Cursor,
};
use std::{
    cell::{Cell, RefCell},
    net::UdpSocket,
    pin::pin,
    rc::Rc,
    time::Duration,
};

const FIRST_UID: &str = "account";
const SECOND_UID: &str = "next-account";
const OLD_OUTLET: &str = "test-outlet";
const NEXT_OUTLET: &str = "next-outlet";
const FABRIC_KEY: u16 = rs_matter::persist::FABRIC_KEYS_START + 1;

#[test]
fn account_recovery_keeps_the_management_connection_and_uses_the_new_matter_window() {
    exercise_account_changes(false);
}

#[test]
fn logout_and_account_switch_release_matter_and_restore_the_same_home_pairing() {
    exercise_account_changes(true);
}

fn exercise_account_changes(paired: bool) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || block_on(account_changes(paired)))
        .unwrap()
        .join()
        .unwrap();
}

async fn account_changes(paired: bool) {
    let directory = tempfile::tempdir().unwrap();
    let instance = InstanceLock::acquire(directory.path()).unwrap();
    let listener = Listener::bind(&instance).unwrap();
    let store = Store::open(directory.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let status = status(&store);
    let xiaomi = store.xiaomi();
    if paired {
        store_test_fabric(&store);
    }
    let fabric = store.matter().get(FABRIC_KEY).unwrap();
    let port = UdpSocket::bind(matter_address(0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let options = BridgeOptions {
        store: &store,
        identity: &identity,
        status: &status,
        port,
    };
    let management = Management::new(&status);
    let account = BridgeAccount::new(xiaomi.clone(), &management).unwrap();
    let scope_confirmed = Event::new();
    let finished = Event::new();
    let stopping = finished.listen();
    let active = RefCell::new(None);
    let starts = Cell::new(0);
    let shutdowns = Cell::new(0);
    let opens = RefCell::new(Vec::new());
    let (send, sent) = async_channel::unbounded();
    let (respond, response) = async_channel::unbounded();
    let bridge = account.run(
        options,
        || {
            let uid = xiaomi.current_snapshot().record.unwrap().uid;
            let mut source = if uid == FIRST_UID {
                AppSource::new(&store, Some(outlet()))
            } else {
                AppSource::from_account(&store)
            };
            let late_read = source.physical.as_ref().map(|device| {
                source.confirm_all();
                source.read_power(false);
                device.set_availability(Availability::Available);
                source.power_read(OLD_OUTLET, true)
            });
            let handle = source.handle.clone();
            let receiver = source.receivers.pop();
            status.update_adapter(handle.diagnostics());
            starts.set(starts.get() + 1);
            *active.borrow_mut() = Some((handle.clone(), source.physical.clone(), late_read));
            let confirm = scope_confirmed.listen();
            let count = &shutdowns;
            let send = send.clone();
            let response = response.clone();
            let network = async move {
                let _guard = CountDrop(count);
                let commands = async {
                    if let Some(mut receiver) = receiver {
                        while let Some(DeviceRequest::Power(request)) =
                            receiver.next_operation().await
                        {
                            send.try_send(request.target).unwrap();
                            response.recv().await.unwrap();
                            request.accept().unwrap();
                        }
                    }
                    future::pending().await
                };
                let catalog = async {
                    if uid == SECOND_UID {
                        confirm.await;
                        source.scope(Scope {
                            uid,
                            home_id: "new-home".into(),
                        });
                        let new_device = PhysicalDeviceDescription::new(
                            DeviceKey::new("xiaomi", NEXT_OUTLET),
                            "Next Outlet",
                            outlet().functions().to_vec(),
                        )
                        .unwrap();
                        source.describe(new_device);
                        source.confirm_all();
                        source.stage();
                        while source
                            .handle
                            .configuration()
                            .device(&DeviceKey::new("xiaomi", NEXT_OUTLET))
                            .is_none()
                        {
                            future::yield_now().await;
                        }
                        source.power_read(NEXT_OUTLET, false)();
                    }
                    future::pending().await
                };
                future::or(commands, catalog).await
            };
            Ok((handle, async { Ok(()) }, network))
        },
        future::or(
            async {
                stopping.await;
                Ok(())
            },
            management.serve(Cursor::new([]), Vec::new(), Some(&listener)),
        ),
        |event| {
            if let PairingEvent::Opened { qr_payload, .. } = event {
                opens.borrow_mut().push(qr_payload);
            }
            Ok(())
        },
    );

    let controller = async {
        let mut terminal = Client::connect(directory.path()).await.unwrap().unwrap();
        let waiting = (false, "Waiting for Xiaomi sign-in.\n".to_string());
        assert_eq!(command(&mut terminal, &["pairing"]).await, waiting);
        assert_eq!(starts.get(), 0);

        commit_login(&xiaomi, FIRST_UID, "access");
        account.credentials_changed();
        assert_pairing(&mut terminal, &opens, paired, 1).await;
        let old_endpoint = store
            .devices()
            .load_configuration()
            .unwrap()
            .device(outlet().key())
            .unwrap()
            .endpoint;
        let (old_source, old_device, late_read) = active.borrow_mut().take().unwrap();
        let old_app = Rc::new(App::open(old_source.clone(), store.clone()));
        let old_device = old_device.unwrap();

        // A check and a committed same-account login both keep the live instance.
        xiaomi.stop_ha_refresh().unwrap();
        account.credentials_changed();
        assert!(matches!(
            old_source.diagnostics().auth.ha,
            crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
        ));
        commit_login(&xiaomi, FIRST_UID, "updated-access");
        account.credentials_changed();
        assert!(matches!(
            old_source.diagnostics().auth.ha,
            crate::xiaomi::auth::AuthenticationState::Checking
        ));
        assert_pairing(&mut terminal, &opens, paired, 1).await;
        assert_eq!(starts.get(), 1);

        let mut controlling = Client::connect(directory.path()).await.unwrap().unwrap();
        old_device.set_availability(Availability::Available);
        let mut sent_control = pin!(command(&mut controlling, &["on", OLD_OUTLET, "outlet:2"]));
        assert!(
            future::or(
                async { panic!("control ended early: {:?}", sent_control.as_mut().await) },
                async { sent.recv().await.unwrap() }
            )
            .await
        );
        let old_function = &old_device.functions()[0];
        let mut queued = pin!(old_function.execute(Command::Off));
        assert!(poll_once(queued.as_mut()).await.is_none());

        xiaomi.logout().unwrap();
        account.credentials_changed();
        assert!(!old_source.has_scope());
        assert!(
            old_app
                .configuration_access(&old_app.current())
                .unwrap()
                .descriptions
                .is_empty()
        );
        assert!(old_app.rename(outlet().key(), "Late old name").is_err());
        assert!(queued.await.is_err());
        assert!(matches!(
            old_function.execute(Command::On).await,
            Err(CommandError::Cancelled)
        ));
        late_read.unwrap()();
        assert_eq!(old_function.snapshot().power(), Some(Power::Off));
        let mut logout = pin!(account.wait_for_logout());
        assert!(poll_once(logout.as_mut()).await.is_none());
        respond.try_send(()).unwrap();
        logout.await;
        assert!(!sent_control.await.0);
        assert!(sent.try_recv().is_err());
        assert_eq!(shutdowns.get(), 1);
        assert!(UdpSocket::bind(matter_address(port)).is_ok());
        assert_eq!(command(&mut terminal, &["pairing", "open"]).await, waiting);
        assert_eq!(
            command(&mut terminal, &["on", OLD_OUTLET, "outlet:2"]).await,
            waiting
        );

        commit_login(&xiaomi, FIRST_UID, "recovered-access");
        account.credentials_changed();
        assert_pairing(&mut terminal, &opens, paired, 2).await;
        assert!(command(&mut terminal, &["status", OLD_OUTLET]).await.0);
        xiaomi.logout().unwrap();
        account.credentials_changed();
        account.wait_for_logout().await;
        assert_eq!(shutdowns.get(), 2);

        commit_login(&xiaomi, SECOND_UID, "next-access");
        account.credentials_changed();
        let preparing = "Waiting for Xiaomi home confirmation.\n";
        loop {
            let (ok, report) = command(&mut terminal, &["status"]).await;
            assert!(ok && !report.contains(OLD_OUTLET), "{report}");
            if report.starts_with(preparing) {
                break;
            }
            assert!(report.starts_with("Preparing Xiaomi devices."), "{report}");
            future::yield_now().await;
        }
        for words in [&["pairing"][..], &["on", OLD_OUTLET, "outlet:2"][..]] {
            assert_eq!(
                command(&mut terminal, words).await,
                (false, preparing.into())
            );
        }
        scope_confirmed.notify(usize::MAX);
        assert_pairing(&mut terminal, &opens, paired, 3).await;
        let (ok, list) = command(&mut terminal, &["list"]).await;
        assert!(
            ok && list.contains(NEXT_OUTLET) && !list.contains(OLD_OUTLET),
            "{list}"
        );
        assert!(
            !command(&mut terminal, &["on", OLD_OUTLET, "outlet:2"])
                .await
                .0
        );
        if !paired {
            let configuration = store.devices().load_configuration().unwrap();
            let new_endpoint = configuration
                .device(&DeviceKey::new("xiaomi", NEXT_OUTLET))
                .unwrap()
                .endpoint;
            let parts = running_parts(port).await;
            assert!(
                parts.contains(&new_endpoint) && !parts.contains(&old_endpoint),
                "{parts:?}"
            );
        }
        assert_eq!(store.load_identity().unwrap(), identity);
        assert_eq!(store.matter().get(FABRIC_KEY).unwrap(), fabric);
        finished.notify(usize::MAX);
    };
    future::or(
        async {
            future::zip(
                async {
                    if let Err(error) = bridge.await {
                        panic!(
                            "bridge failed after {} starts and {} stops: {error}",
                            starts.get(),
                            shutdowns.get()
                        );
                    }
                },
                controller,
            )
            .await;
        },
        async {
            async_io::Timer::after(Duration::from_secs(20)).await;
            panic!(
                "account changes timed out: {} starts, {} stops",
                starts.get(),
                shutdowns.get()
            );
        },
    )
    .await;
}

struct CountDrop<'a>(&'a Cell<usize>);

impl Drop for CountDrop<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

fn commit_login(store: &XiaomiStore, uid: &str, access_token: &str) {
    let mut record = xiaomi_credentials(uid);
    record.ha.authorization.tokens.access_token = access_token.into();
    store
        .commit_login(
            &record.recovery.credentials,
            &record.mijia,
            &record.ha.client_uuid,
            &record.ha.authorization,
        )
        .unwrap();
}

async fn command(client: &mut Client, words: &[&str]) -> (bool, String) {
    let mut output = Vec::new();
    let success = client.execute(words, &mut output).await.unwrap();
    (success, String::from_utf8(output).unwrap())
}

async fn assert_pairing(
    client: &mut Client,
    opens: &RefCell<Vec<String>>,
    paired: bool,
    starts: usize,
) {
    loop {
        let (ok, current) = command(client, &["pairing"]).await;
        if !ok {
            assert!(
                current.starts_with("Waiting for ")
                    || current.starts_with("Preparing Xiaomi devices.")
                    || current.starts_with("Matter is not ready"),
                "{current}"
            );
            async_io::Timer::after(Duration::from_millis(10)).await;
            continue;
        }
        let (ok, reopened) = command(client, &["pairing", "open"]).await;
        assert!(ok, "{reopened}");
        if paired {
            assert_eq!(current, "MiGate is already paired.\n");
            assert_eq!(reopened, current);
            assert!(opens.borrow().is_empty());
        } else {
            let opened = opens.borrow();
            assert_eq!(opened.len(), starts);
            assert!(current.contains(&opened[starts - 1]), "{current}");
            assert!(reopened.contains(&opened[starts - 1]), "{reopened}");
        }
        break;
    }
}
