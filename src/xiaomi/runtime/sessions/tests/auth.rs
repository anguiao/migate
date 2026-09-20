use super::*;

#[test]
fn terminal_help_reads_the_current_runtime_auth_report_each_time() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let runtime = XiaomiRuntime::new(store.clone()).unwrap();
    let service = runtime.service();
    let render = || {
        block_on(crate::terminal::bridge::handle_line(
            &service,
            &store.devices(),
            &runtime,
            "/bin/migate".as_ref(),
            directory.path(),
            "help",
            0,
        ))
        .unwrap()
        .unwrap()
    };

    assert!(render().contains("Xiaomi: not signed in (cn)."));
    runtime.replace_auth_report(crate::xiaomi::auth::AuthReport::for_test(
        crate::xiaomi::auth::AuthenticationState::Checking,
        None,
        crate::xiaomi::auth::CertificateUpdate::NotNeeded,
        0,
    ));
    assert!(render().contains("Xiaomi: checking authentication (cn)..."));
}

#[test]
fn an_auth_refresh_cas_is_rechecked_at_its_committed_revision() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let identity = crate::xiaomi::certificate::ClientIdentity::generate("10001").unwrap();
    let certificate = crate::xiaomi::test_support::sign_csr(
        &identity.csr_pem,
        unix_time() - 60,
        unix_time() + 30 * 24 * 60 * 60,
    )
    .unwrap();
    store
        .xiaomi()
        .replace(&crate::storage::XiaomiRecord {
            uid: "10001".into(),
            region: "cn".into(),
            oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
            redirect_uri:
                "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                    .into(),
            tokens: crate::storage::TokenSet {
                access_token: "old-access".into(),
                refresh_token: "old-refresh".into(),
                expires_at: unix_time() + 1000,
                refresh_at: unix_time() - 1,
            },
            virtual_did: identity.virtual_did,
            private_key_pem: identity.private_key_pem,
            certificate_pem: certificate,
        })
        .unwrap();
    let (base, requests) = crate::xiaomi::test_support::mock_server(vec![
        crate::xiaomi::test_support::MockResponse::json(
            200,
            r#"{"code":0,"result":{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":1000}}"#,
        ),
        crate::xiaomi::test_support::MockResponse::json(
            200,
            r#"{"code":0,"result":{"homelist":[{"uid":"10001","dids":[]}]}}"#,
        ),
        crate::xiaomi::test_support::MockResponse::json(
            200,
            r#"{"code":0,"result":{"homelist":[{"uid":"10001","dids":[]}]}}"#,
        ),
    ]);
    let runtime = XiaomiRuntime::new(store.clone()).unwrap();
    {
        let mut holder = runtime.inner.runner.borrow_mut();
        let runner = holder.as_mut().unwrap();
        runner.auth.set_service(Rc::new(AuthService::new(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_millis(500)).unwrap(),
        )));
        runner.auth.defer_until(Instant::now());
        runner
            .catalog_refresh
            .defer_until(Instant::now() + Duration::from_secs(60));
        runner
            .discovery
            .defer_network_until(Instant::now() + Duration::from_secs(60));
        runner.discovery.disable_monitor();
        runtime.inner.refresh_requested.set(false);
    }
    block_on(future::or(
        async {
            future::or(async { runtime.run().await.unwrap() }, async {
                loop {
                    let stored = store.xiaomi().load().unwrap().unwrap();
                    if stored.tokens.access_token == "fresh-access"
                        && runtime.auth_report().is_success()
                    {
                        break;
                    }
                    Timer::after(Duration::from_millis(10)).await;
                }
                runtime.stop();
            })
            .await;
        },
        async {
            Timer::after(Duration::from_secs(2)).await;
            panic!(
                "runtime did not recheck the revision committed by auth refresh: report={:?}, stored={:?}, requests={:?}",
                runtime.auth_report(),
                store
                    .xiaomi()
                    .load()
                    .unwrap()
                    .map(|record| record.tokens.access_token),
                requests
                    .try_iter()
                    .map(|request| request.target)
                    .collect::<Vec<_>>()
            )
        },
    ));
    assert!(runtime.auth_report().is_success());
    assert_eq!(
        store.xiaomi().load().unwrap().unwrap().tokens.access_token,
        "fresh-access"
    );
}

#[test]
fn a_late_old_session_auth_response_cannot_replace_a_new_login() {
    fn auth_record(uid: &str, access: &str, refresh_at: i64) -> crate::storage::XiaomiRecord {
        let identity = crate::xiaomi::certificate::ClientIdentity::generate(uid).unwrap();
        let certificate = crate::xiaomi::test_support::sign_csr(
            &identity.csr_pem,
            unix_time() - 60,
            unix_time() + 30 * 24 * 60 * 60,
        )
        .unwrap();
        crate::storage::XiaomiRecord {
            uid: uid.into(),
            region: "cn".into(),
            oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
            redirect_uri:
                "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                    .into(),
            tokens: crate::storage::TokenSet {
                access_token: access.into(),
                refresh_token: format!("{uid}-refresh"),
                expires_at: unix_time() + 1000,
                refresh_at,
            },
            virtual_did: identity.virtual_did,
            private_key_pem: identity.private_key_pem,
            certificate_pem: certificate,
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store
        .xiaomi()
        .replace(&auth_record("10001", "old-access", unix_time() - 1))
        .unwrap();
    let (base, requests) = crate::xiaomi::test_support::mock_server(vec![
        crate::xiaomi::test_support::MockResponse::json(
            200,
            r#"{"code":0,"result":{"access_token":"late-old-access","refresh_token":"late-old-refresh","expires_in":1000}}"#,
        )
        .delayed(Duration::from_millis(200)),
        crate::xiaomi::test_support::MockResponse::json(
            200,
            r#"{"code":0,"result":{"homelist":[{"uid":"20002","dids":[]}]}}"#,
        ),
    ]);
    let runtime = XiaomiRuntime::new(store.clone()).unwrap();
    {
        let mut holder = runtime.inner.runner.borrow_mut();
        let runner = holder.as_mut().unwrap();
        runner.auth.set_service(Rc::new(AuthService::new(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_millis(500)).unwrap(),
        )));
        runner.auth.defer_until(Instant::now());
        runner
            .catalog_refresh
            .defer_until(Instant::now() + Duration::from_secs(60));
        runner
            .discovery
            .defer_network_until(Instant::now() + Duration::from_secs(60));
        runner.discovery.disable_monitor();
        runtime.inner.refresh_requested.set(false);
    }
    let assertions_completed = Rc::new(Cell::new(false));
    let completed = assertions_completed.clone();
    block_on(future::or(
        async {
            future::or(async { runtime.run().await.unwrap() }, async {
                blocking::unblock(move || requests.recv_timeout(Duration::from_secs(1)).unwrap())
                    .await;
                let other = Store::open(directory.path()).unwrap();
                other.xiaomi().logout().unwrap();
                other
                    .xiaomi()
                    .replace(&auth_record("20002", "new-access", unix_time() + 500))
                    .unwrap();
                loop {
                    let current = store.xiaomi().load().unwrap().unwrap();
                    if current.uid == "20002"
                        && current.tokens.access_token == "new-access"
                        && runtime.auth_report().is_success()
                    {
                        break;
                    }
                    Timer::after(Duration::from_millis(10)).await;
                }
                Timer::after(Duration::from_millis(250)).await;
                let current = store.xiaomi().load().unwrap().unwrap();
                assert_eq!(current.uid, "20002");
                assert_eq!(current.tokens.access_token, "new-access");
                completed.set(true);
                runtime.stop();
            })
            .await;
        },
        async {
            Timer::after(Duration::from_secs(2)).await;
            panic!("late old-session auth response was not isolated")
        },
    ));
    assert!(
        assertions_completed.get(),
        "runtime stopped before the stale auth assertions completed"
    );
}
