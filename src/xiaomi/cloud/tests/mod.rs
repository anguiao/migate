use super::*;
use crate::xiaomi::test_support::{MockResponse, mock_server};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_lite::future;
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

use support::*;

mod catalog;
mod client;
mod device_info;
mod oauth;
mod properties;
mod support;
