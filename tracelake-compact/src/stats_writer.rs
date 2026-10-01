use crate::compactor::TraceGroup;
use tracelake_core::SegmentStats;

/// Computes a segment's stats (min/max ranges, counts) from its traces.
pub fn build_segment_stats(group: &TraceGroup) -> SegmentStats {
    let spans = group.traces.iter().flat_map(|trace| trace.spans.iter());

    let mut stats = SegmentStats {
        trace_count: group.traces.len() as u64,
        span_count: 0,
        ..SegmentStats::default()
    };

    for span in spans {
        stats.span_count += 1;

        track_min_max(
            &mut stats.start_time_min,
            &mut stats.start_time_max,
            span.start_time,
        );
        track_optional_min_max(
            &mut stats.end_time_min,
            &mut stats.end_time_max,
            span.end_time,
        );

        if let Some(end_time) = span.end_time {
            let latency_ms = (end_time - span.start_time).num_milliseconds().max(0) as u64;
            track_min_max(
                &mut stats.latency_ms_min,
                &mut stats.latency_ms_max,
                latency_ms,
            );
        }

        if let Some(cost_usd) = span.cost_usd {
            track_min_max(&mut stats.cost_usd_min, &mut stats.cost_usd_max, cost_usd);
        }

        for score in &span.scores {
            track_min_max(&mut stats.score_min, &mut stats.score_max, score.value);
        }
    }

    stats
}

/// Updates a numeric min/max pair.
fn track_min_max<T: PartialOrd + Copy>(min: &mut Option<T>, max: &mut Option<T>, value: T) {
    if min.as_ref().is_none_or(|current| value < *current) {
        *min = Some(value);
    }
    if max.as_ref().is_none_or(|current| value > *current) {
        *max = Some(value);
    }
}

/// Updates an optional min/max pair, skipping missing values.
fn track_optional_min_max<T: PartialOrd + Copy>(
    min: &mut Option<T>,
    max: &mut Option<T>,
    value: Option<T>,
) {
    if let Some(value) = value {
        track_min_max(min, max, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use tracelake_core::{EvalScore, Span, SpanStatus, SpanType};

    fn make_span(
        trace_id: &str,
        span_id: &str,
        seconds: u32,
        duration_ms: Option<i64>,
        cost_usd: Option<f64>,
        score: Option<f64>,
    ) -> Span {
        let start_time = Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, seconds).unwrap();
        Span {
            tenant_id: "tenant-1".to_string(),
            namespace: "default".to_string(),
            trace_id: trace_id.to_string(),
            span_id: span_id.to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: span_id.to_string(),
            start_time,
            end_time: duration_ms.map(|ms| start_time + chrono::Duration::milliseconds(ms)),
            status: SpanStatus::Ok,
            attributes: serde_json::Value::Null,
            input: None,
            output: None,
            tokens: None,
            cost_usd,
            model: None,
            provider: None,
            error: None,
            tags: Vec::new(),
            scores: match score {
                Some(value) => vec![EvalScore {
                    name: "quality".to_string(),
                    value,
                }],
                None => Vec::new(),
            },
        }
    }

    fn group_of(spans: Vec<Span>) -> TraceGroup {
        TraceGroup {
            tenant_id: "tenant-1".to_string(),
            namespace: "default".to_string(),
            traces: vec![tracelake_core::Trace {
                tenant_id: "tenant-1".to_string(),
                namespace: "default".to_string(),
                trace_id: "trace-1".to_string(),
                spans,
            }],
        }
    }

    #[test]
    fn stats_track_exact_min_max_and_counts() {
        let spans = vec![
            make_span("trace-1", "a", 10, Some(100), Some(0.5), Some(0.8)),
            make_span("trace-1", "b", 5, Some(5_000), Some(0.001), Some(0.2)),
            make_span("trace-1", "c", 20, None, None, None),
        ];

        let stats = build_segment_stats(&group_of(spans));

        assert_eq!(stats.trace_count, 1);
        assert_eq!(stats.span_count, 3);
        assert_eq!(
            stats.start_time_min,
            Some(Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, 5).unwrap())
        );
        assert_eq!(
            stats.start_time_max,
            Some(Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, 20).unwrap())
        );
        assert_eq!(
            stats.end_time_max,
            Some(
                Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, 10).unwrap()
                    + chrono::Duration::milliseconds(100)
            )
        );
        assert_eq!(stats.latency_ms_min, Some(100));
        assert_eq!(stats.latency_ms_max, Some(5_000));
        assert_eq!(stats.cost_usd_min, Some(0.001));
        assert_eq!(stats.cost_usd_max, Some(0.5));
        assert_eq!(stats.score_min, Some(0.2));
        assert_eq!(stats.score_max, Some(0.8));
    }

    #[test]
    fn empty_group_leaves_ranges_unset() {
        let group = TraceGroup {
            tenant_id: "tenant-1".to_string(),
            namespace: "default".to_string(),
            traces: Vec::new(),
        };

        let stats = build_segment_stats(&group);

        assert_eq!(stats, SegmentStats::default());
        assert!(stats.start_time_min.is_none());
        assert_eq!(stats.span_count, 0);
    }
}
