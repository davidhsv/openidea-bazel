//! A fast, non-cryptographic hasher (the `FxHasher` of rustc) for the sync's maps. The sync looks
//! up hundreds of thousands of labels and jar paths per run inside a CPU-budgeted wasm sandbox,
//! where std's SipHash and ordered maps over long, prefix-sharing labels cost most of the budget.
//! The keys come from the user's own build, so hash flooding is not a concern.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

pub type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
pub type FxHashSet<K> = HashSet<K, BuildHasherDefault<FxHasher>>;

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Clone, Copy, Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(last));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

/// A 64-bit fingerprint of a string, for maps and sets that only ever look keys up: equal strings
/// give equal fingerprints, and distinct ones collide with probability about n²/2⁶⁵ — negligible
/// for the ~10⁴ labels and jars of a sync. Comparing a `u64` on every hit replaces a byte-wise
/// comparison of a long, prefix-sharing label or path, which in wasm costs more than the hashing.
pub fn fingerprint(s: &str) -> u64 {
    let mut h = FxHasher::default();
    h.write(s.as_bytes());
    h.write_usize(s.len());
    avalanche(h.finish())
}

/// [`fingerprint`] of a target key: its label and aspect ids.
pub fn key_fingerprint(key: &crate::types::TargetKey) -> u64 {
    let mut h = FxHasher::default();
    h.write(key.label.as_bytes());
    h.write_usize(key.label.len());
    for id in &key.aspect_ids {
        h.write(id.as_bytes());
        h.write_usize(id.len());
    }
    avalanche(h.finish())
}

/// The splitmix64 finalizer: spreads the multiplicative hash over all 64 bits.
fn avalanche(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_input_equal_hash_and_tails_count() {
        let hash = |b: &[u8]| {
            let mut h = FxHasher::default();
            h.write(b);
            h.finish()
        };
        assert_eq!(hash(b"//a/b:c"), hash(b"//a/b:c"));
        assert_ne!(hash(b"//a/b:c"), hash(b"//a/b:d"));
        assert_ne!(hash(b"12345678x"), hash(b"12345678y"));
        assert_ne!(fingerprint("//a:b"), fingerprint("//a:c"));
        assert_ne!(fingerprint("ab"), fingerprint("ab\0"));
        let key = |label: &str, ids: &[&str]| crate::types::TargetKey {
            label: label.into(),
            aspect_ids: ids.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(
            key_fingerprint(&key("//a:b", &[])),
            key_fingerprint(&key("//a:b", &[]))
        );
        assert_ne!(
            key_fingerprint(&key("//a:b", &[])),
            key_fingerprint(&key("//a:b", &["x"]))
        );
        assert_ne!(
            key_fingerprint(&key("//a:b", &["xy"])),
            key_fingerprint(&key("//a:b", &["x", "y"]))
        );
        let mut set = FxHashSet::default();
        assert!(set.insert("x".to_string()));
        assert!(!set.insert("x".to_string()));
    }
}
