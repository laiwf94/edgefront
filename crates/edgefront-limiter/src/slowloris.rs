//! The slow-client check: pacerd's `min_rate`, ported from `check_rate`
//! and the window bookkeeping in `pump` and `pd_conn_accept` (`proxy.c`).
//!
//! A slowloris client sends just enough to dodge the idle timeout and so
//! holds its connection slot for ever. With `min_rate <bytes> per
//! <window>` set, every connection counts the bytes it moves, both ways
//! together, in a [`Window`] of the minimum's length cut into
//! [`CHECK_SLOTS`] buckets. Once a whole window has passed since the
//! connection opened, it is checked every tenth of a window, and a
//! connection that moved fewer than `bytes` in the last window is cut as
//! `slow_client`.
//!
//! A connection that our own shaping held back is exempt: when it was
//! throttled at any time during the last window, the slowness was ours and
//! the check passes. pacerd records the end of the latest throttle
//! (`last_throttle`), and so does [`SlowCheck::throttled`].
//!
//! A reload applies at the next check: a minimum switched off stops the
//! checks for good, and a new byte count or window length is what the
//! next check compares against. The window's buckets keep the width the
//! connection started with, as pacerd's did.
//!
//! Each connection keeps one [`SlowCheck`], a fixed 184-byte value with
//! no allocation, and asks it for its next deadline
//! ([`SlowCheck::next_check`]) to put on its timer beside its idle and
//! throttle deadlines, as pacerd put the earliest of them on its timer
//! heap. Nothing here reads a clock or keeps a timer of its own.

use crate::config::MinRate;
use crate::window::Window;

/// Buckets in a connection's window: pacerd's 10.
pub const CHECK_SLOTS: u32 = 10;

/// What a check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// Not due yet, or the connection moved enough or was shaped: check
    /// again at `next_ns`.
    Pass { next_ns: u64 },
    /// The connection moved fewer bytes than the minimum in the last
    /// window: close it as `slow_client`.
    Cut,
    /// The minimum was switched off by a reload: no more checks.
    Off,
}

/// One connection's slow-client state. See the [module docs](self).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlowCheck {
    moved: Window,
    /// When the next check is due.
    next_ns: u64,
    /// When the latest throttle ended, or 0 if never throttled.
    last_throttle_ns: u64,
}

/// A window length in nanoseconds, saturating.
fn window_ns(m: &MinRate) -> u64 {
    u64::try_from(m.window.as_nanos()).unwrap_or(u64::MAX)
}

impl SlowCheck {
    /// The check for a connection opened at `now_ns` on a listener with
    /// minimum `min`, or `None` when there is nothing to check: a minimum
    /// of 0 bytes, or a window too short to cut into [`CHECK_SLOTS`]
    /// buckets (the config reader never allows one).
    pub fn new(min: &MinRate, now_ns: u64) -> Option<Self> {
        if min.bytes == 0 {
            return None;
        }
        let w = window_ns(min);
        let moved = Window::new(w, CHECK_SLOTS, now_ns).ok()?;
        Some(Self {
            moved,
            next_ns: now_ns.saturating_add(w),
            last_throttle_ns: 0,
        })
    }

    /// Count `n` bytes moved at `now_ns`, in either direction.
    pub fn moved(&mut self, now_ns: u64, n: u64) {
        self.moved.add(now_ns, n);
    }

    /// Record that shaping held the connection back until `until_ns`.
    pub fn throttled(&mut self, until_ns: u64) {
        self.last_throttle_ns = self.last_throttle_ns.max(until_ns);
    }

    /// When the next check is due.
    pub fn next_check(&self) -> u64 {
        self.next_ns
    }

    /// Check the connection at `now_ns` against `current`, the listener's
    /// minimum as it is now (`None` once a reload switched it off).
    pub fn check(&mut self, now_ns: u64, current: Option<&MinRate>) -> Check {
        if now_ns < self.next_ns {
            return Check::Pass {
                next_ns: self.next_ns,
            };
        }
        let Some(min) = current.filter(|m| m.bytes != 0) else {
            return Check::Off;
        };
        let window = window_ns(min);
        self.next_ns = now_ns.saturating_add((window / 10).max(1));
        let pass = Check::Pass {
            next_ns: self.next_ns,
        };
        if self.last_throttle_ns != 0 && self.last_throttle_ns.saturating_add(window) > now_ns {
            return pass;
        }
        if self.moved.count(now_ns) < min.bytes {
            Check::Cut
        } else {
            pass
        }
    }
}

#[cfg(test)]
mod tests;
