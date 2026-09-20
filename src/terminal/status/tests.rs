use super::*;
use crate::xiaomi::auth::AuthReport;

#[test]
fn certificate_dates_are_recomputed_for_each_render() {
    let report = AuthReport::for_test(
        AuthenticationState::Authenticated,
        Some(CertificateValidity {
            not_before: 1_700_000_000,
            not_after: 1_700_259_201,
        }),
        CertificateUpdate::NotNeeded,
        1_700_000_000,
    );
    assert_eq!(
        format_report(
            &report,
            1_700_000_000,
            Path::new("/bin/migate"),
            Path::new("/data")
        ),
        "Xiaomi: authenticated (cn).\nGateway certificate: valid until 2023-11-17T22:13:21Z."
    );
    assert_eq!(
        format_report(
            &report,
            1_700_000_001,
            Path::new("/bin/migate"),
            Path::new("/data")
        ),
        "Xiaomi: authenticated (cn).\nGateway certificate: valid until 2023-11-17T22:13:21Z; renewal due."
    );
}
