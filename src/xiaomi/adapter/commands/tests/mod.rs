use super::*;

#[test]
fn dropping_the_consumer_scope_closes_the_operation_and_late_results_do_not_replace_last() {
    let commands = Rc::new(Commands::default());
    let binding = PropertyBinding { siid: 2, piid: 1 };
    let first = commands.begin("one", "main", binding, true, Instant::now());
    first.0.submitted(PathKind::Local);
    let first_sent = first.0.sent_at.get();
    let late = first.0.clone();
    drop(first);
    let first_status = commands.snapshot("one", ["main".into()].into_iter());
    assert!(first_status[0].inflight.is_none());
    let first_id = first_status[0].last.as_ref().unwrap().id;
    assert_eq!(
        first_status[0].last.as_ref().unwrap().result,
        Some(Err(CommandError::Cancelled))
    );
    let second = commands.begin("one", "main", binding, false, Instant::now());
    second.0.submitted(PathKind::Cloud);
    second.0.finish(Ok(()));
    late.finish(Ok(()));
    late.submitted(PathKind::Cloud);
    assert_eq!(late.sent_at.get(), first_sent);
    let status = commands.snapshot("one", ["main".into()].into_iter());
    let last = status[0].last.as_ref().unwrap();
    assert!(last.id > first_id);
    assert_eq!(last.actual_path, Some(PathKind::Cloud));
    assert_eq!(last.result, Some(Ok(())));
    assert!(status[0].inflight.is_none());
    commands.remove("one");
    second.0.finish(Err(CommandError::Timeout));
    assert!(
        commands.snapshot("one", ["main".into()].into_iter())[0]
            .last
            .is_none()
    );
}
