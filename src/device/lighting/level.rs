// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025-2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// Adapted from rs-matter's elapsed-deadline level interpolation.

/// A trajectory in the caller's brightness coordinates. Protocol limits and
/// frame power targets are supplied by the command plan, not inferred here.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Task {
    MoveTo {
        start: u8,
        target: u8,
        transition_ms: u64,
    },
    Move {
        start: u8,
        target: u8,
        rate: u8,
    },
}
impl Task {
    pub(crate) fn duration(self) -> u64 {
        match self {
            Self::MoveTo {
                start,
                target,
                transition_ms,
            } => {
                if start == target {
                    0
                } else {
                    transition_ms
                }
            }
            Self::Move {
                start,
                target,
                rate,
            } => (start.abs_diff(target) as u64 * 1000).div_ceil(rate as u64),
        }
    }

    pub(crate) fn frame(self, elapsed: u64) -> (u8, bool) {
        let (start, target) = match self {
            Self::MoveTo { start, target, .. } | Self::Move { start, target, .. } => {
                (start, target)
            }
        };
        let end = elapsed >= self.duration();
        let distance = start.abs_diff(target) as u64;
        let steps = if end {
            distance
        } else {
            match self {
                Self::MoveTo { transition_ms, .. } => elapsed * distance / transition_ms,
                Self::Move { rate, .. } => {
                    (elapsed as u128 * rate as u128 / 1000).min(distance as u128) as u64
                }
            }
        };
        let current = if target >= start {
            start + steps as u8
        } else {
            start - steps as u8
        };
        (current, end)
    }
}
