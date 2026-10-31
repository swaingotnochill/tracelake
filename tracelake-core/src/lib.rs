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
    KeyError, MAX_SEGMENT_BYTES, RecordKey, SpanKey, TombstoneKey, decode_record_key,
    decode_span_key, decode_tombstone_key, encode_record_key, encode_record_tombstone_key,
    encode_span_key, encode_span_storage_key, encode_trace_tombstone_key, namespace_prefix,
    namespace_record_prefix, record_prefix, record_root_prefix, record_tombstone_prefix,
    span_root_prefix, tenant_prefix, tombstone_root_prefix, trace_prefix, trace_tombstone_prefix,
    validate_path_segment,
};

pub use schema::{
    EvolutionError, FieldDef, FieldType, FieldValue, MAX_FIELD_NAME_BYTES, Record, RecordError,
    SCHEMA_VERSION, SchemaBuilder, SchemaError, TableSchema, coerce_value, typed_value,
};

pub use segment::{
    FieldRange, MAX_TRACKED_DISTINCT_VALUES, RecordStats, SegmentEntry, SegmentFile,
    SegmentFileKind, SegmentManifest, SegmentStats, Tombstone, segment_id,
};

pub use span::{EvalScore, Span, SpanStatus, SpanType, TokenUsage, Trace};

pub use record_pack::{
    PackField, RecordPack, RecordPackError, decode_record_pack, encode_record_pack,
};
pub use trace_pack::{
    TRACE_PACK_HEADER_LEN, TraceBlockRef, TracePack, TracePackError, TracePackHeader,
    decode_trace_block, decode_trace_pack, decode_trace_pack_header, encode_trace_pack,
    find_trace_block,
};
