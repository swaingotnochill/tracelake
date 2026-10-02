use thiserror::Error;

/// Errors returned while encoding or decoding a bloom filter.
#[derive(Debug, Error, PartialEq)]
pub enum BloomError {
    #[error("false positive rate must be between 0 and 1, got {rate}")]
    InvalidFalsePositiveRate { rate: f64 },

    #[error("bloom filter truncated: {len} bytes")]
    Truncated { len: usize },

    #[error("bloom filter bit count {bits} does not match {words} stored words")]
    BitCountMismatch { bits: u64, words: u64 },
}

/// A fixed-size bloom filter over byte keys.
///
/// Uses k hash positions derived from two base hashes (double hashing). Item
/// types are not namespaced: the filter only answers "might this key have been
/// inserted", so cross-type matches cost a wasted read, never a wrong result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BloomFilter {
    bits: Vec<u64>,
    num_bits: u64,
    num_hashes: u32,
}

/// Default target false positive rate used when building segment blooms.
pub const DEFAULT_FALSE_POSITIVE_RATE: f64 = 0.01;

impl BloomFilter {
    /// Creates an empty filter sized for `expected_items` at roughly the given
    /// false positive rate.
    pub fn new(expected_items: usize, false_positive_rate: f64) -> Result<Self, BloomError> {
        if false_positive_rate <= 0.0 || false_positive_rate >= 1.0 {
            return Err(BloomError::InvalidFalsePositiveRate {
                rate: false_positive_rate,
            });
        }

        if expected_items == 0 {
            return Ok(Self {
                bits: vec![0u64],
                num_bits: 64,
                num_hashes: 1,
            });
        }

        let n = expected_items as f64;
        let ln2 = std::f64::consts::LN_2;
        let num_bits = (-n * false_positive_rate.ln() / (ln2 * ln2)).ceil() as u64;
        let num_bits = num_bits.max(64);
        let num_hashes = ((num_bits as f64 / n) * ln2).round() as u32;
        let num_hashes = num_hashes.clamp(1, 32);

        Ok(Self {
            bits: vec![0u64; num_bits.div_ceil(64) as usize],
            num_bits,
            num_hashes,
        })
    }

    /// Inserts one key into the filter.
    pub fn insert(&mut self, key: &[u8]) {
        let (hash_a, hash_b) = hash_key(key);

        for index in 0..self.num_hashes as u64 {
            let position = hash_a.wrapping_add(index.wrapping_mul(hash_b)) % self.num_bits;
            self.bits[position as usize / 64] |= 1u64 << (position % 64);
        }
    }

    /// Returns whether the key may have been inserted. False positives are
    /// possible; false negatives are not.
    pub fn contains(&self, key: &[u8]) -> bool {
        let (hash_a, hash_b) = hash_key(key);

        (0..self.num_hashes as u64).all(|index| {
            let position = hash_a.wrapping_add(index.wrapping_mul(hash_b)) % self.num_bits;
            self.bits[position as usize / 64] & (1u64 << (position % 64)) != 0
        })
    }

    /// Serializes the filter: `[num_bits u64][num_hashes u32][bit words]`,
    /// all little-endian.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(12 + self.bits.len() * 8);
        bytes.extend_from_slice(&self.num_bits.to_le_bytes());
        bytes.extend_from_slice(&self.num_hashes.to_le_bytes());
        for word in &self.bits {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// Decodes a filter from its bytes, validating the header and word count.
    pub fn decode(bytes: &[u8]) -> Result<Self, BloomError> {
        if bytes.len() < 12 {
            return Err(BloomError::Truncated { len: bytes.len() });
        }

        let num_bits = u64::from_le_bytes(bytes[..8].try_into().expect("8 byte slice"));
        let num_hashes = u32::from_le_bytes(bytes[8..12].try_into().expect("4 byte slice"));
        let words_bytes = &bytes[12..];

        if !words_bytes.len().is_multiple_of(8) || num_bits == 0 {
            return Err(BloomError::Truncated { len: bytes.len() });
        }

        let word_count = words_bytes.len() / 8;
        let expected_words = num_bits.div_ceil(64);
        if word_count as u64 != expected_words {
            return Err(BloomError::BitCountMismatch {
                bits: num_bits,
                words: word_count as u64,
            });
        }

        let bits = words_bytes
            .chunks_exact(8)
            .map(|word| u64::from_le_bytes(word.try_into().expect("8 byte chunk")))
            .collect();

        Ok(Self {
            bits,
            num_bits,
            num_hashes,
        })
    }
}

/// Derives two independent 64-bit hashes of a key with two seeded SipHash
/// instances.
fn hash_key(key: &[u8]) -> (u64, u64) {
    use std::hash::{Hash, Hasher};

    let mut hasher_a = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher_a);
    let hash_a = hasher_a.finish();

    let mut hasher_b = std::collections::hash_map::DefaultHasher::new();
    (hash_a, key).hash(&mut hasher_b);
    let hash_b = hasher_b.finish();

    (hash_a, hash_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: &[&str] = &[
        "trace-000001",
        "trace-000042",
        "span-000123",
        "tag:production",
        "trace-999999",
    ];

    #[test]
    fn inserted_keys_are_always_contained() {
        let mut filter =
            BloomFilter::new(KEYS.len(), DEFAULT_FALSE_POSITIVE_RATE).expect("filter should build");

        for key in KEYS {
            filter.insert(key.as_bytes());
        }

        for key in KEYS {
            assert!(
                filter.contains(key.as_bytes()),
                "key {key} should be contained"
            );
        }
    }

    #[test]
    fn false_positive_rate_stays_near_target() {
        let inserted: Vec<String> = (0..5_000).map(|i| format!("real-{i}")).collect();
        let absent: Vec<String> = (0..5_000).map(|i| format!("fake-{i}")).collect();

        let mut filter = BloomFilter::new(inserted.len(), DEFAULT_FALSE_POSITIVE_RATE)
            .expect("filter should build");
        for key in &inserted {
            filter.insert(key.as_bytes());
        }

        let false_positives = absent
            .iter()
            .filter(|key| filter.contains(key.as_bytes()))
            .count();

        let rate = false_positives as f64 / absent.len() as f64;
        assert!(
            rate < 0.05,
            "false positive rate {rate} too far above target {DEFAULT_FALSE_POSITIVE_RATE}"
        );
    }

    #[test]
    fn encode_decode_round_trips_membership() {
        let mut filter =
            BloomFilter::new(64, DEFAULT_FALSE_POSITIVE_RATE).expect("filter should build");
        for key in KEYS {
            filter.insert(key.as_bytes());
        }

        let decoded = BloomFilter::decode(&filter.encode()).expect("filter should decode");

        for key in KEYS {
            assert!(decoded.contains(key.as_bytes()));
        }
        assert!(!decoded.contains(b"never-inserted"));
    }

    #[test]
    fn empty_filter_contains_nothing() {
        let filter = BloomFilter::new(0, DEFAULT_FALSE_POSITIVE_RATE).expect("filter should build");

        let decoded = BloomFilter::decode(&filter.encode()).expect("filter should decode");

        assert!(!decoded.contains(b"anything"));
        assert!(!filter.contains(b"anything"));
    }

    #[test]
    fn invalid_rate_and_bytes_are_rejected() {
        let error = BloomFilter::new(10, 1.5).expect_err("rate above 1 should fail");
        assert_eq!(error, BloomError::InvalidFalsePositiveRate { rate: 1.5 });

        let error = BloomFilter::decode(&[0u8; 8]).expect_err("short bytes should fail");
        assert_eq!(error, BloomError::Truncated { len: 8 });

        let mut corrupt = BloomFilter::new(10, DEFAULT_FALSE_POSITIVE_RATE)
            .expect("filter should build")
            .encode();
        corrupt[0] = 0xff;
        let error = BloomFilter::decode(&corrupt).expect_err("mismatched words should fail");
        assert!(matches!(error, BloomError::BitCountMismatch { .. }));
    }
}

#[cfg(kani)]
mod verify {
    use super::*;

    /// Builds a four-word filter directly (256 bits): `BloomFilter::new` is
    /// skipped because its f64 sizing math uses `ln`, which is not modeled.
    fn fixed_filter(num_hashes: u32) -> BloomFilter {
        BloomFilter {
            bits: vec![0u64; 4],
            num_bits: 256,
            num_hashes,
        }
    }

    /// Proves no false negatives for any key and any hash count in the
    /// constructor's 1..=32 clamp: insert then contains always hits.
    #[kani::proof]
    #[kani::unwind(40)]
    fn insert_then_contains_never_loses_a_key() {
        let key: [u8; 8] = kani::any();
        let num_hashes: u32 = kani::any();
        kani::assume(num_hashes >= 1);
        kani::assume(num_hashes <= 32);

        let mut filter = fixed_filter(num_hashes);
        filter.insert(&key);
        assert!(filter.contains(&key));
    }

    /// Proves encode/decode preserves membership exactly on a filter with
    /// symbolic words: an inserted key survives, and an arbitrary probe key
    /// gets the same answer before and after the round-trip.
    #[kani::proof]
    #[kani::unwind(40)]
    fn encode_decode_preserves_contains() {
        let words: [u64; 4] = kani::any();
        let key: [u8; 8] = kani::any();
        let other: [u8; 8] = kani::any();

        let mut filter = BloomFilter {
            bits: words.to_vec(),
            num_bits: 256,
            num_hashes: 3,
        };
        let other_before = filter.contains(&other);
        filter.insert(&key);

        let decoded =
            BloomFilter::decode(&filter.encode()).expect("well-formed filters always decode");

        assert!(
            decoded.contains(&key),
            "no false negatives after round-trip"
        );
        assert_eq!(decoded.contains(&other), other_before);
    }

    /// Proves decode is total on arbitrary short bytes and that any filter
    /// it accepts answers contains safely (no out-of-bounds bit indexing).
    #[kani::proof]
    #[kani::unwind(40)]
    fn decode_is_total_on_short_bytes() {
        let bytes: [u8; 64] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= 64);

        if let Ok(filter) = BloomFilter::decode(&bytes[..len]) {
            let probe: [u8; 8] = kani::any();
            let _ = filter.contains(&probe);
        }
    }
}
