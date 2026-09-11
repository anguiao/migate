use std::{
    error::Error,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

pub const USAGE: &str = "Usage: migate [--data-dir <PATH>] [auth <login|check|logout>]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Bridge,
    Auth(AuthCommand),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthCommand {
    Login,
    Check,
    Logout,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Config {
    pub data_dir: PathBuf,
    pub command: Command,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\n{USAGE}", self.0)
    }
}

impl Error for ConfigError {}

impl Config {
    /// Arguments exclude the executable name. Environment and cwd are injected.
    pub fn parse(
        args: impl IntoIterator<Item = OsString>,
        env_data_dir: Option<OsString>,
        home: Option<OsString>,
        cwd: &Path,
    ) -> Result<Self, ConfigError> {
        let args: Vec<_> = args.into_iter().collect();
        let mut index = 0;
        let mut cli_data_dir = None;
        if args.get(index).is_some_and(|arg| arg == "--data-dir") {
            index += 1;
            let value = args
                .get(index)
                .filter(|value| !value.as_encoded_bytes().starts_with(b"--"))
                .ok_or_else(|| ConfigError("--data-dir requires a path".into()))?;
            cli_data_dir = Some(value.clone());
            index += 1;
        }
        let command = match args.get(index).map(|value| value.to_string_lossy()) {
            None => Command::Bridge,
            Some(value) if value == "auth" => {
                index += 1;
                let command = match args.get(index).map(|value| value.to_string_lossy()) {
                    Some(value) if value == "login" => AuthCommand::Login,
                    Some(value) if value == "check" => AuthCommand::Check,
                    Some(value) if value == "logout" => AuthCommand::Logout,
                    Some(value) => {
                        return Err(ConfigError(format!("Invalid auth command: {value}")));
                    }
                    None => return Err(ConfigError("auth requires a command".into())),
                };
                index += 1;
                Command::Auth(command)
            }
            Some(value) => return Err(ConfigError(format!("Invalid argument: {value}"))),
        };
        if let Some(value) = args.get(index) {
            return Err(ConfigError(format!(
                "Invalid argument: {}",
                value.to_string_lossy()
            )));
        }
        let data_dir = match cli_data_dir.or(env_data_dir) {
            Some(value) => {
                if value.is_empty() {
                    return Err(ConfigError("Data directory cannot be empty".into()));
                }
                PathBuf::from(value)
            }
            None => {
                let home = home.filter(|value| !value.is_empty()).ok_or_else(|| {
                    ConfigError(
                        "Cannot determine the home directory; use --data-dir or MIGATE_DATA_DIR"
                            .into(),
                    )
                })?;
                PathBuf::from(home).join(".migate")
            }
        };
        Ok(Self {
            data_dir: if data_dir.is_absolute() {
                data_dir
            } else {
                cwd.join(data_dir)
            },
            command,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(args: &[&str], env: Option<&str>, home: Option<&str>) -> Result<Config, ConfigError> {
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
}
