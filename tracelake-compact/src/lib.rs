//! Compaction: reads hot spans from SlateDB and writes trace packs to an
//! object store.

pub mod compactor;
pub mod trace_pack_writer;

pub use compactor::{CompactError, TraceGroup, collect_traces};
pub use trace_pack_writer::TracePackWriter;
