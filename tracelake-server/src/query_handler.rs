//! Read-path endpoints: trace fetch and search.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use tracelake_core::Trace;
use tracelake_query::{AggregateRequest, AggregateResult, QueryError, TraceSearch, TraceSummary};

use crate::AppState;
use crate::ingest_handler::ErrorBody;

/// Body of `POST /v1/traces/search`: routing ids plus the filters.
#[derive(Debug, Deserialize)]
pub struct SearchBody {
    pub tenant_id: String,
    pub namespace: String,
    #[serde(flatten)]
    pub search: TraceSearch,
}

/// Fetches one full trace: hot spans merged with compacted segments.
pub async fn get_trace(
    State(state): State<AppState>,
    Path((tenant_id, namespace, trace_id)): Path<(String, String, String)>,
) -> Result<(StatusCode, Json<Trace>), (StatusCode, Json<ErrorBody>)> {
    let trace = state
        .reader
        .get_trace(&tenant_id, &namespace, &trace_id)
        .await
        .map_err(|error| query_error_response(&error))?;

    match trace {
        Some(trace) => Ok((StatusCode::OK, Json(trace))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: format!("trace {trace_id} not found"),
            }),
        )),
    }
}

/// Searches traces by span status, model, and time range.
pub async fn post_search(
    State(state): State<AppState>,
    Json(body): Json<SearchBody>,
) -> Result<(StatusCode, Json<Vec<TraceSummary>>), (StatusCode, Json<ErrorBody>)> {
    let summaries = state
        .reader
        .search_traces(&body.tenant_id, &body.namespace, &body.search)
        .await
        .map_err(|error| query_error_response(&error))?;

    Ok((StatusCode::OK, Json(summaries)))
}

/// Body of `POST /v1/spans/aggregate`: routing ids plus the aggregate request.
#[derive(Debug, Deserialize)]
pub struct SpanAggregateBody {
    pub tenant_id: String,
    pub namespace: String,
    #[serde(flatten)]
    pub request: AggregateRequest,
}

/// Groups and aggregates the span column files.
pub async fn post_span_aggregate(
    State(state): State<AppState>,
    Json(body): Json<SpanAggregateBody>,
) -> Result<(StatusCode, Json<AggregateResult>), (StatusCode, Json<ErrorBody>)> {
    let result = state
        .reader
        .aggregate_spans(&body.tenant_id, &body.namespace, &body.request)
        .await
        .map_err(|error| query_error_response(&error))?;

    Ok((StatusCode::OK, Json(result)))
}

/// Maps query failures to status codes: invalid arguments are client
/// errors, storage failures are server errors.
fn query_error_response(error: &QueryError) -> (StatusCode, Json<ErrorBody>) {
    let status = match error {
        QueryError::Key(_) | QueryError::Aggregate { .. } | QueryError::Record(_) => {
            StatusCode::BAD_REQUEST
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };

    if status == StatusCode::INTERNAL_SERVER_ERROR {
        eprintln!("internal error: {error}");
        return (
            status,
            Json(ErrorBody {
                error: "internal error".to_string(),
            }),
        );
    }

    (
        status,
        Json(ErrorBody {
            error: error.to_string(),
        }),
    )
}
