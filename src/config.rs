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
    pub matter_port: u16,
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
        env_matter_port: Option<OsString>,
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
        let matter_port = match env_matter_port {
            Some(value) => value
                .to_str()
                .and_then(|value| value.parse::<u16>().ok())
                .ok_or_else(|| {
                    ConfigError("MIGATE_MATTER_PORT must be an integer from 0 to 65535".into())
                })?,
            None => rs_matter::MATTER_PORT,
        };
        Ok(Self {
            data_dir: if data_dir.is_absolute() {
                data_dir
            } else {
                cwd.join(data_dir)
            },
            matter_port,
            command,
        })
    }
}

#[cfg(test)]
mod tests;
