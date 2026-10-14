// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// Matter-side ColorControl interpretation; physical trajectories are shared.
pub(in crate::matter::lighting) use crate::lighting::controllers::color::*;

use super::RemainingTime;

#[derive(Default)]
pub(in crate::matter::lighting) struct ColorControlState {
    pub options: u8,
    pub remaining_time: RemainingTime,
}
