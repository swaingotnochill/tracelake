use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub tenant_id: String,
    pub namespace: String,
    pub trace_id: String,
    pub spans: Vec<Span>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub tenant_id: String,
    pub namespace: String,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,

    pub span_type: SpanType,
    pub name: String,

    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub status: SpanStatus,

    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub attributes: serde_json::Value,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,

    pub tokens: Option<TokenUsage>,
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub error: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scores: Vec<EvalScore>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanType {
    Llm,
    Tool,
    Retriever,
    Agent,
    Chain,
    Eval,
    System,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanStatus {
    Ok,
    Error,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt: u32,
    pub completion: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalScore {
    pub name: String,
    pub value: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn get_example_span() -> Span {
        Span {
            tenant_id: "tenantid-1".to_string(),
            namespace: "default".to_string(),
            trace_id: "trace-1".to_string(),
            span_id: "span-1".to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: "gpt-4o".to_string(),
            start_time: Utc.with_ymd_and_hms(2026, 6, 4, 10, 30, 0).unwrap(),
            end_time: Some(Utc.with_ymd_and_hms(2026, 6, 4, 10, 30, 1).unwrap()),
            status: SpanStatus::Ok,
            attributes: serde_json::Value::Null,
            input: Some(json!({ "prompt": "hello" })),
            output: Some(json!({ "text": "world" })),
            tokens: Some(TokenUsage {
                prompt: 10,
                completion: 20,
                total: 30,
            }),
            cost_usd: Some(0.001),
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            error: None,
            tags: Vec::new(),
            scores: Vec::new(),
        }
    }

    #[test]
    fn trace_round_trips_through_json() {
        let trace = Trace {
            tenant_id: "tenant-1".to_string(),
            namespace: "default".to_string(),
            trace_id: "trace-1".to_string(),
            spans: vec![get_example_span()],
        };

        let encoded = serde_json::to_string(&trace).expect("trace should serialize");
        let decoded: Trace = serde_json::from_str(&encoded).expect("trace should deserialize");

        assert_eq!(decoded, trace);
    }

    #[test]
    fn empty_tags_scores_and_null_attributes_are_omitted() {
        let encoded = serde_json::to_value(get_example_span()).expect("span should serialize");

        assert!(encoded.get("tags").is_none());
        assert!(encoded.get("scores").is_none());
        assert!(encoded.get("attributes").is_none());
    }

    #[test]
    fn missing_sparse_fields_deserialize_with_defaults() {
        let decoded: Span = serde_json::from_value(json!({
            "tenant_id": "tenant-1",
            "namespace": "default",
            "trace_id": "trace-1",
            "span_id": "span-1",
            "parent_span_id": null,
            "span_type": "llm",
            "name": "gpt-4o",
            "start_time": "2026-06-04T10:30:00Z",
            "end_time": null,
            "status": "ok",
            "input": null,
            "output": null,
            "tokens": null,
            "cost_usd": null,
            "model": null,
            "provider": null,
            "error": null
        }))
        .expect("span should deserialize with defaults");

        assert_eq!(decoded.attributes, serde_json::Value::Null);
        assert!(decoded.tags.is_empty());
        assert!(decoded.scores.is_empty());
    }

    #[test]
    fn custom_span_type_round_trips_through_json() {
        let mut span = get_example_span();
        span.span_type = SpanType::Custom("workflow_step".to_string());

        let encoded = serde_json::to_string(&span).expect("span should serialize");
        let decoded: Span = serde_json::from_str(&encoded).expect("span should deserialize");

        assert_eq!(
            decoded.span_type,
            SpanType::Custom("workflow_step".to_string())
        );
    }

    #[test]
    fn utc_datetime_round_trips_through_json() {
        let span = get_example_span();

        let encoded = serde_json::to_value(&span).expect("span should serialize");
        assert_eq!(encoded["start_time"], json!("2026-06-04T10:30:00Z"));

        let decoded: Span = serde_json::from_value(encoded).expect("span should deserialize");
        assert_eq!(decoded.start_time, span.start_time);
        assert_eq!(decoded.end_time, span.end_time);
    }
}
