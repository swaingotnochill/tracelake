use crate::span::Span;
use thiserror::Error;

const SPAN_KEY_VERSION: &str = "v1";
const TIMESTAMP_WIDTH: usize = 20;

/// First path segment of every span key. Records live under a disjoint root
/// so a span scan can never deserialize a record value.
const SPAN_ROOT: &str = "v1/t/";

/// First path segment of every user-defined record key.
const RECORD_ROOT: &str = "v1/r/";

#[derive(Debug, Clone, PartialEq, Eq)]
/// Decoded components of a SlateDB span storage key.
pub struct SpanKey {
    pub tenant_id: String,
    pub namespace: String,
    pub trace_id: String,
    pub start_time_ns: i64,
    pub span_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Decoded components of a SlateDB record storage key.
pub struct RecordKey {
    pub tenant_id: String,
    pub namespace: String,
    pub table: String,
    pub record_id: String,
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

/// Returns the byte prefix shared by every span key and by no record key.
pub fn span_root_prefix() -> &'static [u8] {
    SPAN_ROOT.as_bytes()
}

/// Returns the byte prefix shared by every record key and by no span key.
pub fn record_root_prefix() -> &'static [u8] {
    RECORD_ROOT.as_bytes()
}

/// Builds the canonical storage key for one user-defined record:
/// `v1/r/t/{tenant}/ns/{namespace}/tb/{table}/r/{record_id}`.
pub fn encode_record_key(
    tenant_id: &str,
    namespace: &str,
    table: &str,
    record_id: &str,
) -> Result<Vec<u8>, KeyError> {
    validate_segment("tenant_id", tenant_id)?;
    validate_segment("namespace", namespace)?;
    validate_segment("table", table)?;
    validate_segment("record_id", record_id)?;

    let mut key = Vec::with_capacity(
        RECORD_ROOT.len() + tenant_id.len() + namespace.len() + table.len() + record_id.len() + 22,
    );
    key.extend_from_slice(RECORD_ROOT.as_bytes());
    key.extend_from_slice(b"t/");
    key.extend_from_slice(tenant_id.as_bytes());
    key.extend_from_slice(b"/ns/");
    key.extend_from_slice(namespace.as_bytes());
    key.extend_from_slice(b"/tb/");
    key.extend_from_slice(table.as_bytes());
    key.extend_from_slice(b"/r/");
    key.extend_from_slice(record_id.as_bytes());

    Ok(key)
}

/// Decodes a record storage key into its tenant, namespace, table, and record
/// id.
pub fn decode_record_key(key: &[u8]) -> Result<RecordKey, KeyError> {
    let key = std::str::from_utf8(key).map_err(|_| KeyError::InvalidUtf8)?;
    let parts = key.split('/').collect::<Vec<_>>();

    if parts.len() != 10
        || parts[0] != SPAN_KEY_VERSION
        || parts[1] != "r"
        || parts[2] != "t"
        || parts[4] != "ns"
        || parts[6] != "tb"
        || parts[8] != "r"
    {
        return Err(KeyError::InvalidFormat);
    }

    validate_segment("tenant_id", parts[3])?;
    validate_segment("namespace", parts[5])?;
    validate_segment("table", parts[7])?;
    validate_segment("record_id", parts[9])?;

    Ok(RecordKey {
        tenant_id: parts[3].to_string(),
        namespace: parts[5].to_string(),
        table: parts[7].to_string(),
        record_id: parts[9].to_string(),
    })
}

/// Returns the byte prefix that scopes all records of one table.
pub fn record_prefix(tenant_id: &str, namespace: &str, table: &str) -> Result<Vec<u8>, KeyError> {
    validate_segment("tenant_id", tenant_id)?;
    validate_segment("namespace", namespace)?;
    validate_segment("table", table)?;
    Ok(format!("v1/r/t/{tenant_id}/ns/{namespace}/tb/{table}/r/").into_bytes())
}

/// Returns the byte prefix that scopes every record in one namespace.
pub fn namespace_record_prefix(tenant_id: &str, namespace: &str) -> Result<Vec<u8>, KeyError> {
    validate_segment("tenant_id", tenant_id)?;
    validate_segment("namespace", namespace)?;
    Ok(format!("v1/r/t/{tenant_id}/ns/{namespace}/tb/").into_bytes())
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

    let timestamp = encode_timestamp_bytes(start_time_ns);

    let mut key = Vec::with_capacity(
        SPAN_KEY_VERSION.len()
            + tenant_id.len()
            + namespace.len()
            + trace_id.len()
            + span_id.len()
            + timestamp.len()
            + 16,
    );
    key.extend_from_slice(SPAN_KEY_VERSION.as_bytes());
    key.extend_from_slice(b"/t/");
    key.extend_from_slice(tenant_id.as_bytes());
    key.extend_from_slice(b"/ns/");
    key.extend_from_slice(namespace.as_bytes());
    key.extend_from_slice(b"/tr/");
    key.extend_from_slice(trace_id.as_bytes());
    key.extend_from_slice(b"/sp/");
    key.extend_from_slice(&timestamp);
    key.push(b'/');
    key.extend_from_slice(span_id.as_bytes());

    Ok(key)
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

/// Encodes a non-negative timestamp as the fixed-width, zero-padded decimal
/// segment of the span key. Every non-negative `i64` fits: the maximum (19
/// digits) pads to exactly `TIMESTAMP_WIDTH` digits, so lexicographic byte
/// order on this segment equals numeric order.
fn encode_timestamp_bytes(start_time_ns: i64) -> [u8; TIMESTAMP_WIDTH] {
    let mut segment = [b'0'; TIMESTAMP_WIDTH];
    let mut value = start_time_ns;
    let mut index = TIMESTAMP_WIDTH;

    while index > 0 {
        index -= 1;
        segment[index] = b'0' + (value % 10) as u8;
        value /= 10;
    }

    segment
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
    if !is_timestamp_segment(value) {
        return Err(KeyError::InvalidTimestamp {
            value: value.to_string(),
        });
    }

    parse_timestamp_digits(value.as_bytes()).ok_or(KeyError::InvalidTimestamp {
        value: value.to_string(),
    })
}

/// Whether `value` is exactly `TIMESTAMP_WIDTH` ASCII digits - the shape of
/// a span key timestamp segment.
fn is_timestamp_segment(value: &str) -> bool {
    value.len() == TIMESTAMP_WIDTH && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Folds digit bytes into an i64 with checked arithmetic; `None` when the
/// digits do not all fit (a width-20 value can exceed `i64::MAX`).
fn parse_timestamp_digits(digits: &[u8]) -> Option<i64> {
    let mut value: i64 = 0;

    for &digit in digits {
        if !digit.is_ascii_digit() {
            return None;
        }

        value = value.checked_mul(10)?;
        value = value.checked_add(i64::from(digit - b'0'))?;
    }

    Some(value)
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
        let mut keys = [
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

    #[test]
    fn record_key_round_trips() {
        let encoded = encode_record_key("tenant-1", "default", "eval_samples", "sample-7")
            .expect("record key should encode");

        assert_eq!(
            decode_record_key(&encoded).expect("record key should decode"),
            RecordKey {
                tenant_id: "tenant-1".to_string(),
                namespace: "default".to_string(),
                table: "eval_samples".to_string(),
                record_id: "sample-7".to_string(),
            }
        );
    }

    #[test]
    fn record_keys_sort_by_table_then_record_id() {
        let mut keys = [
            encode_record_key("tenant-1", "default", "samples", "record-2").unwrap(),
            encode_record_key("tenant-1", "default", "samples", "record-1").unwrap(),
            encode_record_key("tenant-1", "default", "other", "record-0").unwrap(),
        ];

        keys.sort();

        let decoded = keys
            .iter()
            .map(|key| decode_record_key(key).unwrap())
            .collect::<Vec<_>>();
        let order: Vec<(&str, &str)> = decoded
            .iter()
            .map(|key| (key.table.as_str(), key.record_id.as_str()))
            .collect();

        assert_eq!(
            order,
            vec![
                ("other", "record-0"),
                ("samples", "record-1"),
                ("samples", "record-2"),
            ]
        );
    }

    #[test]
    fn record_prefixes_match_expected_key_scopes() {
        let key = encode_record_key("tenant-1", "default", "samples", "record-1").unwrap();

        assert!(key.starts_with(record_root_prefix()));
        assert!(key.starts_with(&record_prefix("tenant-1", "default", "samples").unwrap()));
        assert!(key.starts_with(&namespace_record_prefix("tenant-1", "default").unwrap()));
        assert!(!key.starts_with(&record_prefix("tenant-1", "default", "other").unwrap()));
    }

    #[test]
    fn span_and_record_roots_never_overlap() {
        let span_key = encode_span_key("tenant-1", "default", "trace-1", 1, "span-1").unwrap();
        let record_key = encode_record_key("tenant-1", "default", "samples", "record-1").unwrap();

        assert!(span_key.starts_with(span_root_prefix()));
        assert!(record_key.starts_with(record_root_prefix()));
        assert!(!span_key.starts_with(record_root_prefix()));
        assert!(!record_key.starts_with(span_root_prefix()));
        assert!(decode_record_key(&span_key).is_err());
        assert!(decode_span_key(&record_key).is_err());
    }

    #[test]
    fn record_key_rejects_invalid_segments() {
        assert_eq!(
            encode_record_key("", "default", "samples", "record-1"),
            Err(KeyError::EmptySegment { field: "tenant_id" })
        );
        assert_eq!(
            encode_record_key("tenant-1", "default", "samples/v2", "record-1"),
            Err(KeyError::SegmentContainsDelimiter { field: "table" })
        );
        assert_eq!(
            encode_record_key("tenant-1", "default", "samples", "record/1"),
            Err(KeyError::SegmentContainsDelimiter { field: "record_id" })
        );
        assert_eq!(
            decode_record_key(b"v1/r/t/tenant-1/ns/default/tb/samples"),
            Err(KeyError::InvalidFormat)
        );
    }
}

#[cfg(kani)]
mod verify {
    use super::*;

    /// Proves the extracted timestamp codec round-trips every
    /// non-negative i64: encode to the fixed-width segment, decode back.
    #[kani::proof]
    #[kani::unwind(24)]
    fn timestamp_codec_round_trips_every_non_negative_value() {
        let ts: i64 = kani::any();
        kani::assume(ts >= 0);

        let segment = encode_timestamp_bytes(ts);
        let text = std::str::from_utf8(&segment).expect("encoded segments are ascii digits");
        assert_eq!(decode_timestamp(text).expect("encoded segments decode"), ts);
    }

    /// Proves the ordering law at the codec core: zero-padded width-20
    /// segments compare in bytes exactly as their numbers compare.
    #[kani::proof]
    #[kani::unwind(24)]
    fn timestamp_segments_order_like_numbers() {
        let ts1: i64 = kani::any();
        let ts2: i64 = kani::any();
        kani::assume(ts1 >= 0);
        kani::assume(ts2 >= 0);

        let a = encode_timestamp_bytes(ts1);
        let b = encode_timestamp_bytes(ts2);

        assert_eq!(a.cmp(&b), ts1.cmp(&ts2));
    }

    /// Proves the digit fold is total on arbitrary width-20 bytes: any
    /// input is either rejected (non-digit or overflow past i64::MAX) or
    /// folds to the value it spells. No std parser is involved.
    #[kani::proof]
    #[kani::unwind(24)]
    fn digit_fold_is_total_on_arbitrary_segments() {
        let bytes: [u8; TIMESTAMP_WIDTH] = kani::any();
        let _ = parse_timestamp_digits(&bytes);
    }
}
