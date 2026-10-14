// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025-2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// Shared physical lighting trajectories used by the executor outside the
// Matter instance. Protocol enums, Options and OnLevel stay at the Matter
// entry and are converted before a plan is committed.
pub(crate) mod color;
pub(crate) mod hs;
pub(crate) mod level;

/// Generic Move direction after the Matter entry maps its protocol enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MoveDirection {
    Up,
    Down,
}
