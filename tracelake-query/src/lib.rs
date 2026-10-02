//! Query engine: fetches traces by merging the hot SlateDB store with
//! compacted segments.

pub mod merge;
pub mod search;
pub mod trace_fetch;

pub use merge::merge_spans;
pub use search::{DEFAULT_SEARCH_LIMIT, TraceSearch, TraceSummary};
pub use trace_fetch::{QueryError, TraceLakeReader};
