use std::{
    error::Error,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

pub const USAGE: &str = "用法：migate [--data-dir <PATH>]";

#[derive(Debug, Eq, PartialEq)]
pub struct Config {
    pub data_dir: PathBuf,
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
        let mut args = args.into_iter();
        let mut cli_data_dir = None;
        while let Some(arg) = args.next() {
            if arg != "--data-dir" || cli_data_dir.is_some() {
                return Err(ConfigError(format!(
                    "无效启动参数：{}",
                    arg.to_string_lossy()
                )));
            }
            cli_data_dir = Some(
                args.next()
                    .filter(|value| !value.as_encoded_bytes().starts_with(b"--"))
                    .ok_or_else(|| ConfigError("--data-dir 缺少路径".into()))?,
            );
        }
        let data_dir = match cli_data_dir.or(env_data_dir) {
            Some(value) => {
                if value.is_empty() {
                    return Err(ConfigError("数据目录不能为空".into()));
                }
                PathBuf::from(value)
            }
            None => {
                let home = home.filter(|value| !value.is_empty()).ok_or_else(|| {
                    ConfigError("无法确定用户主目录，请指定 --data-dir 或 MIGATE_DATA_DIR".into())
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
}
