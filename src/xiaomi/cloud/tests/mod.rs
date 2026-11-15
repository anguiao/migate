pub(crate) use super::notifications::tests::support as notification_support;

use super::*;
use crate::xiaomi::tests::support::{MockResponse, mock_server};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_lite::future;
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

mod client;
mod identity;
mod oauth;
