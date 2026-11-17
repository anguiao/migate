use super::{
    basic_info,
    support::{Context, model_handler, single_entry},
};
use crate::{matter::storage::StoreAdapter, storage::Store};
use futures_lite::future::block_on;
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, ReadReplyInstance,
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
    },
    error::ErrorCode,
    im::{InteractionModel, InteractionModelState},
    tlv::Nullable,
    transport::exchange::MatterBuffers,
    utils::storage::WriteBuf,
};
use std::sync::Mutex;

struct Capture(Mutex<Vec<(std::thread::ThreadId, String)>>);

impl log::Log for Capture {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() == log::Level::Error && metadata.target().ends_with("matter::model")
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            self.0
                .lock()
                .unwrap()
                .push((std::thread::current().id(), record.args().to_string()));
        }
    }

    fn flush(&self) {}
}

static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));

#[test]
fn failed_reads_report_the_path_without_reporting_values_or_buffer_retries() {
    const CHILD: &str = "MIGATE_TEST_MATTER_DIAGNOSTICS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "matter::tests::diagnostics::failed_reads_report_the_path_without_reporting_values_or_buffer_retries",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
            "child test did not execute exactly one assertion suite:\n{stdout}"
        );
        return;
    }
    log::set_logger(&CAPTURE).unwrap();
    log::set_max_level(log::LevelFilter::Error);
    let thread = std::thread::current().id();
    let records = || {
        CAPTURE
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == thread)
            .map(|(_, message)| message.clone())
            .collect::<Vec<_>>()
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let (device, _receiver) = super::test_device();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState = InteractionModelState::new();
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(StoreAdapter::new(store.matter()));
    let handler = model_handler(vec![single_entry(&device)]);
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let ctx = Context::new(&im, 6, 0);

    block_on(async {
        let mut buffer = [0; 128];
        let mut write = WriteBuf::new(&mut buffer);
        assert_eq!(
            handler
                .read(&ctx, ReadReplyInstance::new(&ctx.attribute, &mut write))
                .await
                .unwrap_err()
                .code(),
            ErrorCode::Failure
        );
        assert_eq!(
            records(),
            [
                "Matter attribute read failed: endpoint=2, cluster=6, attribute=0, stage=handler, error=Failure"
            ]
        );

        device.functions()[0].observe(false);
        let mut buffer = [];
        let mut write = WriteBuf::new(&mut buffer);
        assert_eq!(
            handler
                .read(&ctx, ReadReplyInstance::new(&ctx.attribute, &mut write))
                .await
                .unwrap_err()
                .code(),
            ErrorCode::NoSpace
        );
        let mut buffer = [0; 128];
        let mut write = WriteBuf::new(&mut buffer);
        handler
            .read(&ctx, ReadReplyInstance::new(&ctx.attribute, &mut write))
            .await
            .unwrap();
        assert_eq!(records().len(), 1);

        let mut array_ctx = Context::at(&im, 2, 29, 0);
        array_ctx.attribute.array = true;
        array_ctx.attribute.list_chunked = true;
        array_ctx.attribute.list_index = Some(Nullable::some(100));
        let mut buffer = [0; 128];
        let mut write = WriteBuf::new(&mut buffer);
        assert_eq!(
            handler
                .read(
                    &array_ctx,
                    ReadReplyInstance::new(&array_ctx.attribute, &mut write)
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::ConstraintError
        );
        assert_eq!(records().len(), 1, "array end is an internal signal");

        array_ctx.attribute.list_chunked = false;
        let mut buffer = [0; 128];
        let mut write = WriteBuf::new(&mut buffer);
        assert_eq!(
            handler
                .read(
                    &array_ctx,
                    ReadReplyInstance::new(&array_ctx.attribute, &mut write)
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::ConstraintError
        );
        assert_eq!(
            records()[1],
            "Matter attribute read failed: endpoint=2, cluster=29, attribute=0, stage=handler, error=ConstraintError"
        );
    });
}
