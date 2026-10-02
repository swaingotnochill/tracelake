//! Merges span layers from different sources into one ordered list.

use std::collections::HashMap;
use tracelake_core::Span;

/// Merges span layers in ascending precedence order.
///
/// Layers are given oldest first and the hot store last: when the same
/// span_id appears in more than one layer, the last layer wins, so hot spans
/// override compacted spans and newer segments override older ones.
///
/// The result is sorted by (start_time, span_id) — the same order as the
/// storage key.
pub fn merge_spans(layers: &[Vec<Span>]) -> Vec<Span> {
    let mut by_span_id: HashMap<&str, &Span> = HashMap::new();

    for layer in layers {
        for span in layer {
            by_span_id.insert(span.span_id.as_str(), span);
        }
    }

    let mut spans: Vec<Span> = by_span_id.into_values().cloned().collect();
    spans.sort_by(|a, b| {
        a.start_time
            .cmp(&b.start_time)
            .then_with(|| a.span_id.cmp(&b.span_id))
    });

    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use tracelake_core::{Span, SpanStatus, SpanType};

    fn make_span(span_id: &str, nanos: i64, status: SpanStatus) -> Span {
        Span {
            tenant_id: "tenantid-1".to_string(),
            namespace: "default".to_string(),
            trace_id: "trace-1".to_string(),
            span_id: span_id.to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: span_id.to_string(),
            start_time: Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, 0).unwrap()
                + chrono::Duration::nanoseconds(nanos),
            end_time: None,
            status,
            attributes: serde_json::Value::Null,
            input: Some(json!({ "prompt": "hello" })),
            output: None,
            tokens: None,
            cost_usd: None,
            model: None,
            provider: None,
            error: None,
            tags: Vec::new(),
            scores: Vec::new(),
        }
    }

    #[test]
    fn empty_layers_give_empty_output() {
        assert!(merge_spans(&[]).is_empty());
        assert!(merge_spans(&[Vec::new(), Vec::new()]).is_empty());
    }

    #[test]
    fn merges_layers_sorted_by_start_time_then_span_id() {
        let cold = vec![
            make_span("span-b", 2, SpanStatus::Ok),
            make_span("span-a", 2, SpanStatus::Ok),
            make_span("span-z", 1, SpanStatus::Ok),
        ];
        let hot = vec![make_span("span-c", 3, SpanStatus::Ok)];

        let merged = merge_spans(&[cold, hot]);

        let ids: Vec<&str> = merged.iter().map(|span| span.span_id.as_str()).collect();
        assert_eq!(ids, vec!["span-z", "span-a", "span-b", "span-c"]);
    }

    #[test]
    fn later_layers_override_earlier_on_span_id_conflict() {
        let old_segment = vec![make_span("span-a", 1, SpanStatus::Ok)];
        let new_segment = vec![
            make_span("span-a", 1, SpanStatus::Timeout),
            make_span("span-b", 2, SpanStatus::Ok),
        ];
        let hot = vec![make_span("span-a", 1, SpanStatus::Error)];

        let merged = merge_spans(&[old_segment, new_segment, hot]);

        assert_eq!(merged.len(), 2);
        let span_a = merged
            .iter()
            .find(|span| span.span_id == "span-a")
            .expect("span-a should survive");
        assert_eq!(span_a.status, SpanStatus::Error);
        assert!(merged.iter().any(|span| span.span_id == "span-b"));
    }
}
