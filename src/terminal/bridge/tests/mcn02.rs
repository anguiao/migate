use super::*;
use crate::xiaomi::capability::tests::source::SourceFixture;

#[test]
fn list_and_status_show_the_derived_mcn02_direct_patch() {
    block_on(async {
        let (_directory, store, status, registry, _receivers) = fixture();
        let instance = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/miot/lumi-mcn02-synthetic.json"
        ))
        .unwrap();
        let source = SourceFixture::new("ac", "lumi.acpartner.mcn02", instance);
        let description = source.physical_description("Air conditioner");
        let mut record = crate::storage::tests::fixtures::identity(&description, 30);
        let mut configuration = store.devices().load_configuration().unwrap();
        configuration.descriptions.push(source.raw.clone());
        record.native = Some(source.native);
        configuration.devices.push(record);
        store
            .devices()
            .commit_configuration(&configuration, &[])
            .unwrap();
        let (physical, _receivers) = PhysicalDevice::new(description);
        registry.insert(physical);

        for command in ["list", "status ac"] {
            let output = handle_line(&registry, &store.devices(), &status, command, 0)
                .await
                .unwrap()
                .unwrap();
            assert!(output.contains("air-conditioner:2: core"), "{output}");
            assert!(
                output
                    .contains("Patch lumi.mcn02.legacy-miio: air-conditioner:2 (direct transport)"),
                "{output}"
            );
        }
    });
}
