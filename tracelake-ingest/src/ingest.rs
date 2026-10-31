use thiserror::Error;
use tracelake_core::{KeyError, RecordError};

#[derive(Debug, Error)]
/// Errors returned by TraceLake ingest operations.
pub enum IngestError {
    #[error("span key encoding failed: {0}")]
    Key(#[from] KeyError),

    #[error("record does not match its schema: {0}")]
    Record(#[from] RecordError),

    #[error("span serialization failed")]
    Json(#[from] serde_json::Error),

    #[error("slatedb operation failed")]
    SlateDb(#[from] slatedb::Error),
}
