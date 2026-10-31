pub mod bloom;
mod crc;
pub mod key;
pub mod record_pack;
pub mod schema;
pub mod segment;
pub mod span;
pub mod trace_pack;

pub use bloom::{BloomError, BloomFilter, DEFAULT_FALSE_POSITIVE_RATE};
pub use key::{
    KeyError, RecordKey, SpanKey, decode_record_key, decode_span_key, encode_record_key,
    encode_span_key, encode_span_storage_key, namespace_prefix, namespace_record_prefix,
    record_prefix, record_root_prefix, span_root_prefix, tenant_prefix, trace_prefix,
};

pub use schema::{
    EvolutionError, FieldDef, FieldType, FieldValue, Record, RecordError, SCHEMA_VERSION,
    SchemaBuilder, SchemaError, TableSchema, typed_value,
};

pub use segment::{
    FieldRange, MAX_TRACKED_DISTINCT_VALUES, RecordStats, SegmentEntry, SegmentFile,
    SegmentFileKind, SegmentManifest, SegmentStats, segment_id,
};

pub use span::{EvalScore, Span, SpanStatus, SpanType, TokenUsage, Trace};

pub use record_pack::{
    PackField, RecordPack, RecordPackError, decode_record_pack, encode_record_pack,
};
pub use trace_pack::{TracePack, TracePackError, decode_trace_pack, encode_trace_pack};
