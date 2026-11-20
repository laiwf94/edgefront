//! The segment store's index snapshot: `index.snap`, written on a clean
//! shutdown so the next start can skip scanning the segments.
//!
//! ```text
//! magic     8 bytes "EFIDX\0\0\x01"
//! segments  u32 count, then per segment: number u64, length u64, live u64
//! entries   u64 count, then per entry: key (u16 length + bytes),
//!           segment u64, offset u64, frame u64,
//!           tags (u16 count, then u16 length + bytes each)
//! sum       32 bytes SHA-256 of everything before it
//! ```
//!
//! Integers are big-endian. A snapshot is trusted only when its checksum
//! holds, every length in it is in bounds, and every segment it names is
//! still on disk and at least as long as it was ([`Snapshot::covers`]):
//! segments are append-only, so the bytes it describes are unchanged, and
//! anything past them is replayed.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::segment::{Loc, Segment};
use crate::object::Key;

/// The snapshot's file name, in the store's directory.
pub const SNAPSHOT: &str = "index.snap";
const MAGIC: &[u8; 8] = b"EFIDX\0\0\x01";

/// A decoded snapshot.
pub(crate) struct Snapshot {
    pub(crate) segments: BTreeMap<u64, Segment>,
    pub(crate) entries: Vec<(Key, Loc)>,
}

/// Encodes the index.
pub(crate) fn encode(map: &HashMap<Key, Loc>, segments: &BTreeMap<u64, Segment>) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + map.len() * 64);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(segments.len() as u32).to_be_bytes());
    for (n, seg) in segments {
        out.extend_from_slice(&n.to_be_bytes());
        out.extend_from_slice(&seg.len.to_be_bytes());
        out.extend_from_slice(&seg.live.to_be_bytes());
    }
    out.extend_from_slice(&(map.len() as u64).to_be_bytes());
    for (key, loc) in map {
        put16(&mut out, key.as_bytes());
        out.extend_from_slice(&loc.segment.to_be_bytes());
        out.extend_from_slice(&loc.offset.to_be_bytes());
        out.extend_from_slice(&loc.frame.to_be_bytes());
        out.extend_from_slice(&(loc.tags.len().min(u16::MAX as usize) as u16).to_be_bytes());
        for tag in loc.tags.iter().take(u16::MAX as usize) {
            put16(&mut out, tag.as_bytes());
        }
    }
    let sum = Sha256::digest(&out);
    out.extend_from_slice(&sum);
    out
}

fn put16(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len().min(u16::MAX as usize);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&bytes[..len]);
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.bytes.len() < n {
            return None;
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Some(head)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn str16(&mut self) -> Option<&'a str> {
        let len = self.u16()? as usize;
        std::str::from_utf8(self.take(len)?).ok()
    }
}

impl Snapshot {
    /// Decodes a snapshot, or `None` if it is damaged in any way.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Snapshot> {
        if bytes.len() < MAGIC.len() + 32 || &bytes[..MAGIC.len()] != MAGIC {
            return None;
        }
        let (body, sum) = bytes.split_at(bytes.len() - 32);
        if Sha256::digest(body)[..] != *sum {
            return None;
        }
        let mut r = Reader {
            bytes: &body[MAGIC.len()..],
        };
        let count = r.u32()? as usize;
        // Each segment takes 24 bytes; refuse counts the bytes cannot hold.
        if count > r.bytes.len() / 24 {
            return None;
        }
        let mut segments = BTreeMap::new();
        for _ in 0..count {
            let n = r.u64()?;
            let len = r.u64()?;
            let live = r.u64()?;
            segments.insert(n, Segment { len, live });
        }
        let count = r.u64()? as usize;
        // Each entry takes at least 28 bytes.
        if count > r.bytes.len() / 28 {
            return None;
        }
        let mut entries = Vec::with_capacity(count);
        // Most objects share a few surrogate keys: one allocation each.
        let mut interned: HashMap<&str, Arc<str>> = HashMap::new();
        for _ in 0..count {
            let key: Key = r.str16()?.into();
            let segment = r.u64()?;
            let offset = r.u64()?;
            let frame = r.u64()?;
            let ntags = r.u16()? as usize;
            let mut tags = Vec::with_capacity(ntags.min(64));
            for _ in 0..ntags {
                let tag = r.str16()?;
                let shared = interned.entry(tag).or_insert_with(|| Arc::from(tag));
                tags.push(shared.clone());
            }
            let seg = segments.get(&segment)?;
            if offset.checked_add(frame)? > seg.len {
                return None;
            }
            entries.push((
                key,
                Loc {
                    segment,
                    offset,
                    frame,
                    tags,
                },
            ));
        }
        r.bytes.is_empty().then_some(Snapshot { segments, entries })
    }

    /// Whether every segment the snapshot names is on disk, at least as
    /// long as the snapshot recorded.
    pub(crate) fn covers(&self, on_disk: &BTreeMap<u64, u64>) -> bool {
        self.segments
            .iter()
            .all(|(n, seg)| on_disk.get(n).is_some_and(|&len| len >= seg.len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (HashMap<Key, Loc>, BTreeMap<u64, Segment>) {
        let mut map = HashMap::new();
        map.insert(
            Key::from("h/a"),
            Loc {
                segment: 3,
                offset: 0,
                frame: 100,
                tags: vec!["t1".into(), "t2".into()],
            },
        );
        map.insert(
            Key::from("h/b"),
            Loc {
                segment: 4,
                offset: 10,
                frame: 20,
                tags: vec![],
            },
        );
        let mut segments = BTreeMap::new();
        segments.insert(
            3,
            Segment {
                len: 100,
                live: 100,
            },
        );
        segments.insert(4, Segment { len: 30, live: 20 });
        (map, segments)
    }

    #[test]
    fn round_trips() {
        let (map, segments) = sample();
        let snap = Snapshot::decode(&encode(&map, &segments)).unwrap();
        assert_eq!(snap.segments, segments);
        let decoded: HashMap<Key, Loc> = snap.entries.into_iter().collect();
        assert_eq!(decoded, map);
    }

    #[test]
    fn every_damage_is_refused() {
        let (map, segments) = sample();
        let bytes = encode(&map, &segments);
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x01;
            assert!(Snapshot::decode(&b).is_none(), "byte {i}");
        }
        for cut in [0, 7, 8, 20, bytes.len() - 1] {
            assert!(Snapshot::decode(&bytes[..cut]).is_none(), "cut at {cut}");
        }
        // A valid checksum over lengths that point past a segment.
        let mut bad = map.clone();
        bad.get_mut("h/b").unwrap().frame = 1000;
        assert!(Snapshot::decode(&encode(&bad, &segments)).is_none());
        let mut orphan = map.clone();
        orphan.get_mut("h/b").unwrap().segment = 9;
        assert!(Snapshot::decode(&encode(&orphan, &segments)).is_none());
    }

    #[test]
    fn covers_needs_every_segment_at_least_as_long() {
        let (map, segments) = sample();
        let snap = Snapshot::decode(&encode(&map, &segments)).unwrap();
        let on_disk: BTreeMap<u64, u64> = [(3, 100), (4, 31), (5, 7)].into();
        assert!(snap.covers(&on_disk));
        let short: BTreeMap<u64, u64> = [(3, 100), (4, 29)].into();
        assert!(!snap.covers(&short));
        let missing: BTreeMap<u64, u64> = [(4, 30)].into();
        assert!(!snap.covers(&missing));
    }
}
