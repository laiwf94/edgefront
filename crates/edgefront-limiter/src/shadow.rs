//! Shadow mode: a limit that decides every request as it would if it
//! enforced, counts the ones it would have refused, and refuses none.
//!
//! A rule's [`Mode`] is `enforce` (the default), `shadow` or `off`. A
//! shadow rule takes its tokens like an enforcing one, so its table goes
//! through exactly the states an enforcing copy of it would on the same
//! traffic, and its would-block count is that copy's refusal count. A
//! request it would refuse is let through untouched: no status, header or
//! delay of it reaches the response, and no other rule's tokens are given
//! back on its account. [`Shadow`] keeps the count and the last
//! [`EXAMPLES`] would-be refusals (time, a hash of the key, the wait) for
//! an operator to look at, sampled one in `sample_every` so a flood does
//! not churn them.
//!
//! The mode is held in an atomic beside the rule, so moving a rule from
//! shadow to enforce changes nothing else: the same table, with every
//! key's state, starts refusing. `off` skips the rule altogether; turning
//! it back on finds the table as it was left.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// How a rule acts on what it decides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Mode {
    #[default]
    Enforce,
    Shadow,
    Off,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "enforce" => Some(Mode::Enforce),
            "shadow" => Some(Mode::Shadow),
            "off" => Some(Mode::Off),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Enforce => "enforce",
            Mode::Shadow => "shadow",
            Mode::Off => "off",
        }
    }

    fn code(self) -> u8 {
        self as u8
    }

    fn from_code(c: u8) -> Mode {
        match c {
            1 => Mode::Shadow,
            2 => Mode::Off,
            _ => Mode::Enforce,
        }
    }
}

/// How many would-be refusals a rule keeps.
pub const EXAMPLES: usize = 16;

/// One would-be refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Example {
    pub at_ns: u64,
    /// The key's SipHash, never the key: it may be a credential.
    pub key_hash: u64,
    pub retry_after_ns: u64,
}

/// A rule's mode, would-block count and examples.
#[derive(Debug)]
pub struct Shadow {
    mode: AtomicU8,
    would_block: AtomicU64,
    seen: AtomicU64,
    sample_every: u64,
    examples: Mutex<Vec<Example>>,
}

impl Shadow {
    pub fn new(mode: Mode, sample_every: u64) -> Self {
        Shadow {
            mode: AtomicU8::new(mode.code()),
            would_block: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            sample_every: sample_every.max(1),
            examples: Mutex::new(Vec::with_capacity(EXAMPLES)),
        }
    }

    #[inline]
    pub fn mode(&self) -> Mode {
        Mode::from_code(self.mode.load(Ordering::Relaxed))
    }

    /// Changes the mode; the table and counts stay as they are.
    pub fn set_mode(&self, mode: Mode) {
        self.mode.store(mode.code(), Ordering::Relaxed);
    }

    /// Counts a request the rule would have refused.
    pub fn would_block(&self, at_ns: u64, key_hash: u64, retry_after_ns: u64) {
        self.would_block.fetch_add(1, Ordering::Relaxed);
        if !self
            .seen
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.sample_every)
        {
            return;
        }
        let mut ex = self.examples.lock().unwrap_or_else(|e| e.into_inner());
        if ex.len() == EXAMPLES {
            ex.remove(0);
        }
        ex.push(Example {
            at_ns,
            key_hash,
            retry_after_ns,
        });
    }

    pub fn would_blocked(&self) -> u64 {
        self.would_block.load(Ordering::Relaxed)
    }

    /// The kept examples, oldest first.
    pub fn examples(&self) -> Vec<Example> {
        self.examples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_read_change_and_examples_are_sampled() {
        for m in [Mode::Enforce, Mode::Shadow, Mode::Off] {
            assert_eq!(Mode::parse(m.name()), Some(m));
            assert_eq!(Mode::from_code(m.code()), m);
        }
        assert_eq!(Mode::parse("log"), None);
        let s = Shadow::new(Mode::Shadow, 2);
        assert_eq!(s.mode(), Mode::Shadow);
        for i in 0..100 {
            s.would_block(i, i * 7, 5);
        }
        assert_eq!(s.would_blocked(), 100);
        let ex = s.examples();
        assert_eq!(ex.len(), EXAMPLES);
        assert_eq!(
            ex.last().unwrap().at_ns,
            98,
            "every second one, the newest kept"
        );
        s.set_mode(Mode::Enforce);
        assert_eq!(s.mode(), Mode::Enforce);
        assert_eq!(s.would_blocked(), 100, "a mode change keeps the count");
        assert_eq!(Mode::default(), Mode::Enforce);
    }
}
