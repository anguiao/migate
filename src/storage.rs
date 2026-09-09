use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub bridge_id: String,
    pub light_id: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u32,
    identity: Identity,
    blobs: BTreeMap<u16, Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    payload: Payload,
    checksum: [u8; 32],
}

#[derive(Debug)]
pub struct StorageError {
    path: PathBuf,
    context: &'static str,
    source: Option<std::io::Error>,
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.context, self.path.display())?;
        if let Some(source) = &self.source {
            write!(f, "：{source}")?;
        }
        Ok(())
    }
}
impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn Error + 'static))
    }
}

fn invalid(path: &Path, context: &'static str) -> StorageError {
    StorageError {
        path: path.into(),
        context,
        source: None,
    }
}
fn io_error(path: &Path, source: std::io::Error) -> StorageError {
    StorageError {
        path: path.into(),
        context: "存储访问失败",
        source: Some(source),
    }
}
fn checksum(payload: &Payload) -> [u8; 32] {
    // Serialization of these concrete fields is infallible.
    Sha256::digest(serde_json::to_vec(payload).unwrap()).into()
}

pub struct Store {
    directory: PathBuf,
    payload: Payload,
}

impl Store {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, StorageError> {
        let directory = directory.as_ref();
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(directory)
            .map_err(|e| io_error(directory, e))?;
        let path = directory.join("state.json");
        let payload = match fs::read(&path) {
            Ok(bytes) => {
                // Do not include serde errors: they may quote credential contents.
                let envelope: Envelope =
                    serde_json::from_slice(&bytes).map_err(|_| invalid(&path, "存储格式损坏"))?;
                let valid_id = |id: &str| {
                    id.len() == 32
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                };
                if envelope.payload.version != 1
                    || !valid_id(&envelope.payload.identity.bridge_id)
                    || !valid_id(&envelope.payload.identity.light_id)
                    || envelope.payload.identity.bridge_id == envelope.payload.identity.light_id
                    || envelope.checksum != checksum(&envelope.payload)
                {
                    return Err(invalid(&path, "存储版本、身份或校验和无效"));
                }
                envelope.payload
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut entries = fs::read_dir(directory).map_err(|e| io_error(directory, e))?;
                match entries.next() {
                    Some(Ok(_)) => return Err(invalid(&path, "数据目录初始化不完整")),
                    Some(Err(error)) => return Err(io_error(directory, error)),
                    None => {}
                }
                let payload = Payload {
                    version: 1,
                    identity: Identity {
                        bridge_id: Uuid::new_v4().simple().to_string(),
                        light_id: Uuid::new_v4().simple().to_string(),
                    },
                    blobs: BTreeMap::new(),
                };
                let store = Self {
                    directory: directory.into(),
                    payload,
                };
                store.flush()?;
                return Ok(store);
            }
            Err(error) => return Err(io_error(&path, error)),
        };
        private_permissions(directory, 0o700)?;
        private_permissions(&path, 0o600)?;
        Ok(Self {
            directory: directory.into(),
            payload,
        })
    }

    pub fn identity(&self) -> &Identity {
        &self.payload.identity
    }
    pub fn load(&self, key: u16) -> Option<&[u8]> {
        self.payload.blobs.get(&key).map(Vec::as_slice)
    }
    pub fn store(&mut self, key: u16, value: &[u8]) -> Result<(), StorageError> {
        let mut next = self.payload.clone();
        next.blobs.insert(key, value.into());
        self.persist(&next)?;
        self.payload = next;
        Ok(())
    }
    pub fn remove(&mut self, key: u16) -> Result<(), StorageError> {
        if !self.payload.blobs.contains_key(&key) {
            return Ok(());
        }
        let mut next = self.payload.clone();
        next.blobs.remove(&key);
        self.persist(&next)?;
        self.payload = next;
        Ok(())
    }
    pub fn flush(&self) -> Result<(), StorageError> {
        self.persist(&self.payload)
    }

    fn persist(&self, payload: &Payload) -> Result<(), StorageError> {
        private_permissions(&self.directory, 0o700)?;
        let path = self.directory.join("state.json");
        let bytes = serde_json::to_vec(&Envelope {
            payload: payload.clone(),
            checksum: checksum(payload),
        })
        .unwrap();
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)
            .map_err(|e| io_error(&self.directory, e))?;
        temporary
            .write_all(&bytes)
            .map_err(|e| io_error(&path, e))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|e| io_error(&path, e))?;
        temporary
            .persist(&path)
            .map_err(|e| io_error(&path, e.error))?;
        fs::File::open(&self.directory)
            .and_then(|file| file.sync_all())
            .map_err(|e| io_error(&self.directory, e))?;
        Ok(())
    }
}

fn private_permissions(path: &Path, mode: u32) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|e| io_error(path, e))?;
    }
    Ok(())
}
