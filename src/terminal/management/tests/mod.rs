use super::*;
use futures_lite::io::Cursor;
use std::{net::Shutdown, pin::pin};

#[test]
fn interactive_eof_detaches_after_ordered_commands_even_with_a_reply_pending() {
    let (client_socket, server) = UnixStream::pair().unwrap();
    let (input, terminal) = UnixStream::pair().unwrap();
    let server = Async::new(server).unwrap();
    let terminal = Async::new(terminal).unwrap();
    let mut commands = BufReader::new(&server);
    let mut rendered = Vec::new();
    future::block_on(future::or(
        async {
            let mut attached = pin!(async {
                let mut client = Client {
                    stream: BufReader::new(Async::new(client_socket).unwrap()),
                };
                client
                    .attach_interactive(BufReader::new(Async::new(input).unwrap()), &mut rendered)
                    .await
                    .unwrap();
            });
            (&terminal).write_all(b"status one\nstatus").await.unwrap();
            assert!(future::poll_once(attached.as_mut()).await.is_none());
            assert_eq!(
                read_frame::<Vec<String>>(&mut commands, REQUEST_LIMIT)
                    .await
                    .unwrap()
                    .unwrap(),
                ["status", "one"]
            );
            write_frame(&mut &server, &Frame::Output("first command\n".into()))
                .await
                .unwrap();
            write_frame(&mut &server, &Frame::Done(true)).await.unwrap();
            assert!(future::poll_once(attached.as_mut()).await.is_none());
            (&terminal).write_all(b" two\n").await.unwrap();
            assert!(future::poll_once(attached.as_mut()).await.is_none());
            assert_eq!(
                read_frame::<Vec<String>>(&mut commands, REQUEST_LIMIT)
                    .await
                    .unwrap()
                    .unwrap(),
                ["status", "two"]
            );
            terminal.get_ref().shutdown(Shutdown::Write).unwrap();
            attached.await;
            assert_eq!(commands.read(&mut [0]).await.unwrap(), 0);
        },
        async {
            Timer::after(Duration::from_secs(3)).await;
            panic!("timed out waiting for attach to disconnect");
        },
    ));
    assert_eq!(rendered, b"first command\n");
}

#[test]
fn foreground_input_drains_queued_commands_after_eof_cancels_login() {
    let mut executed = Vec::new();
    future::block_on(read_foreground(
        Cursor::new(b"auth login\nstatus\nhelp".as_slice()),
        async |words, session| {
            executed.push(words.join(" "));
            if words == ["auth", "login"] {
                session.disconnected().await;
                return Err(io::Error::from(ErrorKind::BrokenPipe).into());
            }
            assert!(!session.closed.get());
            Ok(())
        },
    ))
    .unwrap();
    assert_eq!(executed, ["auth login", "status", "help"]);
}

struct TerminalReads(VecDeque<&'static [u8]>);

impl Read for TerminalReads {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let part = self.0.pop_front().unwrap();
        buffer[..part.len()].copy_from_slice(part);
        Ok(part.len())
    }
}

#[test]
fn stdin_treats_a_single_terminal_eof_as_the_end_of_input() {
    let mut terminal = StdinReader {
        input: TerminalReads(VecDeque::from([b"status\n".as_slice(), b"", b"help\n"])),
        ended: false,
    };
    assert_eq!(terminal.read(&mut []).unwrap(), 0);

    let mut input = BufReader::new(blocking::Unblock::new(terminal));
    let mut received = String::new();
    future::block_on(input.read_to_string(&mut received)).unwrap();
    assert_eq!(received, "status\n");
}
