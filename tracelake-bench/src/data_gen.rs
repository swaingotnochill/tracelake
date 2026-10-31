//! Synthetic trace data with production-shaped payloads. Shared by every
//! benchmark phase so ingest, fetch, and search measure the same corpus.

use chrono::{Duration, TimeZone, Utc};
use serde_json::json;
use tracelake_core::{EvalScore, Span, SpanStatus, SpanType, TokenUsage};

/// One corpus configuration. Sizes are per trace, so total payload size is
/// `trace_count * spans_per_trace * payload_bytes`.
#[derive(Debug, Clone, Copy)]
pub struct CorpusConfig {
    pub trace_count: usize,
    pub spans_per_trace: usize,
    pub payload_bytes: usize,
    pub failed_every: usize,
}

impl Default for CorpusConfig {
    fn default() -> Self {
        Self {
            trace_count: 2_000,
            spans_per_trace: 10,
            payload_bytes: 2_048,
            failed_every: 9,
        }
    }
}

impl CorpusConfig {
    pub fn span_count(&self) -> usize {
        self.trace_count * self.spans_per_trace
    }
}

pub fn traces(config: CorpusConfig) -> Vec<Vec<Span>> {
    (0..config.trace_count)
        .map(|trace| trace_spans(config, trace))
        .collect()
}

pub fn trace_spans(config: CorpusConfig, trace: usize) -> Vec<Span> {
    let base = Utc
        .with_ymd_and_hms(2026, 10, 1, 12, 0, 0)
        .single()
        .expect("base timestamp is valid");
    let trace_id = format!("trace-{trace:07}");
    let failed = trace.is_multiple_of(config.failed_every);

    (0..config.spans_per_trace)
        .map(|index| {
            let start = base + Duration::milliseconds((trace * 10 + index) as i64);
            let mut span = Span {
                tenant_id: "tenant-bench".to_string(),
                namespace: "default".to_string(),
                trace_id: trace_id.clone(),
                span_id: format!("span-{index:04}"),
                parent_span_id: (index > 0).then(|| "span-0000".to_string()),
                span_type: if index == 0 {
                    SpanType::Agent
                } else {
                    SpanType::Llm
                },
                name: format!("step-{index}"),
                start_time: start,
                end_time: Some(start + Duration::milliseconds(120 + index as i64)),
                status: if failed && index == 1 {
                    SpanStatus::Error
                } else {
                    SpanStatus::Ok
                },
                attributes: json!({ "tenant": "bench", "topic": "support" }),
                input: Some(json!({ "prompt": payload("user asks about refund policy", config.payload_bytes) })),
                output: Some(json!({ "text": payload("refunds post in five days", config.payload_bytes) })),
                tokens: Some(TokenUsage {
                    prompt: 512,
                    completion: 128,
                    total: 640,
                }),
                cost_usd: Some(0.0025),
                model: Some(if trace.is_multiple_of(2) {
                    "gpt-4o".to_string()
                } else {
                    "claude-sonnet".to_string()
                }),
                provider: Some("openai".to_string()),
                error: (failed && index == 1).then(|| "tool timeout after 30s".to_string()),
                tags: vec!["bench".to_string(), "support".to_string()],
                scores: vec![EvalScore {
                    name: "quality".to_string(),
                    value: 0.85,
                }],
            };
            span.start_time = start;
            span
        })
        .collect()
}

/// Repeats `seed` until it reaches roughly `bytes`, keeping the value
/// realistic enough that a text index has real terms to tokenize.
fn payload(seed: &str, bytes: usize) -> String {
    let mut text = String::with_capacity(bytes + seed.len());
    while text.len() < bytes {
        text.push_str(seed);
        text.push_str(". ");
    }
    text.truncate(bytes);
    text
}
