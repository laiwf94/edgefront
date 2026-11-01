//! SipHash (Aumasson and Bernstein, 2012), ported from libpacer's
//! `siphash.c`, and a source of random keys.
//!
//! The keyed table hashes client keys under a secret per-table key so that
//! nobody can choose keys that collide into one probe chain or one shard.
//! libpacer used SipHash-2-4; edgefront defaults to SipHash-1-3, the variant
//! Rust's standard hash maps use, which has two fewer rounds per block and
//! one fewer in finalization and keeps the same keyed-PRF role for short
//! inputs. SipHash-2-4 stays available so a table can place keys exactly
//! where libpacer would, which the differential tests rely on.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

/// Bytes in a SipHash key.
pub const KEY_LEN: usize = 16;

/// Which SipHash variant a keyed table uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum KeyHash {
    /// SipHash-1-3: one compression round per block, three to finalize.
    #[default]
    Sip13,
    /// SipHash-2-4, libpacer's choice; places keys exactly where the C
    /// table does.
    Sip24,
}

impl KeyHash {
    /// Hash `data` under `key` with this variant.
    #[inline]
    pub fn hash(self, key: &[u8; KEY_LEN], data: &[u8]) -> u64 {
        match self {
            KeyHash::Sip13 => siphash13(key, data),
            KeyHash::Sip24 => siphash24(key, data),
        }
    }
}

#[derive(Clone, Copy)]
struct State {
    v0: u64,
    v1: u64,
    v2: u64,
    v3: u64,
}

impl State {
    #[inline(always)]
    fn new(key: &[u8; KEY_LEN]) -> Self {
        let (halves, _) = key.as_chunks::<8>();
        let k0 = u64::from_le_bytes(halves[0]);
        let k1 = u64::from_le_bytes(halves[1]);
        Self {
            v0: k0 ^ 0x736f_6d65_7073_6575,
            v1: k1 ^ 0x646f_7261_6e64_6f6d,
            v2: k0 ^ 0x6c79_6765_6e65_7261,
            v3: k1 ^ 0x7465_6462_7974_6573,
        }
    }

    #[inline(always)]
    fn round(&mut self) {
        self.v0 = self.v0.wrapping_add(self.v1);
        self.v1 = self.v1.rotate_left(13);
        self.v1 ^= self.v0;
        self.v0 = self.v0.rotate_left(32);
        self.v2 = self.v2.wrapping_add(self.v3);
        self.v3 = self.v3.rotate_left(16);
        self.v3 ^= self.v2;
        self.v0 = self.v0.wrapping_add(self.v3);
        self.v3 = self.v3.rotate_left(21);
        self.v3 ^= self.v0;
        self.v2 = self.v2.wrapping_add(self.v1);
        self.v1 = self.v1.rotate_left(17);
        self.v1 ^= self.v2;
        self.v2 = self.v2.rotate_left(32);
    }

    #[inline(always)]
    fn compress<const C: usize>(&mut self, m: u64) {
        self.v3 ^= m;
        for _ in 0..C {
            self.round();
        }
        self.v0 ^= m;
    }
}

/// SipHash-c-d of `data` under `key`.
#[inline(always)]
fn siphash<const C: usize, const D: usize>(key: &[u8; KEY_LEN], data: &[u8]) -> u64 {
    let mut s = State::new(key);
    let (blocks, rest) = data.as_chunks::<8>();
    for block in blocks {
        s.compress::<C>(u64::from_le_bytes(*block));
    }
    // Last block: the remaining bytes, zero padding, and the length's low
    // byte in the top position.
    let mut tail = [0u8; 8];
    tail[..rest.len()].copy_from_slice(rest);
    tail[7] = data.len() as u8;
    s.compress::<C>(u64::from_le_bytes(tail));
    s.v2 ^= 0xff;
    for _ in 0..D {
        s.round();
    }
    s.v0 ^ s.v1 ^ s.v2 ^ s.v3
}

/// SipHash-1-3 of `data` under `key`.
pub fn siphash13(key: &[u8; KEY_LEN], data: &[u8]) -> u64 {
    siphash::<1, 3>(key, data)
}

/// SipHash-2-4 of `data` under `key`, as libpacer's `pacer_siphash24`.
pub fn siphash24(key: &[u8; KEY_LEN], data: &[u8]) -> u64 {
    siphash::<2, 4>(key, data)
}

/// A fresh secret key.
///
/// libpacer read the kernel CSPRNG with `getrandom`. The crate takes no
/// dependencies and forbids unsafe code, so the bytes come from the
/// standard library's `RandomState`, which seeds itself from the operating
/// system's random source and differs on every call; each half of the key
/// is its SipHash output for a distinct input, so it cannot be predicted
/// without the seed.
pub fn random_key() -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    for (i, half) in key.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_usize(i);
        *half = h.finish().to_le_bytes();
    }
    key
}

#[cfg(test)]
mod tests {
    //! The reference vectors from libpacer's tests/test_siphash.c, the
    //! matching SipHash-1-3 vectors, and the key checks.

    use super::*;

    /// SipHash-2-4 of messages 00, 00 01, ... of length 0 to 63 under key
    /// 00..0f, as published with the reference implementation.
    const VECTORS_24: [u64; 64] = [
        0x726fdb47dd0e0e31,
        0x74f839c593dc67fd,
        0x0d6c8009d9a94f5a,
        0x85676696d7fb7e2d,
        0xcf2794e0277187b7,
        0x18765564cd99a68d,
        0xcbc9466e58fee3ce,
        0xab0200f58b01d137,
        0x93f5f5799a932462,
        0x9e0082df0ba9e4b0,
        0x7a5dbbc594ddb9f3,
        0xf4b32f46226bada7,
        0x751e8fbc860ee5fb,
        0x14ea5627c0843d90,
        0xf723ca908e7af2ee,
        0xa129ca6149be45e5,
        0x3f2acc7f57c29bdb,
        0x699ae9f52cbe4794,
        0x4bc1b3f0968dd39c,
        0xbb6dc91da77961bd,
        0xbed65cf21aa2ee98,
        0xd0f2cbb02e3b67c7,
        0x93536795e3a33e88,
        0xa80c038ccd5ccec8,
        0xb8ad50c6f649af94,
        0xbce192de8a85b8ea,
        0x17d835b85bbb15f3,
        0x2f2e6163076bcfad,
        0xde4daaaca71dc9a5,
        0xa6a2506687956571,
        0xad87a3535c49ef28,
        0x32d892fad841c342,
        0x7127512f72f27cce,
        0xa7f32346f95978e3,
        0x12e0b01abb051238,
        0x15e034d40fa197ae,
        0x314dffbe0815a3b4,
        0x027990f029623981,
        0xcadcd4e59ef40c4d,
        0x9abfd8766a33735c,
        0x0e3ea96b5304a7d0,
        0xad0c42d6fc585992,
        0x187306c89bc215a9,
        0xd4a60abcf3792b95,
        0xf935451de4f21df2,
        0xa9538f0419755787,
        0xdb9acddff56ca510,
        0xd06c98cd5c0975eb,
        0xe612a3cb9ecba951,
        0xc766e62cfcadaf96,
        0xee64435a9752fe72,
        0xa192d576b245165a,
        0x0a8787bf8ecb74b2,
        0x81b3e73d20b49b6f,
        0x7fa8220ba3b2ecea,
        0x245731c13ca42499,
        0xb78dbfaf3a8d83bd,
        0xea1ad565322a1a0b,
        0x60e61c23a3795013,
        0x6606d7e446282b93,
        0x6ca4ecb15c5f91e1,
        0x9f626da15c9625f3,
        0xe51b38608ef25f57,
        0x958a324ceb064572,
    ];

    /// The same inputs under SipHash-1-3.
    const VECTORS_13: [u64; 64] = [
        0xabac0158050fc4dc,
        0xc9f49bf37d57ca93,
        0x82cb9b024dc7d44d,
        0x8bf80ab8e7ddf7fb,
        0xcf75576088d38328,
        0xdef9d52f49533b67,
        0xc50d2b50c59f22a7,
        0xd3927d989bb11140,
        0x369095118d299a8e,
        0x25a48eb36c063de4,
        0x79de85ee92ff097f,
        0x70c118c1f94dc352,
        0x78a384b157b4d9a2,
        0x306f760c1229ffa7,
        0x605aa111c0f95d34,
        0xd320d86d2a519956,
        0xcc4fdd1a7d908b66,
        0x9cf2689063dbd80c,
        0x8ffc389cb473e63e,
        0xf21f9de58d297d1c,
        0xc0dc2f46a6cce040,
        0xb992abfe2b45f844,
        0x7ffe7b9ba320872e,
        0x525a0e7fdae6c123,
        0xf464aeb267349c8c,
        0x45cd5928705b0979,
        0x3a3e35e3ca9913a5,
        0xa91dc74e4ade3b35,
        0xfb0bed02ef6cd00d,
        0x88d93cb44ab1e1f4,
        0x540f11d643c5e663,
        0x2370dd1f8c21d1bc,
        0x81157b6c16a7b60d,
        0x4d54b9e57a8ff9bf,
        0x759f12781f2a753e,
        0xcea1a3bebf186b91,
        0x2cf508d3ada26206,
        0xb6101c2da3c33057,
        0xb3f47496ae3a36a1,
        0x626b57547b108392,
        0xc1d2363299e41531,
        0x667cc1923f1ad944,
        0x65704ffec8138825,
        0x24f280d1c28949a6,
        0xc2ca1cedfaf8876b,
        0xc2164bfc9f042196,
        0xa16e9c9368b1d623,
        0x49fb169c8b5114fd,
        0x9f3143f8df074c46,
        0xc6fdaf2412cc86b3,
        0x7eaf49d10a52098f,
        0x1cf313559d292f9a,
        0xc44a30dda2f41f12,
        0x36fae98943a71ed0,
        0x318fb34c73f0bce6,
        0xa27abf3670a7e980,
        0xb4bcc0db243c6d75,
        0x23f8d852fdb71513,
        0x8f035f4da67d8a08,
        0xd89cd0e5b7e8f148,
        0xf6f4e6bcf7a644ee,
        0xaec59ad80f1837f2,
        0xc3b2f6154b6694e0,
        0x9d199062b7bbb3a8,
    ];

    fn inputs() -> ([u8; KEY_LEN], [u8; 64]) {
        (
            std::array::from_fn(|i| i as u8),
            std::array::from_fn(|i| i as u8),
        )
    }

    #[test]
    fn siphash24_matches_reference_vectors() {
        let (key, msg) = inputs();
        for (len, want) in VECTORS_24.iter().enumerate() {
            assert_eq!(siphash24(&key, &msg[..len]), *want, "length {len}");
            assert_eq!(KeyHash::Sip24.hash(&key, &msg[..len]), *want);
        }
    }

    #[test]
    fn siphash13_matches_reference_vectors() {
        let (key, msg) = inputs();
        for (len, want) in VECTORS_13.iter().enumerate() {
            assert_eq!(siphash13(&key, &msg[..len]), *want, "length {len}");
            assert_eq!(KeyHash::Sip13.hash(&key, &msg[..len]), *want);
        }
    }

    #[test]
    #[allow(deprecated)]
    fn agrees_with_the_standard_library() {
        // DefaultHasher is SipHash-1-3 and the deprecated SipHasher is
        // SipHash-2-4, both under the all-zero key when built with new(),
        // and write() feeds the bytes with no length prefix.
        let zero = [0u8; KEY_LEN];
        for msg in [&b""[..], b"192.0.2.1", b"2001:db8::1", &[0xa5; 40]] {
            let mut h13 = std::collections::hash_map::DefaultHasher::new();
            h13.write(msg);
            assert_eq!(siphash13(&zero, msg), h13.finish());
            let mut h24 = std::hash::SipHasher::new();
            h24.write(msg);
            assert_eq!(siphash24(&zero, msg), h24.finish());
        }
    }

    #[test]
    fn depends_on_key() {
        let a = [0u8; KEY_LEN];
        let mut b = [0u8; KEY_LEN];
        b[15] = 1;
        let msg = b"192.0.2.1";
        assert_ne!(siphash24(&a, msg), siphash24(&b, msg));
        assert_ne!(siphash13(&a, msg), siphash13(&b, msg));
    }

    #[test]
    fn random_keys_are_not_zero_and_differ() {
        let a = random_key();
        let b = random_key();
        assert_ne!(a, [0u8; KEY_LEN]);
        assert_ne!(a, b);
        assert_ne!(a[..8], a[8..]);
    }
}
