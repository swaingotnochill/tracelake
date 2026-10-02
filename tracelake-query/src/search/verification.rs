//! Property-based verification of the pure search helpers (`span_matches`,
//! `stats_may_overlap`). Lives as a child module of `search` because those
//! helpers are private and unreachable from integration tests.

use super::{TraceSearch, span_matches, stats_may_overlap};
use chrono::DateTime;
use proptest::prelude::*;
use tracelake_core::{SegmentStats, Span, SpanStatus, SpanType};

/// Fixed epoch (mid-2026); deltas keep nanosecond precision.
const BASE_NANOS: i64 = 1_780_000_000_000_000_000;

const STATUSES: [SpanStatus; 3] = [SpanStatus::Ok, SpanStatus::Error, SpanStatus::Timeout];
const MODELS: [&str; 2] = ["gpt-4o", "claude-3"];

/// Nanosecond offsets within a ±1000s window.
fn nanos() -> impl Strategy<Value = i64> {
    -1_000_000_000_000i64..=1_000_000_000_000
}

fn status() -> impl Strategy<Value = SpanStatus> {
    proptest::sample::select(STATUSES.as_slice())
}

/// A span's model: unset or one of the fixed pool.
fn span_model() -> impl Strategy<Value = Option<String>> {
    proptest::option::of(proptest::sample::select(MODELS.as_slice()).prop_map(str::to_string))
}

fn make_span(start_nanos: i64, status: SpanStatus, model: Option<String>) -> Span {
    Span {
        tenant_id: "tenantid-1".to_string(),
        namespace: "default".to_string(),
        trace_id: "trace-1".to_string(),
        span_id: "span-1".to_string(),
        parent_span_id: None,
        span_type: SpanType::Llm,
        name: "span-1".to_string(),
        start_time: DateTime::from_timestamp_nanos(BASE_NANOS + start_nanos),
        end_time: None,
        status,
        attributes: serde_json::Value::Null,
        input: None,
        output: None,
        tokens: None,
        cost_usd: None,
        model,
        provider: None,
        error: None,
        tags: Vec::new(),
        scores: Vec::new(),
    }
}

proptest! {
    /// S-PRUNE-SOUND: when stats say a segment cannot overlap the query
    /// window, no start_time inside the segment's [min, max] range can
    /// satisfy the search's time filter. With all bounds set, pruning is
    /// exactly the disjunction `segment_max < query_start ||
    /// segment_min > query_end` (the contrapositive of soundness).
    #[test]
    fn pruning_a_segment_means_no_time_in_range_can_match(
        stats_min in nanos(),
        stats_max in nanos(),
        query_start in nanos(),
        query_end in nanos(),
    ) {
        let stats = SegmentStats {
            start_time_min: Some(DateTime::from_timestamp_nanos(BASE_NANOS + stats_min)),
            start_time_max: Some(DateTime::from_timestamp_nanos(BASE_NANOS + stats_max)),
            ..SegmentStats::default()
        };
        let search = TraceSearch {
            time_start: Some(DateTime::from_timestamp_nanos(BASE_NANOS + query_start)),
            time_end: Some(DateTime::from_timestamp_nanos(BASE_NANOS + query_end)),
            ..TraceSearch::default()
        };

        if !stats_may_overlap(&stats, &search) {
            prop_assert!(
                stats_max < query_start || stats_min > query_end,
                "pruned segment [{stats_min}, {stats_max}] against query \
                 [{query_start}, {query_end}]"
            );
        }
    }

    /// S-MATCH (unset): with every filter unset, any span matches.
    #[test]
    fn unset_filters_match_every_span(
        start in nanos(),
        status in status(),
        model in span_model(),
    ) {
        let span = make_span(start, status, model);
        prop_assert!(span_matches(&span, &TraceSearch::default()));
    }

    /// S-MATCH (status): the status filter matches exactly its own value.
    #[test]
    fn status_filter_matches_exactly_its_value(
        span_status in status(),
        query_status in status(),
    ) {
        let span = make_span(0, span_status.clone(), None);

        let matching = TraceSearch {
            status: Some(span_status.clone()),
            ..TraceSearch::default()
        };
        prop_assert!(span_matches(&span, &matching));

        let querying = TraceSearch {
            status: Some(query_status.clone()),
            ..TraceSearch::default()
        };
        prop_assert_eq!(span_matches(&span, &querying), span_status == query_status);
    }

    /// S-MATCH (model): the model filter matches exactly its own value, and
    /// spans without a model never match a model filter.
    #[test]
    fn model_filter_matches_exactly_its_value(
        span_model_value in span_model(),
        query_model in proptest::sample::select(MODELS.as_slice()),
    ) {
        let span = make_span(0, SpanStatus::Ok, span_model_value.clone());

        let querying = TraceSearch {
            model: Some(query_model.to_string()),
            ..TraceSearch::default()
        };
        prop_assert_eq!(
            span_matches(&span, &querying),
            span_model_value.as_deref() == Some(query_model)
        );
    }

    /// S-MATCH (time): each time bound is exact and inclusive on its own.
    #[test]
    fn time_bounds_are_exact_and_inclusive(start in nanos(), bound in nanos()) {
        let span = make_span(start, SpanStatus::Ok, None);

        let lower = TraceSearch {
            time_start: Some(DateTime::from_timestamp_nanos(BASE_NANOS + bound)),
            ..TraceSearch::default()
        };
        prop_assert_eq!(span_matches(&span, &lower), start >= bound);

        let upper = TraceSearch {
            time_end: Some(DateTime::from_timestamp_nanos(BASE_NANOS + bound)),
            ..TraceSearch::default()
        };
        prop_assert_eq!(span_matches(&span, &upper), start <= bound);
    }

    /// S-MATCH (complete): a span with every field inside the filters always
    /// matches, for arbitrary statuses, models, and windows.
    #[test]
    fn spans_inside_all_filters_always_match(
        window_start in nanos(),
        window_width in 0i64..=1_000_000_000,
        offset_in_window in 0i64..=1_000_000_000,
        status in status(),
        model in proptest::sample::select(MODELS.as_slice()),
    ) {
        let window_end = window_start + window_width;
        let start = window_start + (offset_in_window % (window_width + 1));

        let span = make_span(start, status.clone(), Some(model.to_string()));
        let search = TraceSearch {
            time_start: Some(DateTime::from_timestamp_nanos(BASE_NANOS + window_start)),
            time_end: Some(DateTime::from_timestamp_nanos(BASE_NANOS + window_end)),
            status: Some(status),
            model: Some(model.to_string()),
            ..TraceSearch::default()
        };

        prop_assert!(span_matches(&span, &search));
    }
}
