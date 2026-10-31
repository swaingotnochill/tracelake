//! Compaction: reads hot spans and records from SlateDB and writes immutable
//! segments to an object store.

pub mod bloom_writer;
pub mod column_writer;
pub mod compactor;
pub mod manifest_writer;
pub mod record_compactor;
pub mod record_pack_reader;
pub mod record_stats_writer;
pub mod schema_store;
pub mod stats_writer;
pub mod text_indexer;
pub mod trace_pack_writer;

pub use bloom_writer::{build_record_bloom, build_segment_bloom};
pub use column_writer::{build_record_batch, build_span_batch, encode_parquet};
pub use compactor::{CompactError, Compactor, TraceGroup, collect_traces, group_spans};
pub use manifest_writer::{publish_manifest, read_manifest, read_record_manifest};
pub use record_compactor::{RecordCompactor, RecordGroup, collect_record_groups};
pub use record_pack_reader::RecordPackReader;
pub use record_stats_writer::build_record_stats;
pub use schema_store::{put_schema, read_schema};
pub use stats_writer::build_segment_stats;
pub use text_indexer::{
    MAX_INDEXED_TEXT_BYTES, TextIndexError, build_text_index, search_text_index,
};
pub use trace_pack_writer::TracePackWriter;

pub(crate) fn validate_location(
    tenant_id: &str,
    namespace: &str,
    table: Option<&str>,
) -> Result<(), CompactError> {
    tracelake_core::validate_path_segment("tenant_id", tenant_id)?;
    tracelake_core::validate_path_segment("namespace", namespace)?;
    if let Some(table) = table {
        tracelake_core::validate_path_segment("table", table)?;
    }

    Ok(())
}
