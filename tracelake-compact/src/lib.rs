//! Compaction: reads hot spans from SlateDB and writes trace packs to an
//! object store.

pub mod bloom_writer;
pub mod compactor;
pub mod manifest_writer;
pub mod stats_writer;
pub mod trace_pack_writer;

pub use bloom_writer::build_segment_bloom;
pub use compactor::{CompactError, Compactor, TraceGroup, collect_traces, group_spans};
pub use manifest_writer::{publish_manifest, read_manifest};
pub use stats_writer::build_segment_stats;
pub use trace_pack_writer::TracePackWriter;
