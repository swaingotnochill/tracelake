//! Query engine: fetches traces by merging the hot SlateDB store with
//! compacted segments.

pub mod merge;
pub mod trace_fetch;

pub use merge::merge_spans;
pub use trace_fetch::{QueryError, TraceLakeReader};
