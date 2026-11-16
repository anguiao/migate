use super::*;
use crate::xiaomi::certificate::{CertificateStatus, ClientIdentity};

fn stopped_ha_rig(not_before: i64, not_after: i64) -> Rig {
    let mut rig = Rig::new(true, &[]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    let hub = record.hub.as_ref().unwrap();
    let identity =
        ClientIdentity::from_private_key(&record.uid, &hub.virtual_did, &hub.private_key_pem)
            .unwrap();
    record.hub.as_mut().unwrap().certificate_pem = Some(
        crate::xiaomi::tests::support::sign_csr(
            &identity.csr_pem,
            rig.clock.wall() + not_before,
            rig.clock.wall() + not_after,
        )
        .unwrap(),
    );
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.store.xiaomi().stop_ha_refresh().unwrap();
    (rig.source, rig.ready) = rig.reopen_adapter();
    rig
}

fn account_requests(c: &Control) -> (usize, usize, usize, usize, usize) {
    let cloud = c.cloud.lock().unwrap();
    (
        cloud.homes,
        cloud.ha_homes,
        cloud.certs,
        cloud.tokens,
        cloud.restores,
    )
}

async fn settled(c: &Control, reports: &RefCell<Vec<AdapterStatus>>, phase: CertificateStatus) {
    until(|| {
        c.context.directory.borrow().scope_valid()
            && reports
                .borrow()
                .last()
                .is_some_and(|status| status.certificate_status == Some(phase))
    })
    .await;
    async_io::Timer::after(Duration::from_millis(20)).await;
    assert!(!c.context.token_usable());
    assert!(c.context.diagnostics().devices.is_empty());
}

async fn publish_phase(
    c: &Control,
    reports: &RefCell<Vec<AdapterStatus>>,
    by: u64,
    phase: CertificateStatus,
    completed_at: i64,
) {
    c.clock.advance(Duration::from_secs(by));
    let instant = c.context.diagnostics();
    assert_eq!(instant.certificate_status, Some(phase));
    assert_eq!(instant.auth.completed_at(), completed_at);
    until(|| reports.borrow().contains(&instant)).await;
    for _ in 0..10 {
        future::yield_now().await;
    }
    assert_eq!(
        reports
            .borrow()
            .iter()
            .filter(|status| **status == instant)
            .count(),
        1
    );
}

#[test]
fn stopped_ha_publishes_the_renewal_phase_without_devices_or_account_requests() {
    let rig = stopped_ha_rig(-60, 3 * 24 * 60 * 60 + 5);
    let reports = Rc::new(RefCell::new(Vec::<AdapterStatus>::new()));
    let published = reports.clone();
    block_on(rig.run_with_status(
        move |status| published.borrow_mut().push(status),
        |c| async move {
            settled(&c, &reports, CertificateStatus::Valid).await;
            let completed_at = c.context.diagnostics().auth.completed_at();
            let requests = account_requests(&c);
            let changed = c.context.changed.listen();
            let refreshed = c.context.refresh_changed.listen();
            let credentials = c.context.credentials_changed.listen();
            publish_phase(&c, &reports, 5, CertificateStatus::RenewalDue, completed_at).await;
            assert_eq!(account_requests(&c), requests);
            assert!(poll_once(changed).await.is_none());
            assert!(poll_once(refreshed).await.is_none());
            assert!(poll_once(credentials).await.is_none());
        },
    ));
}

#[test]
fn stopped_ha_publishes_certificate_activation_and_expiry_once_without_account_requests() {
    let rig = stopped_ha_rig(5, 10);
    let reports = Rc::new(RefCell::new(Vec::<AdapterStatus>::new()));
    let published = reports.clone();
    block_on(rig.run_with_status(
        move |status| published.borrow_mut().push(status),
        |c| async move {
            settled(&c, &reports, CertificateStatus::NotYetValid).await;
            let completed_at = c.context.diagnostics().auth.completed_at();
            let requests = account_requests(&c);
            let changed = c.context.changed.listen();
            let refreshed = c.context.refresh_changed.listen();
            let credentials = c.context.credentials_changed.listen();
            publish_phase(&c, &reports, 5, CertificateStatus::RenewalDue, completed_at).await;
            assert!(poll_once(changed).await.is_some());
            assert!(poll_once(refreshed).await.is_some());
            assert!(poll_once(credentials).await.is_some());
            publish_phase(&c, &reports, 5, CertificateStatus::Expired, completed_at).await;
            assert_eq!(account_requests(&c), requests);
        },
    ));
}
