use super::*;
use crate::{
    storage::LanRecord,
    xiaomi::{
        lan::{Token, test_support::FakeDevice},
        mijia::{CloudDeviceInfo, LanCandidate, LanCredentials},
    },
};

mod connection;
mod identity;
mod polling;
mod routing;
