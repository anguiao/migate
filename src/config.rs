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
