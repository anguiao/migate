use futures_lite::future::{block_on, poll_once};
use migate::{
    config::Config, device::Command, terminal::handle_line, virtual_device::VirtualLight,
};
use std::{ffi::OsString, path::Path, sync::Arc};

#[test]
fn commands_share_one_state_and_toggles_are_serial() {
    let light = Arc::new(VirtualLight::new());
    assert_eq!(light.snapshot().id, "virtual-light-1");
    assert!(!light.snapshot().power);
    let workers: Vec<_> = (0..5)
        .map(|_| {
            let light = light.clone();
            std::thread::spawn(move || {
                for _ in 0..101 {
                    light.execute(Command::Toggle);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(light.snapshot().power);
    assert!(!light.execute(Command::Off).power);
    assert!(light.execute(Command::On).power);
}

#[test]
fn notifications_track_real_changes_and_latest_snapshot() {
    block_on(async {
        let light = VirtualLight::new();
        let mut changes = light.subscribe();
        light.execute(Command::Off);
        assert!(poll_once(changes.changed()).await.is_none());
        light.execute(Command::On);
        light.execute(Command::Off);
        light.execute(Command::On);
        assert!(changes.changed().await.power);
        assert!(poll_once(changes.changed()).await.is_none());
        light.execute(Command::On);
        assert!(poll_once(changes.changed()).await.is_none());
        light.execute(Command::Off);
        assert!(!changes.changed().await.power);
    });
}

#[test]
fn terminal_recovers_and_reads_shared_state() {
    let light = VirtualLight::new();
    assert_eq!(handle_line(&light, " \n"), None);
    assert_eq!(
        handle_line(&light, " on \n").unwrap(),
        "virtual-light-1: on"
    );
    for invalid in ["toggle", "ON", "on extra", "status extra", "off extra"] {
        assert!(
            handle_line(&light, invalid)
                .unwrap()
                .contains("on, off, status")
        );
        assert!(light.snapshot().power);
    }
    light.execute(Command::Off);
    assert_eq!(
        handle_line(&light, "status").unwrap(),
        "virtual-light-1: off"
    );
    assert_eq!(handle_line(&light, "on").unwrap(), "virtual-light-1: on");
    assert_eq!(handle_line(&light, "off").unwrap(), "virtual-light-1: off");
}

fn config(
    args: &[&str],
    env: Option<&str>,
    home: Option<&str>,
) -> Result<Config, migate::config::ConfigError> {
    Config::parse(
        args.iter().map(OsString::from),
        env.map(OsString::from),
        home.map(OsString::from),
        Path::new("/work"),
    )
}

#[test]
fn data_directory_priority_and_relative_paths() {
    assert_eq!(
        config(&["--data-dir", "cli"], Some("env"), Some("/home/me"))
            .unwrap()
            .data_dir,
        Path::new("/work/cli")
    );
    assert_eq!(
        config(&[], Some("env"), Some("/home/me")).unwrap().data_dir,
        Path::new("/work/env")
    );
    assert_eq!(
        config(&[], None, Some("/home/me")).unwrap().data_dir,
        Path::new("/home/me/.migate")
    );
    assert_eq!(
        config(&["--data-dir", "/absolute"], Some(""), None)
            .unwrap()
            .data_dir,
        Path::new("/absolute")
    );
    assert_eq!(
        config(&[], Some("/env"), None).unwrap().data_dir,
        Path::new("/env")
    );
}

#[test]
fn invalid_configuration_is_rejected() {
    for args in [
        vec!["--other"],
        vec!["--data-dir"],
        vec!["--data-dir", "--other"],
        vec!["--data-dir", ""],
        vec!["--data-dir", "a", "extra"],
        vec!["--data-dir", "a", "--data-dir", "b"],
    ] {
        assert!(
            config(&args, Some("valid"), Some("/home/me")).is_err(),
            "{args:?}"
        );
    }
    assert!(config(&[], Some(""), Some("/home/me")).is_err());
    assert!(config(&[], None, None).is_err());
    assert!(config(&[], None, Some("")).is_err());
}
