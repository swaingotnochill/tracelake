use crate::span::Span;
use thiserror::Error;

const SPAN_KEY_VERSION: &str = "v1";
const TIMESTAMP_WIDTH: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Decoded components of a SlateDB span storage key.
pub struct SpanKey {
    pub tenant_id: String,
    pub namespace: String,
    pub trace_id: String,
    pub start_time_ns: i64,
    pub span_id: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
/// Errors returned when span storage keys cannot be encoded or decoded.
pub enum KeyError {
    #[error("{field} cannot be empty")]
    EmptySegment { field: &'static str },

    #[error("{field} cannot contain '/'")]
    SegmentContainsDelimiter { field: &'static str },

    #[error("start_time_ns cannot be negative: {start_time_ns}")]
    NegativeTimestamp { start_time_ns: i64 },

    #[error("key is not valid UTF-8")]
    InvalidUtf8,

    #[error("key does not match span key format")]
    InvalidFormat,

    #[error("timestamp segment is invalid: {value}")]
    InvalidTimestamp { value: String },

    #[error("timestamp is outside supported nanosecond range")]
    TimestampOutOfRange,
}

/// Encodes span identity fields into the canonical lexicographic storage key.
pub fn encode_span_key(
    tenant_id: &str,
    namespace: &str,
    trace_id: &str,
    start_time_ns: i64,
    span_id: &str,
) -> Result<Vec<u8>, KeyError> {
    validate_segment("tenant_id", tenant_id)?;
    validate_segment("namespace", namespace)?;
    validate_segment("trace_id", trace_id)?;
    validate_segment("span_id", span_id)?;
    validate_timestamp(start_time_ns)?;

    Ok(format!(
        "{SPAN_KEY_VERSION}/t/{tenant_id}/ns/{namespace}/tr/{trace_id}/sp/{start_time_ns:0TIMESTAMP_WIDTH$}/{span_id}"
    )
    .into_bytes())
}

/// Decodes a span storage key into its tenant, namespace, trace, timestamp, and span id.
pub fn decode_span_key(key: &[u8]) -> Result<SpanKey, KeyError> {
    let key = std::str::from_utf8(key).map_err(|_| KeyError::InvalidUtf8)?;
    let parts = split_span_key(key)?;
    let start_time_ns = decode_timestamp(parts[8])?;

    Ok(SpanKey {
        tenant_id: parts[2].to_string(),
        namespace: parts[4].to_string(),
        trace_id: parts[6].to_string(),
        start_time_ns,
        span_id: parts[9].to_string(),
    })
}

/// Returns the byte prefix that scopes all keys for one tenant.
pub fn tenant_prefix(tenant_id: &str) -> Result<Vec<u8>, KeyError> {
    validate_segment("tenant_id", tenant_id)?;
    Ok(format!("{SPAN_KEY_VERSION}/t/{tenant_id}/").into_bytes())
}

/// Returns the byte prefix that scopes all keys for one namespace.
pub fn namespace_prefix(tenant_id: &str, namespace: &str) -> Result<Vec<u8>, KeyError> {
    validate_segment("tenant_id", tenant_id)?;
    validate_segment("namespace", namespace)?;
    Ok(format!("{SPAN_KEY_VERSION}/t/{tenant_id}/ns/{namespace}/").into_bytes())
}

/// Returns the byte prefix that scopes all span keys for one trace.
pub fn trace_prefix(tenant_id: &str, namespace: &str, trace_id: &str) -> Result<Vec<u8>, KeyError> {
    validate_segment("tenant_id", tenant_id)?;
    validate_segment("namespace", namespace)?;
    validate_segment("trace_id", trace_id)?;
    Ok(format!("{SPAN_KEY_VERSION}/t/{tenant_id}/ns/{namespace}/tr/{trace_id}/sp/").into_bytes())
}

/// Builds the canonical storage key for a span.
pub fn encode_span_storage_key(span: &Span) -> Result<Vec<u8>, KeyError> {
    encode_span_key(
        &span.tenant_id,
        &span.namespace,
        &span.trace_id,
        span.start_time
            .timestamp_nanos_opt()
            .ok_or(KeyError::TimestampOutOfRange)?,
        &span.span_id,
    )
}

fn split_span_key(key: &str) -> Result<Vec<&str>, KeyError> {
    let parts = key.split('/').collect::<Vec<_>>();

    if parts.len() != 10
        || parts[0] != SPAN_KEY_VERSION
        || parts[1] != "t"
        || parts[3] != "ns"
        || parts[5] != "tr"
        || parts[7] != "sp"
    {
        return Err(KeyError::InvalidFormat);
    }

    validate_segment("tenant_id", parts[2])?;
    validate_segment("namespace", parts[4])?;
    validate_segment("trace_id", parts[6])?;
    validate_segment("span_id", parts[9])?;

    Ok(parts)
}

fn validate_segment(field: &'static str, value: &str) -> Result<(), KeyError> {
    if value.is_empty() {
        return Err(KeyError::EmptySegment { field });
    }

    if value.contains('/') {
        return Err(KeyError::SegmentContainsDelimiter { field });
    }

    Ok(())
}

fn validate_timestamp(start_time_ns: i64) -> Result<(), KeyError> {
    if start_time_ns < 0 {
        return Err(KeyError::NegativeTimestamp { start_time_ns });
    }

    Ok(())
}

fn decode_timestamp(value: &str) -> Result<i64, KeyError> {
    if value.len() != TIMESTAMP_WIDTH || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(KeyError::InvalidTimestamp {
            value: value.to_string(),
        });
    }

    value.parse().map_err(|_| KeyError::InvalidTimestamp {
        value: value.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{Span, SpanStatus, SpanType};
    use chrono::{TimeZone, Utc};

    #[test]
    fn span_key_round_trips() {
        let encoded = encode_span_key("tenant-1", "default", "trace-1", 42, "span-1")
            .expect("key should encode");

        let decoded = decode_span_key(&encoded).expect("key should decode");

        assert_eq!(
            decoded,
            SpanKey {
                tenant_id: "tenant-1".to_string(),
                namespace: "default".to_string(),
                trace_id: "trace-1".to_string(),
                start_time_ns: 42,
                span_id: "span-1".to_string(),
            }
        );
    }

    #[test]
    fn span_keys_sort_by_trace_then_time_then_span_id() {
        let mut keys = vec![
            encode_span_key("tenant-1", "default", "trace-2", 1, "span-1").unwrap(),
            encode_span_key("tenant-1", "default", "trace-1", 10, "span-2").unwrap(),
            encode_span_key("tenant-1", "default", "trace-1", 2, "span-1").unwrap(),
            encode_span_key("tenant-1", "default", "trace-1", 10, "span-1").unwrap(),
        ];

        keys.sort();

        let decoded = keys
            .iter()
            .map(|key| decode_span_key(key).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(decoded[0].trace_id, "trace-1");
        assert_eq!(decoded[0].start_time_ns, 2);
        assert_eq!(decoded[1].trace_id, "trace-1");
        assert_eq!(decoded[1].start_time_ns, 10);
        assert_eq!(decoded[1].span_id, "span-1");
        assert_eq!(decoded[2].trace_id, "trace-1");
        assert_eq!(decoded[2].start_time_ns, 10);
        assert_eq!(decoded[2].span_id, "span-2");
        assert_eq!(decoded[3].trace_id, "trace-2");
    }

    #[test]
    fn prefixes_match_expected_key_scopes() {
        let key = encode_span_key("tenant-1", "default", "trace-1", 42, "span-1").unwrap();

        assert!(key.starts_with(&tenant_prefix("tenant-1").unwrap()));
        assert!(key.starts_with(&namespace_prefix("tenant-1", "default").unwrap()));
        assert!(key.starts_with(&trace_prefix("tenant-1", "default", "trace-1").unwrap()));
        assert!(!key.starts_with(&trace_prefix("tenant-1", "default", "trace-2").unwrap()));
    }

    #[test]
    fn negative_timestamps_are_rejected() {
        let error = encode_span_key("tenant-1", "default", "trace-1", -1, "span-1")
            .expect_err("negative timestamp should fail");

        assert_eq!(error, KeyError::NegativeTimestamp { start_time_ns: -1 });
    }

    #[test]
    fn empty_segments_are_rejected() {
        let error = encode_span_key("", "default", "trace-1", 1, "span-1")
            .expect_err("empty tenant should fail");

        assert_eq!(error, KeyError::EmptySegment { field: "tenant_id" });
    }

    #[test]
    fn delimiter_in_segments_is_rejected() {
        let error = encode_span_key("tenant/1", "default", "trace-1", 1, "span-1")
            .expect_err("delimiter should fail");

        assert_eq!(
            error,
            KeyError::SegmentContainsDelimiter { field: "tenant_id" }
        );
    }

    #[test]
    fn invalid_keys_are_rejected() {
        assert_eq!(
            decode_span_key(b"v1/t/tenant-1/ns/default"),
            Err(KeyError::InvalidFormat)
        );
        assert_eq!(
            decode_span_key(b"v1/t/tenant-1/ns/default/tr/trace-1/sp/not-a-time/span-1"),
            Err(KeyError::InvalidTimestamp {
                value: "not-a-time".to_string(),
            })
        );
    }

    #[test]
    fn span_storage_key_uses_span_identity_and_start_time() {
        let span = Span {
            tenant_id: "tenant-1".to_string(),
            namespace: "default".to_string(),
            trace_id: "trace-1".to_string(),
            span_id: "span-1".to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: "gpt-4o".to_string(),
            start_time: Utc.with_ymd_and_hms(2026, 6, 4, 10, 30, 0).unwrap(),
            end_time: None,
            status: SpanStatus::Ok,
            attributes: serde_json::Value::Null,
            input: None,
            output: None,
            tokens: None,
            cost_usd: None,
            model: None,
            provider: None,
            error: None,
            tags: Vec::new(),
            scores: Vec::new(),
        };

        let key = encode_span_storage_key(&span).expect("span key should encode");
        let decoded = decode_span_key(&key).expect("span key should decode");

        assert_eq!(decoded.tenant_id, span.tenant_id);
        assert_eq!(decoded.namespace, span.namespace);
        assert_eq!(decoded.trace_id, span.trace_id);
        assert_eq!(decoded.span_id, span.span_id);
        assert_eq!(
            decoded.start_time_ns,
            span.start_time
                .timestamp_nanos_opt()
                .ok_or(KeyError::TimestampOutOfRange)
                .unwrap()
        );
    }
}
