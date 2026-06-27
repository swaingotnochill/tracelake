use thiserror::Error;
use tracelake_core::KeyError;

#[derive(Debug, Error)]
/// Errors returned by TraceLake ingest operations.
pub enum IngestError {
    #[error("span key encoding failed")]
    Key(#[from] KeyError),

    #[error("span serialization failed")]
    Json(#[from] serde_json::Error),

    #[error("slatedb operation failed")]
    SlateDb(#[from] slatedb::Error),
}
