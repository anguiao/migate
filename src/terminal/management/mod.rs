mod protocol;

use self::protocol::{Frame, GREETING, REQUEST_LIMIT, read_frame, write_frame};
use super::bridge::Management;
use crate::{
    RuntimeError,
    storage::{InstanceLock, SOCKET_FILE_NAME},
};
use async_io::{Async, Timer};
use futures_lite::{
    StreamExt, future,
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
};
use futures_util::stream::FuturesUnordered;
use std::{
    cell::Cell,
    collections::VecDeque,
    fs::{self, Metadata},
    io::{self, ErrorKind, Read},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
        net::{SocketAddr, UnixListener, UnixStream},
    },
    path::{Path, PathBuf, absolute},
    time::Duration,
};

pub struct Listener<'a> {
    listener: Async<UnixListener>,
    socket: SocketFile,
    _instance: &'a InstanceLock,
}

struct SocketFile {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Listener<'_> {
    pub fn bind(instance: &InstanceLock) -> Result<Listener<'_>, RuntimeError> {
        let directory = instance.directory();
        let path = directory.join(SOCKET_FILE_NAME);
        let setup = || {
            if socket_metadata(&path)?.is_some() {
                fs::remove_file(&path)?;
            }
            let listener = SocketAddress::new(directory)?.bind()?;
            let metadata = fs::symlink_metadata(&path)?;
            let socket = SocketFile {
                path: path.clone(),
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            Ok::<_, io::Error>(Listener {
                listener: Async::new(listener)?,
                socket,
                _instance: instance,
            })
        };
        setup().map_err(|error| {
            format!(
                "Failed to set up management socket ({}): {error}",
                path.display()
            )
            .into()
        })
    }

    pub(crate) async fn serve(&self, management: &Management<'_>) -> Result<(), RuntimeError> {
        let mut sessions = FuturesUnordered::new();
        loop {
            let completed = async {
                while sessions.next().await.is_some() {}
                future::pending().await
            };
            let (stream, _) = future::or(completed, self.listener.accept())
                .await
                .map_err(|error| {
                    format!(
                        "Failed to accept a management connection ({}): {error}",
                        self.socket.path.display(),
                    )
                })?;
            sessions.push(async move {
                if let Err(error) = serve_session(&stream, management).await {
                    log::debug!("Management session ended: {error}");
                }
            });
        }
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && (metadata.dev(), metadata.ino()) == (self.device, self.inode)
            && let Err(error) = fs::remove_file(&self.path)
        {
            log::warn!(
                "Failed to remove management socket ({}): {error}",
                self.path.display()
            );
        }
    }
}

fn socket_metadata(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => Ok(Some(metadata)),
        Ok(_) => Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "Management socket path conflict",
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

struct SocketAddress {
    address: SocketAddr,
    _alias: Option<tempfile::TempDir>,
}

impl SocketAddress {
    fn new(directory: &Path) -> io::Result<Self> {
        if let Ok(address) = SocketAddr::from_pathname(directory.join(SOCKET_FILE_NAME)) {
            return Ok(Self {
                address,
                _alias: None,
            });
        }

        // The filesystem path has been checked already. A short private alias
        // fits in sockaddr_un while the socket file stays in the data directory.
        let alias = tempfile::Builder::new()
            .prefix("migate-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")?;
        let link = alias.path().join("data");
        symlink(absolute(directory)?, &link)?;
        Ok(Self {
            address: SocketAddr::from_pathname(link.join(SOCKET_FILE_NAME))?,
            _alias: Some(alias),
        })
    }

    fn bind(self) -> io::Result<UnixListener> {
        UnixListener::bind_addr(&self.address)
    }

    fn connect(self) -> io::Result<UnixStream> {
        UnixStream::connect_addr(&self.address)
    }
}

pub struct Client {
    stream: BufReader<Async<UnixStream>>,
}

impl Client {
    /// Return `None` only if the socket is absent or refuses the connection.
    pub async fn connect(directory: &Path) -> Result<Option<Self>, RuntimeError> {
        let path = directory.join(SOCKET_FILE_NAME);
        let connecting = |error| {
            format!(
                "Failed to connect to the management socket ({}): {error}",
                path.display()
            )
        };
        if socket_metadata(&path).map_err(connecting)?.is_none() {
            return Ok(None);
        }
        let address = SocketAddress::new(directory).map_err(connecting)?;
        let connected = blocking::unblock(move || address.connect()).await;
        let stream = match connected {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionRefused | ErrorKind::NotFound
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(connecting(error).into()),
        };
        let mut stream = BufReader::new(Async::new(stream).map_err(connecting)?);
        let greeting = async {
            let mut header = [0; GREETING.len()];
            stream.read_exact(&mut header).await?;
            if header != GREETING {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "Unexpected greeting",
                ));
            }
            Ok(())
        };
        let timeout = async {
            Timer::after(Duration::from_secs(3)).await;
            Err(io::Error::new(
                ErrorKind::TimedOut,
                "Timed out waiting for the greeting",
            ))
        };
        future::or(greeting, timeout)
            .await
            .map_err(|error| format!("Management protocol error ({}): {error}", path.display()))?;
        Ok(Some(Self { stream }))
    }

    pub async fn execute(
        &mut self,
        words: &[&str],
        mut output: impl AsyncWrite + Unpin,
    ) -> Result<bool, RuntimeError> {
        let request = protocol::encode(&words)?;
        if request.len() as u64 > REQUEST_LIMIT {
            return Err("Management command is too long".into());
        }
        self.stream
            .get_mut()
            .write_all(&request)
            .await
            .map_err(|error| format!("Failed to submit the management command: {error}"))?;
        loop {
            let frame = read_frame::<Frame>(&mut self.stream, u64::MAX).await;
            match frame {
                Ok(Some(Frame::Output(text))) => {
                    output.write_all(text.as_bytes()).await?;
                    output.flush().await?;
                }
                Ok(Some(Frame::Done(success))) => return Ok(success),
                Err(error) if error.kind() == ErrorKind::InvalidData => {
                    return Err(format!("Management protocol error: {error}").into());
                }
                _ => {
                    let follow_up = if words.first() == Some(&"auth") {
                        "Reconnect and use auth check to verify the saved credentials."
                    } else {
                        "Reconnect and use status DID to check the device."
                    };
                    return Err(format!("The management connection closed before completion. The command result is unknown. {follow_up}").into());
                }
            }
        }
    }

    pub async fn attach(
        &mut self,
        input: impl AsyncBufRead + Unpin,
        mut output: impl AsyncWrite + Unpin,
    ) -> Result<(), RuntimeError> {
        read_terminal(input, async |words| {
            self.execute(words, &mut output).await?;
            Ok(())
        })
        .await
    }

    pub async fn attach_interactive(
        &mut self,
        input: impl AsyncBufRead + Unpin,
        mut output: impl AsyncWrite + Unpin,
    ) -> Result<(), RuntimeError> {
        TerminalInput::new(input)
            .read_commands(async |words, input| {
                let disconnected = async {
                    input.wait_for_end().await?;
                    Ok(false)
                };
                let command = async {
                    self.execute(words, &mut output).await?;
                    Ok(true)
                };
                future::or(disconnected, command).await
            })
            .await
    }
}

pub fn stdin_input(stdin: io::Stdin) -> impl AsyncBufRead + Unpin {
    BufReader::new(blocking::Unblock::new(StdinReader {
        input: stdin,
        ended: false,
    }))
}

struct StdinReader<R> {
    input: R,
    ended: bool,
}

impl<R: Read> Read for StdinReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.ended || buffer.is_empty() {
            return Ok(0);
        }
        let read = self.input.read(buffer)?;
        // A terminal sends a single zero-length read at Ctrl-D. Latch it
        // because the blocking reader can read again while filling its buffer.
        self.ended = read == 0;
        Ok(read)
    }
}

pub(crate) async fn read_terminal(
    input: impl AsyncBufRead + Unpin,
    mut execute: impl AsyncFnMut(&[&str]) -> Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    TerminalInput::new(input)
        .read_commands(async |words, _| {
            execute(words).await?;
            Ok(true)
        })
        .await
}

pub(crate) async fn read_foreground(
    input: impl AsyncBufRead + Unpin,
    mut execute: impl AsyncFnMut(&[&str], &Session) -> Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    TerminalInput::new(input)
        .read_commands(async |words, input| {
            let session = Session::default();
            let command = execute(words, &session);
            if words == ["auth", "login"] {
                let disconnected = async {
                    input.wait_for_end().await?;
                    session.close();
                    future::pending().await
                };
                let result = future::or(disconnected, command).await;
                if !session.closed.get() {
                    result?;
                }
            } else {
                command.await?;
            }
            Ok(true)
        })
        .await
}

struct TerminalInput<R> {
    reader: R,
    queued: VecDeque<String>,
    partial: Vec<u8>,
    ended: bool,
}

impl<R: AsyncBufRead + Unpin> TerminalInput<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            queued: VecDeque::new(),
            partial: Vec::new(),
            ended: false,
        }
    }

    async fn read_commands(
        &mut self,
        mut execute: impl AsyncFnMut(&[&str], &mut Self) -> Result<bool, RuntimeError>,
    ) -> Result<(), RuntimeError> {
        loop {
            let line = match self.queued.pop_front() {
                Some(line) => line,
                None => match self.read_line().await? {
                    Some(line) => line,
                    None => return Ok(()),
                },
            };
            let words = line.split_whitespace().collect::<Vec<_>>();
            if !words.is_empty() && !execute(&words, self).await? {
                return Ok(());
            }
        }
    }

    async fn wait_for_end(&mut self) -> io::Result<()> {
        while let Some(line) = self.read_line().await? {
            self.queued.push_back(line);
        }
        Ok(())
    }

    async fn read_line(&mut self) -> io::Result<Option<String>> {
        if self.ended {
            return Ok(None);
        }
        // Keep bytes read before the command completes and drops its input watcher.
        self.ended = self.reader.read_until(b'\n', &mut self.partial).await? == 0;
        if self.partial.is_empty() {
            return Ok(None);
        }
        String::from_utf8(std::mem::take(&mut self.partial))
            .map(Some)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))
    }
}

#[derive(Default)]
pub(crate) struct Session {
    closed: Cell<bool>,
    changed: event_listener::Event,
}

impl Session {
    fn close(&self) {
        self.closed.set(true);
        self.changed.notify(usize::MAX);
    }

    pub(crate) async fn disconnected(&self) {
        let listener = self.changed.listen();
        if !self.closed.get() {
            listener.await;
        }
    }
}

pub(crate) struct Output<W> {
    writer: W,
    framed: bool,
}

impl<W: AsyncWrite + Unpin> Output<W> {
    pub(crate) fn terminal(writer: W) -> Self {
        Self {
            writer,
            framed: false,
        }
    }

    pub(crate) async fn write(&mut self, session: &Session, text: String) -> io::Result<()> {
        let disconnected = async {
            session.disconnected().await;
            Err(ErrorKind::BrokenPipe.into())
        };
        let write = async {
            if self.framed {
                write_frame(&mut self.writer, &Frame::Output(text)).await
            } else {
                self.writer.write_all(text.as_bytes()).await?;
                self.writer.flush().await
            }
        };
        future::or(disconnected, write).await
    }
}

async fn serve_session(stream: &Async<UnixStream>, management: &Management<'_>) -> io::Result<()> {
    let mut input = BufReader::new(stream);
    let mut output = Output {
        writer: stream,
        framed: true,
    };
    let session = Session::default();
    output.writer.write_all(GREETING).await?;
    while let Some(words) = read_frame::<Vec<String>>(&mut input, REQUEST_LIMIT).await? {
        let words = words.iter().map(String::as_str).collect::<Vec<_>>();
        let watch = async {
            // A client can leave after submission. Observe the disconnect without
            // dropping the command future, whose side effects belong to the bridge.
            if !input.fill_buf().await.is_ok_and(|bytes| !bytes.is_empty()) {
                session.close();
            }
            future::pending().await
        };
        let execute = management.execute(&words, &session, &mut output);
        let result = future::or(watch, execute).await;
        if session.closed.get() {
            return Ok(());
        }
        write_frame(&mut output.writer, &Frame::Done(result?)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
