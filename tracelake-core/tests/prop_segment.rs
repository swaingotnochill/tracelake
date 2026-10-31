//! Property-based tests for the segment manifest in
//! `tracelake_core::segment`.

use chrono::{DateTime, Utc};
use proptest::prelude::*;
use tracelake_core::{
    FieldRange, RecordStats, SegmentEntry, SegmentFile, SegmentFileKind, SegmentManifest,
    SegmentStats, segment_id,
};

/// Fixed epoch (mid-2026); deltas stay inside RFC3339-safe nanosecond range.
const BASE_NANOS: i64 = 1_780_000_000_000_000_000;

/// Small fixed alphabet for strings (ids, paths).
const STRING_ALPHABET: &[char] = &['a', 'b', 'c', '0', '1', '-'];

/// Bounded arbitrary string: 1..=12 chars.
fn small_string() -> impl Strategy<Value = String> {
    prop::collection::vec(proptest::sample::select(STRING_ALPHABET), 1..=12)
        .prop_map(|chars| chars.into_iter().collect())
}

/// Timestamps within a ±13-day window around the base epoch.
fn datetime() -> impl Strategy<Value = DateTime<Utc>> {
    any::<i64>().prop_map(|delta| DateTime::from_timestamp_nanos(BASE_NANOS + (delta % (1 << 40))))
}

/// Finite f64 that round-trips through JSON (no NaN/inf, bounded precision).
fn rational() -> impl Strategy<Value = f64> {
    (
        any::<i16>(),
        proptest::sample::select([8u32, 16, 64, 256].as_slice()),
    )
        .prop_map(|(num, den)| num as f64 / den as f64)
}

fn stats() -> impl Strategy<Value = SegmentStats> {
    (
        0u64..=100_000,
        0u64..=100_000,
        proptest::option::of(datetime()),
        proptest::option::of(datetime()),
        proptest::option::of(datetime()),
        proptest::option::of(datetime()),
        proptest::option::of(0u64..=10_000),
        proptest::option::of(0u64..=10_000),
        proptest::option::of(rational()),
        proptest::option::of(rational()),
        proptest::option::of(rational()),
        proptest::option::of(rational()),
    )
        .prop_map(
            |(
                trace_count,
                span_count,
                start_time_min,
                start_time_max,
                end_time_min,
                end_time_max,
                latency_ms_min,
                latency_ms_max,
                cost_usd_min,
                cost_usd_max,
                score_min,
                score_max,
            )| {
                SegmentStats {
                    trace_count,
                    span_count,
                    start_time_min,
                    start_time_max,
                    end_time_min,
                    end_time_max,
                    latency_ms_min,
                    latency_ms_max,
                    cost_usd_min,
                    cost_usd_max,
                    score_min,
                    score_max,
                }
            },
        )
}

fn file() -> impl Strategy<Value = SegmentFile> {
    (
        proptest::sample::select(
            [
                SegmentFileKind::TracePack,
                SegmentFileKind::Bloom,
                SegmentFileKind::Stats,
            ]
            .as_slice(),
        ),
        small_string(),
        0u64..=4096,
        any::<u32>(),
    )
        .prop_map(|(kind, path, size_bytes, crc32)| SegmentFile {
            kind,
            path,
            size_bytes,
            crc32,
        })
}

/// Segment ids: either properly formatted (`segment_id(n)`) or junk that can
/// never parse as a numeric suffix.
fn entry_id() -> impl Strategy<Value = String> {
    prop_oneof![
        (0u32..=1_000).prop_map(segment_id),
        prop::collection::vec(proptest::sample::select(['x', 'y', 'z'].as_slice()), 1..=6)
            .prop_map(|chars| chars.into_iter().collect()),
    ]
}

fn field_range() -> impl Strategy<Value = FieldRange> {
    (
        prop::option::of(any::<i64>()),
        prop::option::of(0.0f64..1_000.0),
        prop::option::of(small_string()),
        prop::option::of(prop::collection::vec(small_string(), 0..=4)),
    )
        .prop_map(|(i64_min, f64_max, string_min, distinct)| FieldRange {
            i64_min,
            f64_max,
            string_min,
            distinct,
            ..FieldRange::default()
        })
}

fn record_stats() -> impl Strategy<Value = RecordStats> {
    (
        any::<u64>(),
        0u64..=100_000,
        prop::collection::vec((small_string(), field_range()), 0..=3),
    )
        .prop_map(|(schema_fingerprint, record_count, fields)| RecordStats {
            schema_fingerprint,
            record_count,
            fields: fields.into_iter().collect(),
        })
}

fn entry() -> impl Strategy<Value = SegmentEntry> {
    (
        entry_id(),
        0u64..=1_000,
        0u64..=100_000,
        prop::collection::vec(file(), 0..=3),
        stats(),
        prop::option::of(record_stats()),
    )
        .prop_map(
            |(segment_id, trace_count, span_count, files, stats, record_stats)| SegmentEntry {
                segment_id,
                trace_count,
                span_count,
                files,
                stats,
                record_stats,
            },
        )
}

fn manifest() -> impl Strategy<Value = SegmentManifest> {
    (
        1u32..=3,
        small_string(),
        small_string(),
        prop::option::of(small_string()),
        prop::collection::vec(entry(), 0..=4),
    )
        .prop_map(
            |(version, tenant_id, namespace, table, segments)| SegmentManifest {
                version,
                tenant_id,
                namespace,
                table,
                segments,
            },
        )
}

proptest! {
    /// M-RT: manifests round-trip through JSON with exact equality, None
    /// stats fields omitted and defaulted back.
    #[test]
    fn manifests_round_trip_through_json_with_equality(m in manifest()) {
        let encoded = serde_json::to_string(&m).expect("manifests serialize");
        let decoded: SegmentManifest =
            serde_json::from_str(&encoded).expect("manifest JSON parses back");
        prop_assert_eq!(decoded, m);
    }

    /// M-NEXT: next_segment_number is the highest numeric suffix plus one,
    /// and 1 for an empty manifest; junk ids never affect it.
    #[test]
    fn next_segment_number_is_max_suffix_plus_one(
        numbers in prop::collection::vec(0u32..=15, 0..=6),
        junk in prop::collection::vec(entry_id(), 0..=3),
    ) {
        let mut distinct: Vec<u32> = numbers;
        distinct.sort_unstable();
        distinct.dedup();

        let expected = distinct.last().map_or(1, |max| max + 1);

        let mut m = SegmentManifest::new("tenant-1", "default");
        prop_assert_eq!(m.next_segment_number(), 1, "empty manifest starts at 1");

        for number in &distinct {
            m.segments.push(SegmentEntry {
                segment_id: segment_id(*number),
                trace_count: 1,
                span_count: 1,
                files: Vec::new(),
                stats: SegmentStats::default(),
                record_stats: None,
            });
        }
        prop_assert_eq!(m.next_segment_number(), expected);

        // Junk ids (unparseable suffixes) leave the numbering unchanged.
        let junk_ids: Vec<String> = junk
            .into_iter()
            .filter(|id| !id.contains('-'))
            .collect();
        for id in &junk_ids {
            m.segments.push(SegmentEntry {
                segment_id: id.clone(),
                trace_count: 1,
                span_count: 1,
                files: Vec::new(),
                stats: SegmentStats::default(),
                record_stats: None,
            });
        }
        prop_assert_eq!(m.next_segment_number(), expected);
    }
}

/// M-ID: segment_id formatting holds for every number in the six-digit
/// domain, exhaustively.
#[test]
fn segment_id_formats_every_number_up_to_six_digits() {
    for number in 0..=999_999u32 {
        assert_eq!(
            segment_id(number),
            format!("segment-{number:06}"),
            "formatting broke at {number}"
        );
    }
}
