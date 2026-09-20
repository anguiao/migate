use super::*;

fn config(args: &[&str], env: Option<&str>, home: Option<&str>) -> Result<Config, ConfigError> {
    Config::parse(
        args.iter().map(OsString::from),
        env.map(OsString::from),
        None,
        home.map(OsString::from),
        Path::new("/work"),
    )
}

#[test]
fn matter_port_defaults_and_accepts_ephemeral_or_explicit_ports() {
    for (value, expected) in [
        (None, 5540),
        (Some("0"), 0),
        (Some("5541"), 5541),
        (Some("65535"), 65535),
    ] {
        let config = Config::parse(
            [],
            Some("/data".into()),
            value.map(OsString::from),
            None,
            Path::new("/work"),
        )
        .unwrap();
        assert_eq!(config.matter_port, expected);
    }
}

#[test]
fn invalid_matter_ports_are_rejected() {
    use std::os::unix::ffi::OsStringExt;

    for value in [
        OsString::from(""),
        OsString::from("-1"),
        OsString::from("65536"),
        OsString::from("1.5"),
        OsString::from("port"),
        OsString::from_vec(vec![0xff]),
    ] {
        let error = Config::parse(
            [],
            Some("/data".into()),
            Some(value),
            None,
            Path::new("/work"),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("MIGATE_MATTER_PORT must be an integer from 0 to 65535")
        );
    }
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
    assert_eq!(
        config(&["--data-dir", "cli", "auth", "check"], None, None)
            .unwrap()
            .command,
        Command::Auth(AuthCommand::Check)
    );
}

#[test]
fn commands_are_parsed_only_after_global_options() {
    assert_eq!(
        config(&[], None, Some("/home/me")).unwrap().command,
        Command::Bridge
    );
    for (name, command) in [
        ("login", AuthCommand::Login),
        ("check", AuthCommand::Check),
        ("logout", AuthCommand::Logout),
    ] {
        assert_eq!(
            config(&["auth", name], None, Some("/home/me"))
                .unwrap()
                .command,
            Command::Auth(command)
        );
    }
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
        vec!["auth"],
        vec!["auth", "other"],
        vec!["auth", "check", "extra"],
        vec!["auth", "check", "--data-dir", "late"],
        vec!["--data-dir", "auth", "check"],
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
