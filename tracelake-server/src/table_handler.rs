//! Table endpoints: schema registration, record ingest, and record queries.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tracelake_compact::{CompactError, put_schema, read_schema};
use tracelake_core::{Record, TableSchema};
use tracelake_ingest::{EvalLog, IngestError, eval_sample_schema, ingest_eval_log};
use tracelake_query::{QueryError, RecordQuery};

use crate::AppState;
use crate::ingest_handler::ErrorBody;

type ApiResult<T> = Result<(StatusCode, Json<T>), (StatusCode, Json<ErrorBody>)>;

#[derive(Debug, Deserialize)]
pub struct RecordsBody {
    pub records: Vec<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
pub struct RecordListBody {
    pub records: Vec<Map<String, Value>>,
    pub scanned_segments: u64,
    pub pruned_segments: u64,
}

#[derive(Debug, Deserialize)]
pub struct EvalsBody {
    pub tenant_id: String,
    pub namespace: String,
    pub log: EvalLog,
}

/// Registers a table schema. Re-registering the same shape is a no-op; a
/// different shape is refused so published packs stay decodable.
pub async fn put_table_schema(
    State(state): State<AppState>,
    Path((tenant_id, namespace, table)): Path<(String, String, String)>,
    Json(schema): Json<TableSchema>,
) -> ApiResult<Value> {
    if schema.table != table {
        return Err(bad_request(format!(
            "schema table '{}' does not match path table '{table}'",
            schema.table
        )));
    }

    put_schema(
        &state.store,
        &state.base_path,
        &tenant_id,
        &namespace,
        &schema,
    )
    .await
    .map_err(|error| match error {
        CompactError::SchemaConflict { .. } => (
            StatusCode::CONFLICT,
            Json(ErrorBody {
                error: error.to_string(),
            }),
        ),
        CompactError::Schema(_) => bad_request(error.to_string()),
        other => server_error(other.to_string()),
    })?;

    Ok((StatusCode::OK, Json(json!({ "table": table }))))
}

/// Reads the registered schema for one table.
pub async fn get_table_schema(
    State(state): State<AppState>,
    Path((tenant_id, namespace, table)): Path<(String, String, String)>,
) -> ApiResult<TableSchema> {
    let schema = read_schema(
        &state.store,
        &state.base_path,
        &tenant_id,
        &namespace,
        &table,
    )
    .await
    .map_err(|error| server_error(error.to_string()))?;

    match schema {
        Some(schema) => Ok((StatusCode::OK, Json(schema))),
        None => Err(not_found(format!(
            "table '{table}' has no registered schema"
        ))),
    }
}

/// Validates and writes records against the registered schema.
pub async fn post_records(
    State(state): State<AppState>,
    Path((tenant_id, namespace, table)): Path<(String, String, String)>,
    Json(body): Json<RecordsBody>,
) -> ApiResult<Value> {
    let schema = load_schema(&state, &tenant_id, &namespace, &table).await?;
    let records: Vec<Record> = body
        .records
        .into_iter()
        .map(|fields| Record { fields })
        .collect();

    state
        .db
        .put_records(&tenant_id, &namespace, &schema, &records)
        .await
        .map_err(ingest_error)?;

    Ok((StatusCode::OK, Json(json!({ "written": records.len() }))))
}

/// Fetches one record by primary key.
pub async fn get_record(
    State(state): State<AppState>,
    Path((tenant_id, namespace, table, record_id)): Path<(String, String, String, String)>,
) -> ApiResult<Map<String, Value>> {
    let record = state
        .reader
        .get_record(&tenant_id, &namespace, &table, &record_id)
        .await
        .map_err(query_error)?;

    match record {
        Some(record) => Ok((StatusCode::OK, Json(record.fields))),
        None => Err(not_found(format!("record {record_id} not found"))),
    }
}

/// Filters and projects one table.
pub async fn post_record_query(
    State(state): State<AppState>,
    Path((tenant_id, namespace, table)): Path<(String, String, String)>,
    Json(query): Json<RecordQuery>,
) -> ApiResult<RecordListBody> {
    let result = state
        .reader
        .query_records(&tenant_id, &namespace, &table, &query)
        .await
        .map_err(query_error)?;

    Ok((
        StatusCode::OK,
        Json(RecordListBody {
            records: result
                .records
                .into_iter()
                .map(|record| record.fields)
                .collect(),
            scanned_segments: result.scanned_segments,
            pruned_segments: result.pruned_segments,
        }),
    ))
}

/// Ingests one inspect-ai eval log as rows of `inspect_ai_samples`.
pub async fn post_evals(
    State(state): State<AppState>,
    Json(body): Json<EvalsBody>,
) -> ApiResult<Value> {
    let schema = eval_sample_schema();
    put_schema(
        &state.store,
        &state.base_path,
        &body.tenant_id,
        &body.namespace,
        &schema,
    )
    .await
    .map_err(|error| match error {
        CompactError::SchemaConflict { .. } => (
            StatusCode::CONFLICT,
            Json(ErrorBody {
                error: error.to_string(),
            }),
        ),
        other => server_error(other.to_string()),
    })?;

    let written = ingest_eval_log(&state.db, &body.tenant_id, &body.namespace, &body.log)
        .await
        .map_err(ingest_error)?;

    Ok((
        StatusCode::OK,
        Json(json!({ "written": written, "table": schema.table })),
    ))
}

async fn load_schema(
    state: &AppState,
    tenant_id: &str,
    namespace: &str,
    table: &str,
) -> Result<TableSchema, (StatusCode, Json<ErrorBody>)> {
    read_schema(&state.store, &state.base_path, tenant_id, namespace, table)
        .await
        .map_err(|error| server_error(error.to_string()))?
        .ok_or_else(|| not_found(format!("table '{table}' has no registered schema")))
}

fn ingest_error(error: IngestError) -> (StatusCode, Json<ErrorBody>) {
    let status = match &error {
        IngestError::Key(_) | IngestError::Record(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };

    (
        status,
        Json(ErrorBody {
            error: error.to_string(),
        }),
    )
}

fn query_error(error: QueryError) -> (StatusCode, Json<ErrorBody>) {
    let status = match &error {
        QueryError::SchemaMissing { .. } => StatusCode::NOT_FOUND,
        QueryError::InvalidQuery { .. } | QueryError::Record(_) | QueryError::Key(_) => {
            StatusCode::BAD_REQUEST
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };

    (
        status,
        Json(ErrorBody {
            error: error.to_string(),
        }),
    )
}

fn bad_request(message: String) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::BAD_REQUEST, Json(ErrorBody { error: message }))
}

fn not_found(message: String) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::NOT_FOUND, Json(ErrorBody { error: message }))
}

fn server_error(message: String) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody { error: message }),
    )
}
