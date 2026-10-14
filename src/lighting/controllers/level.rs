// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025-2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// Adapted from rs-matter 0.4.1 dm/clusters/app/level_control.rs,
// commit 3f3528ef5c492b6feb0a2e043264dc47e9da4340. Apache-2.0.
// Retains bounded step/move planning; protocol mode enums are mapped by the
// Matter entry and time uses elapsed deadlines.
use super::MoveDirection;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Task {
    MoveToLevel {
        with_on_off: bool,
        start: u8,
        target: u8,
        transition_ms: u64,
    },
    Move {
        with_on_off: bool,
        start: u8,
        direction: MoveDirection,
        rate: u8,
    },
}
impl Task {
    pub(crate) fn duration(self) -> u64 {
        match self {
            Self::MoveToLevel {
                start,
                target,
                transition_ms,
                ..
            } => {
                if start == target {
                    0
                } else {
                    transition_ms
                }
            }
            Self::Move {
                start,
                direction,
                rate,
                ..
            } => {
                let target = match direction {
                    MoveDirection::Up => 254,
                    MoveDirection::Down => 1,
                };
                (start.abs_diff(target) as u64 * 1000).div_ceil(rate as u64)
            }
        }
    }
    pub(crate) fn frame(self, elapsed: u64) -> (u8, bool, Option<bool>) {
        match self {
            Self::MoveToLevel {
                with_on_off,
                start,
                target,
                transition_ms,
            } => {
                let end = elapsed >= self.duration();
                let steps = if end {
                    start.abs_diff(target) as u64
                } else {
                    elapsed * start.abs_diff(target) as u64 / transition_ms
                };
                let current = if target >= start {
                    start + steps as u8
                } else {
                    start - steps as u8
                };
                (
                    current,
                    end,
                    with_on_off.then_some(if end {
                        target > 1
                    } else {
                        start > 1 || target > start
                    }),
                )
            }
            Self::Move {
                with_on_off,
                start,
                direction,
                rate,
            } => {
                let steps = (elapsed as u128 * rate as u128 / 1000).min(254) as u8;
                let current = match direction {
                    MoveDirection::Up => start.saturating_add(steps).min(254),
                    MoveDirection::Down => start.saturating_sub(steps).max(1),
                };
                let end = elapsed >= self.duration();
                (
                    current,
                    end,
                    with_on_off.then_some(current > 1 || (!end && direction == MoveDirection::Up)),
                )
            }
        }
    }
}
