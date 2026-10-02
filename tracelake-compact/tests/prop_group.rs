//! Property-based tests for the pure span grouping in
//! `tracelake_compact::group_spans`.

use chrono::DateTime;
use proptest::prelude::*;
use tracelake_compact::group_spans;
use tracelake_core::{Span, SpanStatus, SpanType};

/// Fixed epoch (mid-2026); deltas keep nanosecond precision.
const BASE_NANOS: i64 = 1_780_000_000_000_000_000;

const TENANTS: [&str; 2] = ["tenant-0", "tenant-1"];
const NAMESPACES: [&str; 2] = ["ns-0", "ns-1"];
const TRACES: [&str; 2] = ["trace-0", "trace-1"];
const SPAN_IDS: [&str; 3] = ["span-a", "span-b", "span-c"];

/// Everything a generated span varies over, drawn from the small pools above.
fn span_spec() -> impl Strategy<Value = (usize, usize, usize, i64, String, u8)> {
    (
        0usize..2,
        0usize..2,
        0usize..2,
        0i64..=1_000,
        proptest::sample::select(SPAN_IDS.as_slice()).prop_map(str::to_string),
        any::<u8>(),
    )
}

fn build_span(
    tenant: usize,
    namespace: usize,
    trace: usize,
    nanos: i64,
    span_id: &str,
    tag: u8,
) -> Span {
    Span {
        tenant_id: TENANTS[tenant].to_string(),
        namespace: NAMESPACES[namespace].to_string(),
        trace_id: TRACES[trace].to_string(),
        span_id: span_id.to_string(),
        parent_span_id: None,
        span_type: SpanType::Llm,
        name: format!("v{tag}"),
        start_time: DateTime::from_timestamp_nanos(BASE_NANOS + nanos),
        end_time: None,
        status: match tag % 3 {
            0 => SpanStatus::Ok,
            1 => SpanStatus::Error,
            _ => SpanStatus::Timeout,
        },
        attributes: serde_json::Value::Null,
        input: None,
        output: None,
        tokens: None,
        cost_usd: None,
        model: tag.is_multiple_of(2).then(|| "gpt-4o".to_string()),
        provider: None,
        error: None,
        tags: Vec::new(),
        scores: Vec::new(),
    }
}

/// Sorts spans into storage key order: tenant, namespace, trace_id,
/// start_time, then span_id — the order `scan_all_spans` returns.
fn sort_key_order(spans: &mut [Span]) {
    spans.sort_by(|a, b| {
        (
            &a.tenant_id,
            &a.namespace,
            &a.trace_id,
            a.start_time,
            &a.span_id,
        )
            .cmp(&(
                &b.tenant_id,
                &b.namespace,
                &b.trace_id,
                b.start_time,
                &b.span_id,
            ))
    });
}

proptest! {
    /// Arbitrary interleavings of 2 tenants x 2 namespaces x 2 traces,
    /// sorted into key order (the scan contract), group exactly: groups and
    /// traces follow first-appearance order, every span lands in exactly one
    /// trace at its input position, and counts match the distinct counts.
    #[test]
    fn groups_partition_key_ordered_spans_exactly(
        specs in prop::collection::vec(span_spec(), 0..=24),
    ) {
        let mut spans: Vec<Span> = specs
            .into_iter()
            .map(|(tenant, namespace, trace, nanos, span_id, tag)| {
                build_span(tenant, namespace, trace, nanos, &span_id, tag)
            })
            .collect();
        sort_key_order(&mut spans);

        let groups = group_spans(spans.clone());

        // Every span appears exactly once, in input order: grouping is a
        // partition into consecutive runs.
        let flattened: Vec<&Span> = groups
            .iter()
            .flat_map(|group| group.traces.iter().flat_map(|trace| trace.spans.iter()))
            .collect();
        prop_assert_eq!(flattened.len(), spans.len());
        for (index, span) in flattened.iter().enumerate() {
            prop_assert_eq!(*span, &spans[index]);
        }

        // One group per distinct (tenant, namespace), in first-appearance
        // (key) order, and traces likewise.
        let mut seen_pairs: Vec<(String, String)> = Vec::new();
        for span in &spans {
            let pair = (span.tenant_id.clone(), span.namespace.clone());
            if !seen_pairs.contains(&pair) {
                seen_pairs.push(pair);
            }
        }
        prop_assert_eq!(
            groups
                .iter()
                .map(|group| (group.tenant_id.clone(), group.namespace.clone()))
                .collect::<Vec<_>>(),
            seen_pairs
        );

        for group in &groups {
            let mut seen_traces: Vec<String> = Vec::new();
            for span in spans.iter().filter(|span| {
                span.tenant_id == group.tenant_id && span.namespace == group.namespace
            }) {
                if !seen_traces.contains(&span.trace_id) {
                    seen_traces.push(span.trace_id.clone());
                }
            }
            prop_assert_eq!(
                group
                    .traces
                    .iter()
                    .map(|trace| trace.trace_id.clone())
                    .collect::<Vec<_>>(),
                seen_traces
            );
            for trace in &group.traces {
                prop_assert!(trace.tenant_id == group.tenant_id);
                prop_assert!(trace.namespace == group.namespace);
                prop_assert!(!trace.spans.is_empty());
            }
        }
    }
}
