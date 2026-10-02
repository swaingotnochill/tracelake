//! Property-based tests for the trace pack format in
//! `tracelake_core::trace_pack`.

use chrono::DateTime;
use proptest::prelude::*;
use tracelake_core::{Span, TokenUsage, Trace, decode_trace_pack, encode_trace_pack};

/// Fixed epoch (mid-2026) so generated timestamps stay realistic; deltas keep
/// nanosecond precision without leaving RFC3339 range.
const BASE_NANOS: i64 = 1_780_000_000_000_000_000;

/// Small fixed alphabet for ids.
const ID_ALPHABET: &[char] = &['a', 'b', 'c', '0', '1', '-'];

/// Bounded id: 1..=14 chars, no '/'.
fn id() -> impl Strategy<Value = String> {
    prop::collection::vec(proptest::sample::select(ID_ALPHABET), 1..=14)
        .prop_map(|chars| chars.into_iter().collect())
}

/// Everything a generated span varies over: id, time, and a content flavor.
type SpanSpec = (String, i64, u8);

/// One trace's id base plus its span specs.
type TraceEntry = (String, Vec<SpanSpec>);

fn span_spec() -> impl Strategy<Value = SpanSpec> {
    (id(), 0i64..(1 << 32), any::<u8>())
}

/// Builds a span whose JSON serializes and deserializes exactly.
fn build_span(trace_id: &str, span_id: &str, nanos: i64, flavor: u8) -> Span {
    let start_time = DateTime::from_timestamp_nanos(BASE_NANOS + nanos);
    Span {
        tenant_id: "tenantid-1".to_string(),
        namespace: "default".to_string(),
        trace_id: trace_id.to_string(),
        span_id: span_id.to_string(),
        parent_span_id: flavor.is_multiple_of(2).then(|| "parent-1".to_string()),
        span_type: match flavor % 3 {
            0 => tracelake_core::SpanType::Llm,
            1 => tracelake_core::SpanType::Tool,
            _ => tracelake_core::SpanType::Custom("custom-1".to_string()),
        },
        name: format!("span-{span_id}-{flavor}"),
        start_time,
        end_time: (flavor % 3 == 1).then(|| DateTime::from_timestamp_nanos(BASE_NANOS + nanos + 1)),
        status: match flavor % 3 {
            0 => tracelake_core::SpanStatus::Ok,
            1 => tracelake_core::SpanStatus::Error,
            _ => tracelake_core::SpanStatus::Timeout,
        },
        attributes: if flavor.is_multiple_of(2) {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "flavor": flavor, "stable": true })
        },
        input: (flavor % 3 != 2).then(|| serde_json::json!({ "prompt": "hello", "n": flavor })),
        output: (flavor % 2 == 1).then(|| serde_json::json!({ "text": "world" })),
        tokens: flavor.is_multiple_of(3).then(|| TokenUsage {
            prompt: flavor as u32,
            completion: (flavor % 7) as u32,
            total: (flavor as u32) + (flavor % 7) as u32,
        }),
        cost_usd: flavor.is_multiple_of(2).then_some((flavor as f64) * 0.001),
        model: (flavor % 3 == 1).then(|| format!("model-{flavor}")),
        provider: flavor.is_multiple_of(4).then(|| "openai".to_string()),
        error: (flavor % 3 == 2).then(|| format!("boom-{flavor}")),
        tags: if flavor.is_multiple_of(2) {
            Vec::new()
        } else {
            vec![format!("tag-{flavor}"), "e2e".to_string()]
        },
        scores: if flavor % 3 == 1 {
            vec![tracelake_core::EvalScore {
                name: "helpful".to_string(),
                value: (flavor as f64) / 8.0,
            }]
        } else {
            Vec::new()
        },
    }
}

/// One trace's worth of span specs.
fn trace_entry() -> impl Strategy<Value = TraceEntry> {
    (id(), prop::collection::vec(span_spec(), 1..=5))
}

/// Builds admissible pack input: 1..=4 traces with unique trace ids (the
/// "-index" suffix guarantees uniqueness), each with 1..=5 spans.
fn build_traces(entries: Vec<TraceEntry>) -> Vec<Trace> {
    entries
        .into_iter()
        .enumerate()
        .map(|(index, (base, specs))| {
            let trace_id = format!("{base}-{}", index + 1);
            Trace {
                tenant_id: "tenantid-1".to_string(),
                namespace: "default".to_string(),
                trace_id: trace_id.clone(),
                spans: specs
                    .into_iter()
                    .map(|(span_id, nanos, flavor)| build_span(&trace_id, &span_id, nanos, flavor))
                    .collect(),
            }
        })
        .collect()
}

proptest! {
    /// P-RT: encode then decode recovers every trace's spans exactly, in
    /// encode order, with matching counts and sorted trace ids.
    #[test]
    fn packs_round_trip_every_trace_exactly(
        entries in prop::collection::vec(trace_entry(), 1..=4),
    ) {
        let traces = build_traces(entries);

        let bytes = encode_trace_pack(&traces).expect("admissible traces encode");
        let pack = decode_trace_pack(&bytes).expect("valid packs decode");

        let mut sorted = traces.clone();
        sorted.sort_by(|a, b| a.trace_id.cmp(&b.trace_id));

        prop_assert_eq!(pack.trace_count(), sorted.len());
        prop_assert_eq!(
            pack.span_count() as usize,
            sorted.iter().map(|trace| trace.spans.len()).sum::<usize>()
        );
        let expected_ids: Vec<String> =
            sorted.iter().map(|trace| trace.trace_id.clone()).collect();
        prop_assert_eq!(pack.trace_ids().expect("ids should read"), expected_ids);

        for trace in &sorted {
            prop_assert_eq!(
                pack.read_trace(&trace.trace_id).expect("lookup should succeed"),
                Some(trace.spans.clone())
            );
        }
        prop_assert_eq!(
            pack.read_trace("no-such-trace").expect("misses should succeed"),
            None
        );
    }

    /// P-TRUNC: every strict prefix of a valid pack is rejected.
    #[test]
    fn strict_prefixes_of_valid_packs_are_rejected(
        entries in prop::collection::vec(trace_entry(), 1..=4),
    ) {
        let traces = build_traces(entries);
        let bytes = encode_trace_pack(&traces).expect("admissible traces encode");

        for cut in 0..bytes.len() {
            prop_assert!(
                decode_trace_pack(&bytes[..cut]).is_err(),
                "prefix of {cut} bytes must be rejected"
            );
        }
        prop_assert!(decode_trace_pack(&bytes).is_ok());
    }

    /// P-NP: the decoder and every reader is total on arbitrary bytes — no
    /// panics from the directory walk, binary search, or block reader.
    #[test]
    fn decode_and_readers_never_panic_on_arbitrary_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..=512),
        probe in id(),
    ) {
        if let Ok(pack) = decode_trace_pack(&bytes) {
            let _ = pack.trace_ids();
            let _ = pack.read_trace(&probe);
        }
    }
}
