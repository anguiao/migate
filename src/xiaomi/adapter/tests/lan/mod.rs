use super::*;
use crate::{
    storage::LanRecord,
    xiaomi::{
        lan::{Token, tests::support::FakeDevice},
        mijia::{CloudDeviceInfo, LanCandidate, LanCredentials},
    },
};

const RECOVERY_REQUEST_TIMEOUT: Duration = Duration::from_millis(400);

mod connection;
mod identity;
mod polling;
mod routing;
