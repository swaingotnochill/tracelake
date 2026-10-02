//! Property-based tests for the merge laws of `tracelake_query::merge`.

use chrono::DateTime;
use proptest::prelude::*;
use tracelake_core::{Span, SpanStatus, SpanType};
use tracelake_query::merge_spans;

/// Fixed epoch (mid-2026); deltas keep nanosecond precision.
const BASE_NANOS: i64 = 1_780_000_000_000_000_000;

/// Small fixed span-id pool so layers overlap often.
const SPAN_IDS: &[&str] = &["a", "b", "c", "d"];

/// Everything a generated span varies over: id, time offset, and a
/// distinguishability tag.
fn span_spec() -> impl Strategy<Value = (String, i64, u8)> {
    (
        proptest::sample::select(SPAN_IDS).prop_map(str::to_string),
        0i64..=1_000,
        any::<u8>(),
    )
}

/// Builds a span; `name` carries the tag so two spans with the same id but
/// different origin never compare equal by accident.
fn build_span(span_id: &str, nanos: i64, tag: u8) -> Span {
    Span {
        tenant_id: "tenantid-1".to_string(),
        namespace: "default".to_string(),
        trace_id: "trace-1".to_string(),
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
        error: (tag % 3 == 1).then(|| format!("boom-{tag}")),
        tags: Vec::new(),
        scores: Vec::new(),
    }
}

fn layer() -> impl Strategy<Value = Vec<Span>> {
    prop::collection::vec(span_spec(), 0..=4).prop_map(|specs| {
        specs
            .into_iter()
            .map(|(id, nanos, tag)| build_span(&id, nanos, tag))
            .collect()
    })
}

proptest! {
    /// L1: any number of empty layers merges to an empty output.
    #[test]
    fn empty_layers_merge_to_empty(layer_count in 0u8..=3) {
        let layers: Vec<Vec<Span>> = vec![Vec::new(); layer_count as usize];
        prop_assert!(merge_spans(&layers).is_empty());
    }

    /// L2 + L3 + L4: output is sorted by (start_time, span_id), every span
    /// equals the copy from the last layer holding its id, and the id set is
    /// exactly the union of the input id sets.
    #[test]
    fn merge_output_follows_all_layer_laws(specs in prop::collection::vec(layer(), 0..=3)) {
        let layers: Vec<Vec<Span>> = specs;
        let merged = merge_spans(&layers);

        // L2: sortedness by (start_time, span_id); L4 implies no duplicate ids.
        for pair in merged.windows(2) {
            prop_assert!(
                pair[0].start_time < pair[1].start_time
                    || (pair[0].start_time == pair[1].start_time
                        && pair[0].span_id < pair[1].span_id),
                "output must be sorted by (start_time, span_id), got {} then {}",
                pair[0].span_id,
                pair[1].span_id
            );
        }

        // L3: last-layer-wins (and within a layer, the last occurrence of a
        // duplicated id wins, matching the sequential insert order).
        for span in &merged {
            let candidates: Vec<&Span> = layers
                .iter()
                .flatten()
                .filter(|candidate| candidate.span_id == span.span_id)
                .collect();
            let winner = candidates
                .last()
                .expect("output ids always come from some layer");
            prop_assert!(span == *winner);
        }

        // L4: no loss, no spurious spans.
        let input_ids: std::collections::HashSet<&str> = layers
            .iter()
            .flatten()
            .map(|span| span.span_id.as_str())
            .collect();
        let output_ids: std::collections::HashSet<&str> =
            merged.iter().map(|span| span.span_id.as_str()).collect();
        prop_assert_eq!(merged.len(), output_ids.len());
        prop_assert_eq!(input_ids, output_ids);
    }

    /// L5: merging a layer with itself is idempotent when the layer's ids are
    /// unique.
    #[test]
    fn merging_a_layer_with_itself_is_idempotent(specs in prop::collection::vec(span_spec(), 0..=4)) {
        let mut layer: Vec<Span> = Vec::new();
        for (id, nanos, tag) in specs {
            if !layer.iter().any(|span| span.span_id == id) {
                layer.push(build_span(&id, nanos, tag));
            }
        }

        let once = merge_spans(std::slice::from_ref(&layer));
        let twice = merge_spans(&[layer.clone(), layer]);

        prop_assert_eq!(once, twice);
    }
}
