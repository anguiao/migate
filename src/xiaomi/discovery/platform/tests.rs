use super::*;

#[test]
fn native_interface_collection_identifies_loopback_without_trusting_it() {
    let records = collect_interface_records().unwrap();
    assert!(records.iter().any(|record| record.loopback));
    assert!(
        records
            .iter()
            .filter(|record| record.loopback)
            .all(|record| !record.physical)
    );
}

#[test]
fn native_default_route_collection_runs_even_without_a_physical_lan() {
    let records = collect_interface_records().unwrap();
    let route = futures_lite::future::block_on(collect_default_route(
        &records,
        std::time::Duration::from_secs(5),
    ))
    .unwrap();
    if let Some(route) = route {
        assert!(
            records
                .iter()
                .any(|record| record.index == route.interface_index)
        );
    }
}
