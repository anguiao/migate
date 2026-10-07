use super::*;
use crate::{
    storage::{Store, TokenSet, XiaomiRecord},
    xiaomi::{
        certificate::{CertificateValidity, ClientIdentity},
        cloud::CloudClient,
        test_support::{MockResponse, dynamic_mock_server, mock_server, sign_csr},
    },
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_lite::future::{self, block_on};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    thread,
    time::Duration,
};

use support::*;

mod bridge;
mod certificate;
mod check;
mod lifecycle;
mod login;
mod support;
