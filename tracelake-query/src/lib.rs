//! Query engine: fetches traces by merging the hot SlateDB store with
//! compacted segments, and queries user-defined tables.

pub mod aggregate;
pub mod merge;
pub mod record_query;
pub mod search;
pub mod trace_fetch;

pub use aggregate::{AggregateMetric, AggregateRequest, AggregateResult, DEFAULT_AGGREGATE_LIMIT};
pub use merge::merge_spans;
pub use record_query::{DEFAULT_RECORD_LIMIT, RecordFilter, RecordQuery, RecordQueryResult};
pub use search::{DEFAULT_SEARCH_LIMIT, TraceSearch, TraceSummary};
pub use trace_fetch::{QueryError, TraceLakeReader};
