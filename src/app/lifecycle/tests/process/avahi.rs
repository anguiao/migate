//! Control replies to Avahi entry group operations while the bridge requests rebuilds.

use super::{
    MATTER_LISTENING, Stream, TestProcess, process_support::TIMEOUT, ready_bridge_command,
};
use futures_lite::{StreamExt, future};
use rs_matter::utils::zbus::{Connection, Message, MessageStream, connection, message::Type};
use std::{collections::HashMap, fs, path::Path, process::Command};

pub(super) fn private_bus(directory: &Path) -> (TestProcess, String) {
    let address = format!("unix:path={}/test-bus", directory.display());
    let mut bus = TestProcess::spawn(Command::new("dbus-daemon").args([
        "--session",
        "--nofork",
        "--print-address=1",
        "--address",
        &address,
    ]));
    bus.wait_for_output(Stream::Stdout, &address);
    (bus, address)
}

#[derive(Default)]
struct EntryGroup {
    port: Option<u16>,
    committed: bool,
}

struct AvahiPeer {
    connection: Connection,
    messages: MessageStream,
    next_group: usize,
    groups: HashMap<String, EntryGroup>,
}

impl AvahiPeer {
    fn new(address: &str) -> Self {
        future::block_on(async {
            let connection = connection::Builder::address(address)
                .unwrap()
                .build()
                .await
                .unwrap();
            let messages = MessageStream::from(&connection);
            connection
                .request_name("org.freedesktop.Avahi")
                .await
                .unwrap();
            Self {
                connection,
                messages,
                next_group: 0,
                groups: HashMap::new(),
            }
        })
    }

    /// Apply the requested operation before withholding its method return.
    fn hold_reply_to(&mut self, method: &str) -> Message {
        future::block_on(future::or(
            async {
                loop {
                    let message = self.messages.next().await.unwrap().unwrap();
                    let header = message.header();
                    if message.message_type() != Type::MethodCall {
                        continue;
                    }
                    let member = header.member().unwrap().as_str();
                    let path = header.path().unwrap().as_str();
                    match member {
                        "GetAPIVersion" => {
                            self.connection.reply(&header, &516u32).await.unwrap();
                            continue;
                        }
                        "EntryGroupNew" => {
                            self.next_group += 1;
                            let path = format!("/entry_group/{}", self.next_group);
                            self.groups.insert(path.clone(), EntryGroup::default());
                            let path =
                                rs_matter::utils::zbus::zvariant::OwnedObjectPath::try_from(path)
                                    .unwrap();
                            self.connection.reply(&header, &path).await.unwrap();
                            continue;
                        }
                        "AddService" => {
                            let body = message.body();
                            let (_, _, _, _, _, _, _, port, _) = body
                                .deserialize::<(i32, i32, u32, &str, &str, &str, &str, u16, Vec<Vec<u8>>)>()
                                .unwrap();
                            self.groups.get_mut(path).unwrap().port = Some(port);
                        }
                        "AddServiceSubtype" => assert!(self.groups.contains_key(path)),
                        "Commit" => self.groups.get_mut(path).unwrap().committed = true,
                        "Free" => {
                            if self.groups.remove(path).is_none() {
                                self.connection
                                    .reply_error(
                                        &header,
                                        "org.freedesktop.DBus.Error.UnknownObject",
                                        &"Entry group was already freed",
                                    )
                                    .await
                                    .unwrap();
                                panic!("bridge freed {path} twice");
                            }
                        }
                        _ => panic!("unexpected Avahi method {member}"),
                    }
                    if member == method {
                        return message;
                    }
                    self.connection.reply(&header, &()).await.unwrap();
                }
            },
            async {
                async_io::Timer::after(TIMEOUT).await;
                panic!("did not receive {method} from the bridge");
            },
        ))
    }

    fn release(&self, message: Message) {
        future::block_on(self.connection.reply(&message.header(), &())).unwrap();
    }

    fn assert_registered(&self, port: u16) {
        assert_eq!(self.groups.len(), 1);
        let group = self.groups.values().next().unwrap();
        assert!(group.committed);
        assert_eq!(group.port, Some(port));
    }
}

fn stage_rebuilds(
    process: &mut TestProcess,
    directory: &Path,
    changes: [u8; 2],
    expected_runs: usize,
) {
    for change in changes {
        fs::write(directory.join("source-remove"), b"change").unwrap();
        process.wait_for_output(
            Stream::Stderr,
            &format!("Fake source staged configuration {change}"),
        );
    }
    let responses = process.output().0.matches("Available commands:").count();
    process.send("help\n");
    process.wait_for_output_matching(Stream::Stdout, "help output", TIMEOUT, |output| {
        output.matches("Available commands:").count() > responses
    });
    let (_, stderr) = process.output();
    assert_eq!(
        stderr.matches(MATTER_LISTENING).count(),
        expected_runs,
        "{stderr}"
    );
}

#[test]
fn rebuilds_wait_for_commit_and_free_replies_without_stalling_the_source() {
    let directory = tempfile::tempdir().unwrap();
    let mut command = ready_bridge_command(directory.path(), 0);
    let (_bus, address) = private_bus(directory.path());
    let mut avahi = AvahiPeer::new(&address);
    let mut process = TestProcess::spawn(command.env("DBUS_SYSTEM_BUS_ADDRESS", &address));

    let commit = avahi.hold_reply_to("Commit");
    stage_rebuilds(&mut process, directory.path(), [1, 2], 1);
    avahi.release(commit);

    let free = avahi.hold_reply_to("Free");
    stage_rebuilds(&mut process, directory.path(), [3, 4], 2);
    avahi.release(free);

    let free = avahi.hold_reply_to("Free");
    avahi.release(free);
    let (port, _) = process.wait_for_advertisement(3);
    avahi.assert_registered(port);
    assert_eq!(avahi.next_group, 3);

    fs::write(directory.path().join("source-remove"), b"change").unwrap();
    let _free = avahi.hold_reply_to("Free");
    stage_rebuilds(&mut process, directory.path(), [6, 7], 4);
    process.stop("-TERM");
}

#[test]
fn avahi_errors_during_a_requested_rebuild_are_fatal() {
    let directory = tempfile::tempdir().unwrap();
    let mut command = ready_bridge_command(directory.path(), 0);
    let (_bus, address) = private_bus(directory.path());
    let mut avahi = AvahiPeer::new(&address);
    let mut process = TestProcess::spawn(command.env("DBUS_SYSTEM_BUS_ADDRESS", &address));

    let commit = avahi.hold_reply_to("Commit");
    stage_rebuilds(&mut process, directory.path(), [1, 2], 1);
    future::block_on(avahi.connection.reply_error(
        &commit.header(),
        "org.freedesktop.DBus.Error.Failed",
        &"Injected Avahi commit failure",
    ))
    .unwrap();
    let (status, _, stderr) = process.wait_for_exit(TIMEOUT);
    assert!(!status.success(), "{stderr}");
    assert!(
        stderr.contains("Failed to advertise Matter services with Avahi"),
        "{stderr}"
    );
    assert!(stderr.contains("Injected Avahi commit failure"), "{stderr}");
}
