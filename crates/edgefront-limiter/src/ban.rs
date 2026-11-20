//! Bans: keys refused outright until a time, set automatically (a key
//! boxed by a [`crate::penalty::PenaltyBox`] too often) or by an operator.
//!
//! [`BanList::is_banned`] compares the ban's end with the caller's clock,
//! so a ban ends exactly when it says, whatever the sweeping does. An
//! expiry wheel ([`BanList::sweep`]) of 1,024 slots of `tick_ns` drops
//! ended bans in time proportional to the bans ending, not to the list;
//! a ban further out than one turn of the wheel waits in its slot for the
//! turn it ends in.
//!
//! Bans are kept apart from the config: a reload builds new limits around
//! the same list, so bans survive it. They also survive a restart through
//! [`BanList::save`] and [`BanList::load`], a text file of one ban a line
//! (`<end in Unix ms> <key as hex> <reason>`), written to a temporary
//! file and renamed so a crash never leaves half a list. Times in the
//! file are wall-clock, since a monotonic clock starts over with the
//! process; [`BanList::load`] converts them with the caller's offset.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

const SLOTS: usize = 1024;

/// Why a key is banned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ban {
    pub until_ns: u64,
    /// `manual`, `penalty`, or what the operator wrote; no spaces.
    pub reason: String,
}

struct Inner {
    bans: HashMap<Vec<u8>, Ban>,
    wheel: Vec<Vec<Vec<u8>>>,
    /// The tick the wheel has swept up to.
    swept: u64,
}

/// The bans, by key.
pub struct BanList {
    tick_ns: u64,
    max: usize,
    inner: Mutex<Inner>,
    /// The number of bans held, readable without the lock.
    held: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for BanList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BanList")
            .field("bans", &self.len())
            .finish()
    }
}

impl BanList {
    /// A list of at most `max` bans swept in ticks of `tick_ns`.
    pub fn new(tick_ns: u64, max: usize) -> Self {
        BanList {
            tick_ns: tick_ns.max(1),
            max,
            inner: Mutex::new(Inner {
                bans: HashMap::new(),
                wheel: vec![Vec::new(); SLOTS],
                swept: 0,
            }),
            held: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Bans `key` until `until_ns`, or extends a ban already there to
    /// the later of the two. Refused when the list is full.
    pub fn ban(&self, key: &[u8], until_ns: u64, reason: &str) -> Result<(), String> {
        if reason.is_empty()
            || reason
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(format!("reason {reason:?} must be one word"));
        }
        let mut g = self.lock();
        if !g.bans.contains_key(key) && g.bans.len() >= self.max {
            return Err(format!("the ban list is full ({} bans)", self.max));
        }
        let slot = ((until_ns / self.tick_ns) as usize) % SLOTS;
        let e = g.bans.entry(key.to_vec()).or_insert(Ban {
            until_ns: 0,
            reason: String::new(),
        });
        if until_ns >= e.until_ns {
            e.until_ns = until_ns;
            e.reason = reason.to_owned();
        }
        g.wheel[slot].push(key.to_vec());
        self.held
            .store(g.bans.len(), std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Lifts `key`'s ban; whether there was one.
    pub fn unban(&self, key: &[u8]) -> bool {
        let mut g = self.lock();
        let had = g.bans.remove(key).is_some();
        self.held
            .store(g.bans.len(), std::sync::atomic::Ordering::Relaxed);
        had
    }

    /// Whether any ban is held, without taking the lock: the request
    /// path's check before it looks a key up.
    #[inline]
    pub fn any(&self) -> bool {
        self.held.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    /// `key`'s ban, if it has one in force at `now_ns`.
    pub fn is_banned(&self, key: &[u8], now_ns: u64) -> Option<Ban> {
        self.lock()
            .bans
            .get(key)
            .filter(|b| b.until_ns > now_ns)
            .cloned()
    }

    /// Whether the list still holds an entry for `key`, ended or not.
    pub fn holds(&self, key: &[u8]) -> bool {
        self.lock().bans.contains_key(key)
    }

    /// Drops bans ended by `now_ns`; how many.
    pub fn sweep(&self, now_ns: u64) -> usize {
        let mut g = self.lock();
        let now_tick = now_ns / self.tick_ns;
        if g.swept == 0 {
            g.swept = now_tick.saturating_sub(SLOTS as u64);
        }
        let mut dropped = 0;
        // Only whole ticks that have passed: a ban ending later in the
        // current tick stays for the next sweep.
        let from = g.swept.max(now_tick.saturating_sub(SLOTS as u64));
        for tick in from..now_tick {
            let slot = (tick as usize) % SLOTS;
            let keys = std::mem::take(&mut g.wheel[slot]);
            let mut keep = Vec::new();
            for k in keys {
                match g.bans.get(&k) {
                    Some(b) if b.until_ns <= now_ns => {
                        g.bans.remove(&k);
                        dropped += 1;
                    }
                    // A later ban, in this slot for a later turn.
                    Some(b) if (b.until_ns / self.tick_ns) as usize % SLOTS == slot => keep.push(k),
                    // Lifted, or moved to another slot by an extension.
                    _ => {}
                }
            }
            g.wheel[slot] = keep;
        }
        g.swept = now_tick;
        self.held
            .store(g.bans.len(), std::sync::atomic::Ordering::Relaxed);
        dropped
    }

    pub fn len(&self) -> usize {
        self.lock().bans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every ban, sorted by key.
    pub fn list(&self) -> Vec<(Vec<u8>, Ban)> {
        let mut v: Vec<_> = self
            .lock()
            .bans
            .iter()
            .map(|(k, b)| (k.clone(), b.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Writes the bans in force at `now_ns` to `path`, ends as Unix
    /// milliseconds: `wall_ms = until_ns / 1e6 + offset_ms`.
    pub fn save(&self, path: &Path, now_ns: u64, offset_ms: i64) -> std::io::Result<usize> {
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        let mut n = 0;
        for (k, b) in self.list() {
            if b.until_ns <= now_ns {
                continue;
            }
            let wall = i64::try_from(b.until_ns / 1_000_000)
                .unwrap_or(i64::MAX)
                .saturating_add(offset_ms);
            let hex: String = k.iter().map(|x| format!("{x:02x}")).collect();
            writeln!(f, "{wall} {hex} {}", b.reason)?;
            n += 1;
        }
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(n)
    }

    /// Adds the bans in `path` still in force, converting with the same
    /// offset [`BanList::save`] used; a bad line is an error naming it.
    pub fn load(&self, path: &Path, now_ns: u64, offset_ms: i64) -> Result<usize, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut n = 0;
        for (i, line) in text.lines().enumerate() {
            let bad = || {
                format!(
                    "{}: line {}: expected `<unix ms> <hex key> <reason>`",
                    path.display(),
                    i + 1
                )
            };
            let mut parts = line.split(' ');
            let (Some(wall), Some(hex), Some(reason), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return Err(bad());
            };
            let wall: i64 = wall.parse().map_err(|_| bad())?;
            if hex.len() % 2 != 0 || hex.is_empty() {
                return Err(bad());
            }
            let key: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|j| u8::from_str_radix(&hex[j..j + 2], 16))
                .collect::<Result<_, _>>()
                .map_err(|_| bad())?;
            let until_ms = wall.saturating_sub(offset_ms);
            let Ok(until_ms) = u64::try_from(until_ms) else {
                continue;
            };
            let until_ns = until_ms.saturating_mul(1_000_000);
            if until_ns <= now_ns {
                continue;
            }
            self.ban(&key, until_ns, reason)?;
            n += 1;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const SEC: u64 = 1_000_000_000;

    #[test]
    fn bans_end_exactly_on_time_and_are_swept() {
        let b = BanList::new(10 * MS, 1000);
        let t = 100 * SEC;
        b.ban(b"a", t + 500 * MS, "manual").unwrap();
        b.ban(b"b", t + 2 * SEC, "penalty").unwrap();
        // Far beyond one turn of the wheel (10.24 s).
        b.ban(b"c", t + 60 * SEC, "manual").unwrap();
        assert!(b.is_banned(b"a", t + 500 * MS - 1).is_some());
        assert!(
            b.is_banned(b"a", t + 500 * MS).is_none(),
            "the ban ends on its time"
        );
        assert_eq!(b.is_banned(b"b", t).unwrap().reason, "penalty");
        assert_eq!(b.sweep(t + 510 * MS), 1);
        assert_eq!(b.len(), 2);
        // Many turns later, only what has ended goes.
        for s in 1..=59 {
            b.sweep(t + s * SEC);
        }
        assert_eq!(b.len(), 1);
        assert!(b.is_banned(b"c", t + 59 * SEC).is_some());
        assert_eq!(b.sweep(t + 61 * SEC), 1);
        assert!(b.is_empty());
    }

    /// Expiry accuracy: 1,000 bans with random ends over 20 s, swept every
    /// 100 ms; each is gone from the list within 100 ms of its end, and
    /// never refused a key after it.
    #[test]
    fn a_thousand_bans_expire_within_a_sweep() {
        let b = BanList::new(10 * MS, 10_000);
        let start = 10 * SEC;
        let mut x = 99u64;
        let mut ends = Vec::new();
        for i in 0..1000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let end = start + x % (20 * SEC);
            b.ban(&i.to_be_bytes(), end, "manual").unwrap();
            ends.push(end);
        }
        let mut now = start;
        while now < start + 21 * SEC {
            now += 100 * MS;
            b.sweep(now);
            for (i, &end) in ends.iter().enumerate() {
                let key = (i as u32).to_be_bytes();
                let held = b.holds(&key);
                if end + 100 * MS <= now {
                    assert!(!held, "ban {i} ended at {end}, still held at {now}");
                }
                assert_eq!(b.is_banned(&key, now).is_some(), end > now);
            }
        }
        assert!(b.is_empty());
    }

    #[test]
    fn extensions_lifts_and_limits() {
        let b = BanList::new(MS, 2);
        b.ban(b"a", 5 * SEC, "manual").unwrap();
        b.ban(b"a", 9 * SEC, "penalty").unwrap();
        assert_eq!(
            b.is_banned(b"a", 6 * SEC).unwrap(),
            Ban {
                until_ns: 9 * SEC,
                reason: "penalty".into()
            }
        );
        // An earlier end does not shorten a ban.
        b.ban(b"a", 7 * SEC, "manual").unwrap();
        assert_eq!(b.is_banned(b"a", 8 * SEC).unwrap().until_ns, 9 * SEC);
        b.ban(b"b", SEC, "x").unwrap();
        assert_eq!(
            b.ban(b"c", SEC, "x").unwrap_err(),
            "the ban list is full (2 bans)"
        );
        assert!(b.unban(b"b") && !b.unban(b"b"));
        assert!(b.ban(b"c", SEC, "two words").is_err());
        assert!(b.ban(b"c", SEC, "").is_err());
        // Swept at the old end, the extended ban stays.
        b.sweep(5 * SEC + MS);
        assert!(b.is_banned(b"a", 6 * SEC).is_some());
        assert!(format!("{b:?}").contains("bans: 1"));
    }

    #[test]
    fn bans_survive_a_restart_through_their_file() {
        let dir = std::env::temp_dir().join(format!("bans-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bans.txt");
        let b = BanList::new(MS, 100);
        let now = 50 * SEC;
        b.ban(&[0x0a, 0, 0, 1], now + 30 * SEC, "manual").unwrap();
        b.ban(b"key", now + 90 * SEC, "penalty").unwrap();
        b.ban(b"gone", now - 1, "manual").unwrap();
        // The process's clock started 1,000 s after the Unix epoch's 1.8e12 ms.
        let offset = 1_800_000_000_000i64;
        assert_eq!(b.save(&path, now, offset).unwrap(), 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "1800000080000 0a000001 manual\n1800000140000 6b6579 penalty\n"
        );
        // A new process whose clock is 10 s in: the same wall times.
        let c = BanList::new(MS, 100);
        let offset2 = offset + 40_000;
        assert_eq!(c.load(&path, 10 * SEC, offset2).unwrap(), 2);
        assert_eq!(c.is_banned(b"key", 10 * SEC).unwrap().until_ns, 100 * SEC);
        // Read after 100 s more, only the later ban is left.
        let d = BanList::new(MS, 100);
        assert_eq!(d.load(&path, 60 * SEC, offset2).unwrap(), 1);
        std::fs::write(&path, "x 00 manual\n").unwrap();
        assert!(d.load(&path, 0, 0).unwrap_err().contains("line 1"));
        std::fs::write(&path, "1 0 manual\n").unwrap();
        assert!(d.load(&path, 0, 0).is_err());
        std::fs::write(&path, "1 zz manual\n").unwrap();
        assert!(d.load(&path, 0, 0).is_err());
        assert!(d.load(&dir.join("none"), 0, 0).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
