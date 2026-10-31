//! Write-path endpoints: span ingest and compaction.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tracelake_compact::{Compactor, RecordCompactor};
use tracelake_core::Span;
use tracelake_ingest::IngestError;

use crate::AppState;

/// Body of `POST /v1/spans`.
#[derive(Debug, Deserialize)]
pub struct SpansBody {
    pub spans: Vec<Span>,
}

/// Acknowledgement of `POST /v1/spans`.
#[derive(Debug, serde::Serialize)]
pub struct WrittenBody {
    pub written: usize,
}

/// Error body for every failing endpoint.
#[derive(Debug, serde::Serialize)]
pub struct ErrorBody {
    pub error: String,
}

/// Ingests a batch of spans into the hot store.
pub async fn post_spans(
    State(state): State<AppState>,
    Json(body): Json<SpansBody>,
) -> Result<(StatusCode, Json<WrittenBody>), (StatusCode, Json<ErrorBody>)> {
    state
        .db
        .put_spans(&body.spans)
        .await
        .map_err(|error| ingest_error_response(&error))?;

    Ok((
        StatusCode::OK,
        Json(WrittenBody {
            written: body.spans.len(),
        }),
    ))
}

/// Compacts the hot store into segments on the segment store. Runs one
/// compaction per (tenant, namespace) present in the hot store.
pub async fn post_compact(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let compactor = Compactor::new(Arc::clone(&state.store), state.base_path.clone());
    let entries = compactor
        .compact(&state.db)
        .await
        .map_err(|error| error_response(&error))?;

    let record_entries = RecordCompactor::new(Arc::clone(&state.store), state.base_path.clone())
        .compact(&state.db)
        .await
        .map_err(|error| error_response(&error))?;

    let segments: Vec<&str> = entries
        .iter()
        .map(|entry| entry.segment_id.as_str())
        .collect();
    let record_segments: Vec<&str> = record_entries
        .iter()
        .map(|entry| entry.segment_id.as_str())
        .collect();

    Ok((
        StatusCode::OK,
        Json(json!({ "segments": segments, "record_segments": record_segments })),
    ))
}

/// Maps ingest failures to status codes: invalid spans are client errors,
/// everything else is a server failure.
fn ingest_error_response(error: &IngestError) -> (StatusCode, Json<ErrorBody>) {
    let status = match error {
        IngestError::Key(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };

    (
        status,
        Json(ErrorBody {
            error: error.to_string(),
        }),
    )
}

/// Maps any other failure to a 500 with the error message.
fn error_response(error: &impl std::fmt::Display) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody {
            error: error.to_string(),
        }),
    )
}
