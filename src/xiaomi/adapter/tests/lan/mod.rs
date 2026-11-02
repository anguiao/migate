use super::*;
use crate::{
    storage::LanRecord,
    xiaomi::{
        cloud::{CloudDeviceInfo, LanCandidate, LanCredentials},
        lan::{Token, test_support::FakeDevice},
    },
};

mod connection;
mod identity;
mod polling;
mod routing;
