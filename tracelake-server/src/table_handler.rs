//! Table endpoints: schema registration, record ingest, and record queries.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tracelake_compact::{CompactError, put_schema, read_schema};
use tracelake_core::{Record, TableSchema};
use tracelake_ingest::{EvalLog, IngestError, eval_sample_schema, sample_records};
use tracelake_query::{AggregateRequest, AggregateResult, QueryError, RecordQuery};

use crate::AppState;
use crate::ingest_handler::ErrorBody;
use crate::query_handler::limit_response;

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

/// Registers a table schema. An identical shape is a no-op, a compatible
/// change becomes the next version, and an incompatible change is refused.
pub async fn put_table_schema(
    State(state): State<AppState>,
    Path((tenant_id, namespace, table)): Path<(String, String, String)>,
    Json(schema): Json<TableSchema>,
) -> ApiResult<Value> {
    validate_location(&tenant_id, &namespace, &table)?;

    if schema.table != table {
        return Err(bad_request(format!(
            "schema table '{}' does not match path table '{table}'",
            schema.table
        )));
    }

    let registered = put_schema(
        &state.store,
        &state.base_path,
        &tenant_id,
        &namespace,
        &schema,
    )
    .await
    .map_err(|error| match error {
        CompactError::Evolution(ref evolution) => evolution_error(evolution),
        CompactError::Schema(_) => bad_request(error.to_string()),
        other => server_error(other.to_string()),
    })?;

    Ok((
        StatusCode::OK,
        Json(json!({ "table": table, "version": registered.version })),
    ))
}

/// Reads the registered schema for one table.
pub async fn get_table_schema(
    State(state): State<AppState>,
    Path((tenant_id, namespace, table)): Path<(String, String, String)>,
) -> ApiResult<TableSchema> {
    validate_location(&tenant_id, &namespace, &table)?;
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
    validate_location(&tenant_id, &namespace, &table)?;
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
    validate_location(&tenant_id, &namespace, &table)?;
    tracelake_core::validate_path_segment("record_id", &record_id)
        .map_err(|error| bad_request(error.to_string()))?;
    let record = state
        .limits
        .run(
            &tenant_id,
            state
                .reader
                .get_record(&tenant_id, &namespace, &table, &record_id),
        )
        .await
        .map_err(limit_response)?
        .map_err(query_error)?;

    match record {
        Some(record) => Ok((StatusCode::OK, Json(record.fields))),
        None => Err(not_found(format!("record {record_id} not found"))),
    }
}

/// Deletes one record and writes a tombstone that masks compacted copies.
pub async fn delete_record(
    State(state): State<AppState>,
    Path((tenant_id, namespace, table, record_id)): Path<(String, String, String, String)>,
) -> ApiResult<Value> {
    validate_location(&tenant_id, &namespace, &table)?;
    tracelake_core::validate_path_segment("record_id", &record_id)
        .map_err(|error| bad_request(error.to_string()))?;
    let schema = load_schema(&state, &tenant_id, &namespace, &table).await?;
    let existed = state
        .db
        .delete_record(&tenant_id, &namespace, &schema, &record_id)
        .await
        .map_err(ingest_error)?;

    Ok((StatusCode::OK, Json(json!({ "deleted": existed }))))
}

/// Filters and projects one table.
pub async fn post_record_query(
    State(state): State<AppState>,
    Path((tenant_id, namespace, table)): Path<(String, String, String)>,
    Json(query): Json<RecordQuery>,
) -> ApiResult<RecordListBody> {
    validate_location(&tenant_id, &namespace, &table)?;
    let result = state
        .limits
        .run(
            &tenant_id,
            state
                .reader
                .query_records(&tenant_id, &namespace, &table, &query),
        )
        .await
        .map_err(limit_response)?
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

/// Groups and aggregates one table's column files.
pub async fn post_record_aggregate(
    State(state): State<AppState>,
    Path((tenant_id, namespace, table)): Path<(String, String, String)>,
    Json(request): Json<AggregateRequest>,
) -> ApiResult<AggregateResult> {
    validate_location(&tenant_id, &namespace, &table)?;
    let result = state
        .limits
        .run(
            &tenant_id,
            state
                .reader
                .aggregate_records(&tenant_id, &namespace, &table, &request),
        )
        .await
        .map_err(limit_response)?
        .map_err(query_error)?;

    Ok((StatusCode::OK, Json(result)))
}

/// Ingests one inspect-ai eval log as rows of `inspect_ai_samples`.
pub async fn post_evals(
    State(state): State<AppState>,
    Json(body): Json<EvalsBody>,
) -> ApiResult<Value> {
    validate_location(
        &body.tenant_id,
        &body.namespace,
        tracelake_ingest::EVAL_SAMPLE_TABLE,
    )?;
    let schema = match read_schema(
        &state.store,
        &state.base_path,
        &body.tenant_id,
        &body.namespace,
        tracelake_ingest::EVAL_SAMPLE_TABLE,
    )
    .await
    .map_err(|error| server_error(error.to_string()))?
    {
        Some(registered) => registered,
        None => put_schema(
            &state.store,
            &state.base_path,
            &body.tenant_id,
            &body.namespace,
            &eval_sample_schema(),
        )
        .await
        .map_err(|error| server_error(error.to_string()))?,
    };

    let mut records =
        sample_records(&body.log, None).map_err(|error| bad_request(error.to_string()))?;
    for record in &mut records {
        record.fields.retain(|name, _| schema.field(name).is_some());
    }

    state
        .db
        .put_records(&body.tenant_id, &body.namespace, &schema, &records)
        .await
        .map_err(ingest_error)?;

    Ok((
        StatusCode::OK,
        Json(json!({ "written": records.len(), "table": schema.table })),
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

fn evolution_error(evolution: &tracelake_core::EvolutionError) -> (StatusCode, Json<ErrorBody>) {
    let status = match evolution {
        tracelake_core::EvolutionError::RequiredWithoutDefault { .. } => StatusCode::BAD_REQUEST,
        _ => StatusCode::CONFLICT,
    };

    (
        status,
        Json(ErrorBody {
            error: evolution.to_string(),
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
    eprintln!("internal error: {message}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody {
            error: "internal error".to_string(),
        }),
    )
}

fn validate_location(
    tenant_id: &str,
    namespace: &str,
    table: &str,
) -> Result<(), (StatusCode, Json<ErrorBody>)> {
    for (field, value) in [
        ("tenant_id", tenant_id),
        ("namespace", namespace),
        ("table", table),
    ] {
        tracelake_core::validate_path_segment(field, value)
            .map_err(|error| bad_request(error.to_string()))?;
    }

    Ok(())
}
