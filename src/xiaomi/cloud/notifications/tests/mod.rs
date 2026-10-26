use super::{
    NotificationDriver, NotificationError, NotificationEvent, NotificationHandle, Timing,
    connection::start,
};
use crate::{
    storage::{TokenSet, XiaomiRecord},
    xiaomi::{capability::PropertyBinding, cloud::CLIENT_ID, mqtt},
};
use async_io::Async;
use futures_lite::{
    future,
    io::{AsyncReadExt, AsyncWriteExt},
};
use mqttbytes::{QoS, v5::*};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    future::Future,
    net::{SocketAddr, TcpListener, TcpStream},
    rc::Rc,
    time::Duration,
};

use support::*;

mod connection;
mod events;
mod messages;
mod subscriptions;
pub(crate) mod support;
