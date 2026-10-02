//! Property-based tests for the bloom filter in `tracelake_core::bloom`.

use proptest::prelude::*;
use tracelake_core::{BloomFilter, DEFAULT_FALSE_POSITIVE_RATE};

/// Arbitrary byte key of bounded length.
fn byte_key() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..=32)
}

proptest! {
    /// B-NFN: no false negatives — every inserted key is contained, in the
    /// live filter and after an encode/decode round-trip, even when the
    /// filter was sized for fewer items than inserted.
    #[test]
    fn inserted_keys_never_falsely_drop_out(
        expected_items in proptest::sample::select([0usize, 1, 8, 64].as_slice()),
        keys in prop::collection::vec(byte_key(), 0..=64),
    ) {
        let mut filter =
            BloomFilter::new(expected_items, DEFAULT_FALSE_POSITIVE_RATE)
                .expect("valid rate builds a filter");
        for key in &keys {
            filter.insert(key);
        }
        for key in &keys {
            prop_assert!(filter.contains(key));
        }

        let decoded = BloomFilter::decode(&filter.encode())
            .expect("encoded filters decode");
        for key in &keys {
            prop_assert!(decoded.contains(key));
        }
    }

    /// B-RT: encode/decode preserves membership exactly — for inserted keys
    /// (must contain) and for genuinely absent keys (same answer as the
    /// original filter, so no false negatives and no new false positives).
    #[test]
    fn encode_decode_preserves_membership_exactly(
        inserted in prop::collection::vec(byte_key(), 1..=32),
        probes in prop::collection::vec(byte_key(), 1..=32),
    ) {
        let mut filter =
            BloomFilter::new(inserted.len(), DEFAULT_FALSE_POSITIVE_RATE)
                .expect("valid rate builds a filter");
        for key in &inserted {
            filter.insert(key);
        }

        let decoded = BloomFilter::decode(&filter.encode())
            .expect("encoded filters decode");

        for key in &inserted {
            prop_assert!(decoded.contains(key));
        }
        for key in &probes {
            if !inserted.contains(key) {
                prop_assert_eq!(decoded.contains(key), filter.contains(key));
            }
        }
    }

    /// B-REJ: decode is total on arbitrary bytes, and every filter it
    /// accepts answers contains() safely on arbitrary keys.
    #[test]
    fn decode_never_panics_on_arbitrary_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..=64),
        probe in byte_key(),
    ) {
        if let Ok(filter) = BloomFilter::decode(&bytes) {
            let _ = filter.contains(&probe);
        }
    }

    /// B-FPR: the false positive rate stays well above nothing and well
    /// below the rejection bound. Target rate 0.01; the assertion allows
    /// 0.05 because a 1024-probe sample of a 1% filter exceeds 5% with
    /// probability ~1e-15. The hash is an unseeded DefaultHasher, so results
    /// are deterministic for a fixed corpus; only the corpus is random here.
    #[test]
    fn false_positive_rate_stays_far_below_reject_bound(
        inserted_salt in any::<u32>(),
        absent_salt in any::<u32>(),
    ) {
        const ITEMS: usize = 1024;

        let inserted: Vec<String> =
            (0..ITEMS).map(|i| format!("real-{inserted_salt}-{i}")).collect();
        let absent: Vec<String> =
            (0..ITEMS).map(|i| format!("fake-{absent_salt}-{i}")).collect();

        let mut filter =
            BloomFilter::new(ITEMS, DEFAULT_FALSE_POSITIVE_RATE)
                .expect("valid rate builds a filter");
        for key in &inserted {
            filter.insert(key.as_bytes());
        }
        for key in &inserted {
            prop_assert!(filter.contains(key.as_bytes()));
        }

        let false_positives = absent
            .iter()
            .filter(|key| filter.contains(key.as_bytes()))
            .count();
        let rate = false_positives as f64 / ITEMS as f64;
        prop_assert!(rate < 0.05, "false positive rate {rate} at {false_positives}/{ITEMS}");
    }
}
