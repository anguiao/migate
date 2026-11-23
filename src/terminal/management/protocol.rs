use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, ErrorKind};

pub(super) const GREETING: &[u8] = b"migate/1\n";
pub(super) const REQUEST_LIMIT: u64 = 64 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Frame {
    Output(String),
    Done(bool),
}

pub(super) async fn read_frame<T: DeserializeOwned>(
    reader: &mut (impl AsyncBufRead + Unpin),
    limit: u64,
) -> io::Result<Option<T>> {
    let mut bytes = Vec::new();
    reader.take(limit).read_until(b'\n', &mut bytes).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.last() != Some(&b'\n') {
        if bytes.len() as u64 == limit {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "Management message is too long",
            ));
        }
        return Err(io::Error::new(
            ErrorKind::UnexpectedEof,
            "Incomplete management message",
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))
}

pub(super) fn encode(message: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(super) async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &Frame,
) -> io::Result<()> {
    writer.write_all(&encode(message)?).await
}
