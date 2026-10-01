use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Inventory of every compacted segment for one (tenant, namespace).
///
/// The manifest is the atomic publish point for compaction: segment files are
/// written first, then a single manifest PUT makes them visible to readers.
/// Step 4 assumes a single compactor per namespace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentManifest {
    /// Manifest format version.
    pub version: u32,
    pub tenant_id: String,
    pub namespace: String,
    /// Every published segment, oldest first.
    pub segments: Vec<SegmentEntry>,
}

/// One compacted segment: one trace pack plus its sidecar files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentEntry {
    /// Segment id, e.g. `segment-000001`; also the file name stem.
    pub segment_id: String,
    pub trace_count: u64,
    pub span_count: u64,
    /// Files belonging to this segment, with sizes and checksums.
    pub files: Vec<SegmentFile>,
    /// Embedded stats so the planner can prune without extra reads.
    pub stats: SegmentStats,
}

/// One file inside a segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentFile {
    pub kind: SegmentFileKind,
    /// Full object path from the store root.
    pub path: String,
    pub size_bytes: u64,
    pub crc32: u32,
}

/// Kinds of files a segment may contain. Parquet columns and the text index
/// join in Steps 7 and 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentFileKind {
    TracePack,
    Bloom,
    Stats,
}

impl SegmentFileKind {
    /// File name extension for this kind.
    pub fn extension(self) -> &'static str {
        match self {
            SegmentFileKind::TracePack => "tracepack",
            SegmentFileKind::Bloom => "bloom",
            SegmentFileKind::Stats => "stats",
        }
    }
}

/// Min/max ranges over a segment's spans, used to skip whole segments when a
/// query's filters cannot overlap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SegmentStats {
    pub trace_count: u64,
    pub span_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time_min: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time_max: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time_min: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time_max: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms_max: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_max: Option<f64>,
}

impl SegmentManifest {
    /// Creates an empty manifest for one (tenant, namespace).
    pub fn new(tenant_id: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self {
            version: 1,
            tenant_id: tenant_id.into(),
            namespace: namespace.into(),
            segments: Vec::new(),
        }
    }

    /// Next free segment number: the highest existing suffix plus one.
    pub fn next_segment_number(&self) -> u32 {
        self.segments
            .iter()
            .filter_map(|entry| entry.segment_id.rsplit('-').next())
            .filter_map(|suffix| suffix.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1
    }
}

/// Formats a segment number as its id, e.g. `1` -> `segment-000001`.
pub fn segment_id(number: u32) -> String {
    format!("segment-{number:06}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn example_entry(segment_id: &str) -> SegmentEntry {
        SegmentEntry {
            segment_id: segment_id.to_string(),
            trace_count: 2,
            span_count: 20,
            files: vec![SegmentFile {
                kind: SegmentFileKind::TracePack,
                path: format!("base/tenant/default/{segment_id}.tracepack"),
                size_bytes: 1024,
                crc32: 0xdead_beef,
            }],
            stats: SegmentStats {
                trace_count: 2,
                span_count: 20,
                start_time_min: Some(
                    DateTime::parse_from_rfc3339("2026-06-16T10:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                start_time_max: Some(
                    DateTime::parse_from_rfc3339("2026-06-16T11:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                end_time_min: None,
                end_time_max: None,
                latency_ms_min: Some(10),
                latency_ms_max: Some(5_000),
                cost_usd_min: Some(0.001),
                cost_usd_max: Some(0.5),
                score_min: None,
                score_max: None,
            },
        }
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let mut manifest = SegmentManifest::new("tenant-1", "default");
        manifest.segments.push(example_entry("segment-000001"));
        manifest.segments.push(example_entry("segment-000002"));

        let encoded = serde_json::to_string(&manifest).expect("manifest should encode");
        let decoded: SegmentManifest =
            serde_json::from_str(&encoded).expect("manifest should decode");

        assert_eq!(decoded, manifest);
    }

    #[test]
    fn empty_stats_fields_are_omitted_and_default_back() {
        let stats = SegmentStats {
            trace_count: 1,
            span_count: 1,
            start_time_min: None,
            start_time_max: None,
            end_time_min: None,
            end_time_max: None,
            latency_ms_min: None,
            latency_ms_max: None,
            cost_usd_min: None,
            cost_usd_max: None,
            score_min: None,
            score_max: None,
        };

        let value = serde_json::to_value(stats).expect("stats should encode");
        assert!(value.get("latency_ms_min").is_none());

        let decoded: SegmentStats = serde_json::from_value(value).expect("stats should decode");
        assert_eq!(decoded, stats);
    }

    #[test]
    fn next_segment_number_follows_the_highest_suffix() {
        let mut manifest = SegmentManifest::new("tenant-1", "default");
        assert_eq!(manifest.next_segment_number(), 1);

        manifest.segments.push(example_entry("segment-000007"));
        manifest.segments.push(example_entry("segment-000002"));

        assert_eq!(manifest.next_segment_number(), 8);
        assert_eq!(segment_id(manifest.next_segment_number()), "segment-000008");
    }
}
