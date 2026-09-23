//! Clip triggers and local-only game event sources.

use std::time::{Duration, Instant};

pub mod hotkey;
pub mod lol;

/// A request to clip. Both manual and automatic paths funnel through this, so the
/// replay buffer has exactly one entry point (spec §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Hotkey,
    GameEvent { kind: EventKind, at: Instant },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Kill,
    Death,
    Assist,
    Objective,
    GameStart,
    GameEnd,
}

/// Maps an instant onto the capture timeline, which is what the ledger indexes.
#[derive(Debug)]
pub struct CaptureClock {
    start: Instant,
}

impl CaptureClock {
    pub fn new() -> Self {
        Self { start: Instant::now() }
    }

    pub fn ms_at(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.start).as_millis() as u64
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

impl Default for CaptureClock {
    fn default() -> Self {
        Self::new()
    }
}
