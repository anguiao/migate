use super::*;
use crate::xiaomi::{
    mijia::{MijiaClient, MijiaSession},
    tests::support::ReceivedRequest,
};
use futures_lite::future::poll_once;
use std::sync::mpsc::Receiver;

async fn received(requests: &Receiver<ReceivedRequest>) -> ReceivedRequest {
    future::or(
        async {
            loop {
                if let Ok(request) = requests.try_recv() {
                    return request;
                }
                async_io::Timer::after(Duration::from_millis(1)).await;
            }
        },
        async {
            async_io::Timer::after(Duration::from_secs(1)).await;
            panic!("expected a request");
        },
    )
    .await
}

fn commit_replacement(store: &Store, original: &XiaomiRecord) -> XiaomiRecord {
    let mut replacement = original.clone();
    replacement.ha.authorization.tokens.access_token = "new-login-access".into();
    replacement.ha.authorization.tokens.refresh_token = "new-login-refresh".into();
    replacement.mijia = MijiaSession::new(
        original.uid.clone(),
        "new-login-service".into(),
        "c2VjdXJpdHk=".into(),
        "test-agent".into(),
    )
    .unwrap();
    replacement.recovery.credentials = AccountCredentials::new(
        original.uid.clone(),
        "new-login-cuser".into(),
        "new-login-pass".into(),
        original.recovery.credentials.identity().clone(),
    )
    .unwrap();
    store
        .xiaomi()
        .commit_login(
            &replacement.recovery.credentials,
            &replacement.mijia,
            &replacement.ha.client_uuid,
            &replacement.ha.authorization,
        )
        .unwrap();
    replacement
}

#[test]
fn check_shares_background_refresh_even_when_it_finishes_during_mijia_check() {
    for (status, refresh_at) in [(200, NOW), (200, NOW + 500), (401, NOW), (503, NOW)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", refresh_at, NOW + 60);
        store.xiaomi().fixture_replace(&original).unwrap();
        let snapshot = store.xiaomi().snapshot().unwrap();

        let (release_refresh, refresh_gate) = flume::bounded(1);
        let mut first = true;
        let (base, requests) = dynamic_mock_server(8, move |request| {
            if request.target.contains("get_token") {
                let response =
                    MockResponse::json(status, &token_body("fresh-access", "fresh-refresh"));
                if std::mem::take(&mut first) {
                    response.gated(refresh_gate.clone())
                } else {
                    response
                }
            } else if request.target.ends_with("gethome") {
                MockResponse::json(200, &home_body(Some("uid"), &[]))
            } else {
                assert!(request.target.ends_with("get_central_crt"));
                certificate_response(request)
            }
        });
        let (release_mijia, mijia_gate) = flume::bounded(1);
        let (mijia_base, mijia_requests) = dynamic_mock_server(1, move |_| {
            MockResponse::json(200, &json!({"code":0,"result":{"homelist":[{"uid":"uid","id":"1","dids":[],"roomlist":[]}],"has_more":false}}).to_string())
                .gated(mijia_gate.clone())
        });

        let cloud = CloudClient::for_test(&base, Duration::from_secs(2)).unwrap();
        let background = AuthService::new(store.xiaomi(), cloud.clone()).unwrap();
        let check = AuthService {
            store: store.xiaomi(),
            cloud,
            account: AccountClient::new(),
            mijia: MijiaClient::for_test(&mijia_base, Duration::from_secs(2)),
            now,
        };

        let report = block_on(async {
            let mut refreshing = Box::pin(background.refresh_bridge_tokens(
                &original,
                snapshot.ha_generation().unwrap(),
                || NOW,
            ));
            assert!(poll_once(refreshing.as_mut()).await.is_none());
            assert!(received(&requests).await.target.contains("get_token"));

            let mut checking = Box::pin(check.check());
            assert!(poll_once(checking.as_mut()).await.is_none());
            received(&mijia_requests).await;

            release_refresh.send(()).unwrap();
            assert_eq!(refreshing.await.is_ok(), status == 200);
            release_mijia.send(()).unwrap();
            checking.await.unwrap()
        });

        assert!(matches!(report.mijia, AuthenticationState::Authenticated));
        let rest: Vec<_> = requests.try_iter().collect();
        assert!(
            rest.iter()
                .all(|request| !request.target.contains("get_token")),
            "duplicated refresh for HTTP {status} and refresh deadline {refresh_at}"
        );
        let saved = store.xiaomi().load().unwrap().unwrap();
        match status {
            200 => {
                assert!(report.is_success(), "{report:?}");
                assert_eq!(rest.len(), 2);
                assert!(rest.iter().all(|request| {
                    request
                        .headers
                        .to_ascii_lowercase()
                        .contains("authorization: bearerfresh-access")
                }));
                assert_eq!(saved.ha.authorization.tokens.refresh_token, "fresh-refresh");
            }
            401 => {
                assert!(matches!(
                    report.ha,
                    AuthenticationState::SignInRequired(FailureReason::Cloud(error)) if error.is_unauthorized()
                ));
                assert!(saved.ha.stopped);
                assert!(rest.is_empty());
            }
            503 => {
                assert!(matches!(
                    report.ha,
                    AuthenticationState::Unavailable(FailureReason::Cloud(error)) if error.http_status() == Some(503)
                ));
                assert_eq!(saved, original);
                assert!(rest.is_empty());
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn a_check_uses_rotated_tokens_during_a_home_request_but_never_adopts_a_new_login() {
    for new_login in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", NOW + 500, NOW + 1_000_000);
        store.xiaomi().fixture_replace(&original).unwrap();
        let generation = store.xiaomi().snapshot().unwrap().ha_generation().unwrap();
        let (release, gate) = flume::bounded(1);
        let (base, requests) = dynamic_mock_server(4, move |request| {
            if request.target.contains("get_token") {
                return MockResponse::json(200, &token_body("fresh-access", "fresh-refresh"));
            }
            assert!(request.target.ends_with("gethome"));
            let home = MockResponse::json(200, &home_body(Some("uid"), &[]));
            if request
                .headers
                .to_ascii_lowercase()
                .contains("bearerold-access")
            {
                home.gated(gate.clone())
            } else {
                home
            }
        });
        let auth = AuthService::with_clock(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_secs(2)).unwrap(),
            now,
        );
        let (result, ()) = block_on(future::zip(auth.check(), async {
            let home = received(&requests).await;
            assert!(home.target.ends_with("gethome"));
            assert!(
                home.headers
                    .to_ascii_lowercase()
                    .contains("bearerold-access")
            );
            if new_login {
                commit_replacement(&store, &original);
            } else {
                auth.refresh_bridge_tokens(&original, generation, || NOW)
                    .await
                    .unwrap();
            }
            release.send(()).unwrap();
        }));
        let rest: Vec<_> = requests.try_iter().collect();
        if new_login {
            assert!(matches!(result, Err(AuthError::Missing(_))), "{result:?}");
            assert!(rest.is_empty());
        } else {
            let report = result.unwrap();
            assert!(report.is_success(), "{report:?}");
            assert_eq!(rest.len(), 2);
            assert!(rest[0].target.contains("get_token"));
            assert!(rest[1].target.ends_with("gethome"));
            assert!(
                rest[1]
                    .headers
                    .to_ascii_lowercase()
                    .contains("bearerfresh-access")
            );
        }
    }
}

#[test]
fn late_bridge_refresh_cannot_update_the_next_login_or_restore_logged_out_credentials() {
    for (status, relogin) in [(200, true), (401, true), (200, false), (401, false)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", NOW, NOW + 1_000_000);
        store.xiaomi().fixture_replace(&original).unwrap();
        let snapshot = store.xiaomi().snapshot().unwrap();
        let (release, gate) = flume::bounded(1);
        let (base, requests) = dynamic_mock_server(1, move |_| {
            MockResponse::json(status, &token_body("old-refreshed", "old-rotated"))
                .gated(gate.clone())
        });
        let auth = AuthService::new(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_secs(2)).unwrap(),
        )
        .unwrap();

        block_on(async {
            let mut pending = Box::pin(auth.refresh_bridge_tokens(
                &original,
                snapshot.ha_generation().unwrap(),
                || NOW,
            ));
            assert!(poll_once(pending.as_mut()).await.is_none());
            assert!(received(&requests).await.target.contains("get_token"));
            let expected = if relogin {
                Some(commit_replacement(&store, &original))
            } else {
                store.xiaomi().logout().unwrap();
                None
            };
            release.send(()).unwrap();
            let result = pending.await;
            assert_eq!(store.xiaomi().load().unwrap(), expected);
            assert_eq!(auth.saved_bridge_credentials().unwrap(), expected);
            assert!(matches!(result, Err(AuthError::Missing(_))), "{result:?}");
        });
    }
}

#[test]
fn late_mijia_recovery_cannot_update_the_next_login_or_restore_logged_out_credentials() {
    for (rejected, relogin) in [(false, true), (true, true), (false, false), (true, false)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", NOW, NOW + 1_000_000);
        store.xiaomi().fixture_replace(&original).unwrap();
        let snapshot = store.xiaomi().snapshot().unwrap();
        let (release, gate) = flume::bounded(1);
        let account_gate = gate.clone();
        let (account_base, requests) = dynamic_mock_server(2, move |request| {
            if request.target.starts_with("/sts?") {
                return MockResponse::raw(
                    "HTTP/1.1 200 Test\r\nSet-Cookie: serviceToken=restored-service; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
            if rejected {
                MockResponse::json(200, r#"{"code":70002}"#).gated(account_gate.clone())
            } else {
                MockResponse::json(200, &json!({"code":0,"userId":"uid","cUserId":"restored-cuser","passToken":"restored-pass","ssecurity":"bmV3LXNlY3VyaXR5","location":"https://sts.api.io.mi.com/sts?ticket=secret"}).to_string())
            }
        });
        let (mijia_base, mijia_requests) = dynamic_mock_server(1, move |_| {
            MockResponse::json(200, &json!({"code":0,"result":{"homelist":[{"uid":"uid","id":"1","dids":[],"roomlist":[]}],"has_more":false}}).to_string()).gated(gate.clone())
        });
        let auth = AuthService {
            store: store.xiaomi(),
            cloud: CloudClient::for_test(&account_base, Duration::from_secs(2)).unwrap(),
            account: AccountClient::for_test(&account_base, Duration::from_secs(2)),
            mijia: MijiaClient::for_test(&mijia_base, Duration::from_secs(2)),
            now,
        };

        block_on(async {
            let mut pending =
                Box::pin(auth.recover_mijia(&original.uid, snapshot.mijia_generation().unwrap()));
            assert!(poll_once(pending.as_mut()).await.is_none());
            future::or(
                async {
                    panic!(
                        "recovery completed before the response was released: {:?}",
                        pending.as_mut().await
                    );
                },
                async {
                    received(&requests).await;
                    if !rejected {
                        assert!(received(&requests).await.target.starts_with("/sts?"));
                        received(&mijia_requests).await;
                    }
                },
            )
            .await;
            let expected = if relogin {
                Some(commit_replacement(&store, &original))
            } else {
                store.xiaomi().logout().unwrap();
                None
            };
            release.send(()).unwrap();
            let result = pending.await;
            assert_eq!(store.xiaomi().load().unwrap(), expected);
            assert_eq!(auth.saved_bridge_credentials().unwrap(), expected);
            assert!(matches!(result, Err(AuthError::Missing(_))), "{result:?}");
        });
    }
}

#[test]
fn an_online_check_adopts_background_certificates_saved_before_or_during_renewal() {
    for (hold_home, status) in [(false, 503), (false, 200), (false, 401), (true, 200)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut original = valid_record("uid", "access", NOW + 500, NOW + 60);
        let short = original
            .hub
            .as_mut()
            .unwrap()
            .certificate_pem
            .take()
            .unwrap();
        store.xiaomi().fixture_replace(&original).unwrap();
        let generation = store.xiaomi().snapshot().unwrap().ha_generation().unwrap();
        let (release, gate) = flume::bounded(1);
        let mut delayed = Some(if status == 200 {
            MockResponse::json(200, &json!({"code":0,"result":{"cert":short}}).to_string())
        } else {
            MockResponse::json(status, "certificate request failed")
        });
        let (base, requests) = dynamic_mock_server(3, move |request| {
            if request.target.ends_with("gethome") {
                let response = MockResponse::json(200, &home_body(Some("uid"), &[]));
                return if hold_home {
                    response.gated(gate.clone())
                } else {
                    response
                };
            }
            assert!(request.target.ends_with("get_central_crt"));
            if !hold_home && let Some(response) = delayed.take() {
                return response.gated(gate.clone());
            }
            certificate_response(request)
        });
        let auth = AuthService::with_clock(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_secs(2)).unwrap(),
            now,
        );
        let (result, renewed) = block_on(future::zip(auth.check(), async {
            assert!(received(&requests).await.target.ends_with("gethome"));
            if !hold_home {
                assert!(
                    received(&requests)
                        .await
                        .target
                        .ends_with("get_central_crt")
                );
            }
            let renewed = auth
                .renew_bridge_certificate(&original, generation, || NOW)
                .await
                .unwrap();
            release.send(()).unwrap();
            renewed
        }));
        let report = result.unwrap();
        assert!(report.is_success(), "HTTP {status}: {report:?}");
        assert_eq!(report.certificate, renewed.validity);
        assert_eq!(report.certificate_update, CertificateUpdate::NotNeeded);
        assert_eq!(
            auth.saved_bridge_credentials().unwrap(),
            Some(renewed.credentials)
        );
        assert_eq!(
            requests.try_iter().count(),
            1,
            "only the background request remains"
        );
    }
}

#[test]
fn late_certificate_renewal_cannot_change_the_next_login_or_recreate_a_hub_after_logout() {
    for relogin in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", NOW, NOW + 60);
        store.xiaomi().fixture_replace(&original).unwrap();
        let generation = store.xiaomi().snapshot().unwrap().ha_generation().unwrap();
        let (release, gate) = flume::bounded(1);
        let (base, requests) = dynamic_mock_server(1, move |request| {
            certificate_response(request).gated(gate.clone())
        });
        let auth = AuthService::new(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_secs(2)).unwrap(),
        )
        .unwrap();

        block_on(async {
            let mut pending =
                Box::pin(auth.renew_bridge_certificate(&original, generation, || NOW));
            assert!(poll_once(pending.as_mut()).await.is_none());
            assert!(
                received(&requests)
                    .await
                    .target
                    .ends_with("get_central_crt")
            );
            let expected = if relogin {
                Some(commit_replacement(&store, &original))
            } else {
                store.xiaomi().logout().unwrap();
                None
            };
            release.send(()).unwrap();
            assert!(matches!(pending.await, Err(AuthError::Missing(_))));
            assert_eq!(store.xiaomi().load().unwrap(), expected);
            assert_eq!(auth.saved_bridge_credentials().unwrap(), expected);
        });
    }
}
