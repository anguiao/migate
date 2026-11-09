use super::*;

#[test]
fn detailed_cancellation_removes_unsent_frames_without_replay() {
    block_on(async {
        for dispatched in [false, true] {
            let (handle, mut driver, budget, written) = rig();
            let write = handle.set_property("target", BINDING, true, |_| {});
            if dispatched {
                driver.drain_requests().unwrap();
            }
            handle.cancel_device("target").unwrap();
            driver.drain_cancellations();
            assert_eq!(
                write.await,
                RequestOutcome {
                    result: Err(CentralError::Cancelled),
                    stage: RequestStage::NotSent,
                    timeout: None,
                }
            );
            write_ready(&mut driver, &budget, usize::MAX).await;
            assert_eq!(written.get(), 0);
            assert!(driver.pending.is_empty());
        }
    });
}
