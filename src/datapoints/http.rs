use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use super::{
    BatchItem, Datapoint, DeleteCommand, Deleted, IngestOutcome, Ingested, ListQuery, Listed,
    NewDatapoint,
};
use crate::app::Services;
use crate::error::ServiceError;
use crate::http::{ApiError, Json};

pub fn routes() -> Router<Services> {
    Router::new()
        .route("/api/v1/ingest", post(ingest))
        .route("/api/v1/ingest/batch", post(ingest_batch))
        .route("/api/v1/delete", post(delete))
        .route("/api/v1/datapoints", get(list))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestRequest {
    pub idempotency_key: Option<String>,
    #[serde(flatten)]
    pub datapoint: NewDatapoint,
}

#[derive(Debug, Serialize)]
pub struct IngestResponse {
    #[serde(flatten)]
    pub datapoint: Datapoint,
    pub deduplicated: bool,
    pub replaced: bool,
}

impl From<Ingested> for IngestResponse {
    fn from(i: Ingested) -> Self {
        IngestResponse {
            datapoint: i.datapoint,
            deduplicated: i.outcome == IngestOutcome::Deduplicated,
            replaced: i.outcome == IngestOutcome::Replaced,
        }
    }
}

fn status_of(outcome: IngestOutcome) -> StatusCode {
    match outcome {
        IngestOutcome::Created => StatusCode::CREATED,
        IngestOutcome::Deduplicated | IngestOutcome::Replaced => StatusCode::OK,
    }
}

pub async fn ingest(
    State(services): State<Services>,
    Json(req): Json<IngestRequest>,
) -> Result<(StatusCode, axum::Json<IngestResponse>), ApiError> {
    let ingested = services
        .datapoints
        .ingest(req.datapoint, req.idempotency_key.as_deref())
        .await?;
    Ok((status_of(ingested.outcome), axum::Json(ingested.into())))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRequest {
    pub idempotency_key: Option<String>,
    pub items: Vec<NewDatapoint>,
}

#[derive(Debug, Serialize)]
pub struct ItemErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum BatchItemResult {
    Ok {
        status: u16,
        deduplicated: bool,
        replaced: bool,
        datapoint: Datapoint,
    },
    Err {
        status: u16,
        error: ItemErrorBody,
    },
}

#[derive(Debug, Serialize)]
pub struct BatchResponse {
    pub results: Vec<BatchItemResult>,
}

fn batch_item_result(index: usize, item: BatchItem) -> BatchItemResult {
    match item {
        BatchItem::Ingested(ingested) => {
            let status = status_of(ingested.outcome).as_u16();
            let resp = IngestResponse::from(ingested);
            BatchItemResult::Ok {
                status,
                deduplicated: resp.deduplicated,
                replaced: resp.replaced,
                datapoint: resp.datapoint,
            }
        }
        BatchItem::Failed(e) => {
            if let ServiceError::Internal(inner) = &e {
                tracing::error!(error = ?inner, item = index, "batch item failed");
            }
            let e = ApiError::from(e);
            BatchItemResult::Err {
                status: e.status().as_u16(),
                error: ItemErrorBody {
                    code: e.code().into(),
                    message: e.to_string(),
                },
            }
        }
        BatchItem::Skipped => BatchItemResult::Err {
            status: 0,
            error: ItemErrorBody {
                code: "skipped".into(),
                message: "not attempted after an earlier item failed".into(),
            },
        },
    }
}

pub async fn ingest_batch(
    State(services): State<Services>,
    Json(req): Json<BatchRequest>,
) -> Result<axum::Json<BatchResponse>, ApiError> {
    let items = services
        .datapoints
        .ingest_batch(req.items, req.idempotency_key.as_deref())
        .await?;
    let results = items
        .into_iter()
        .enumerate()
        .map(|(i, item)| batch_item_result(i, item))
        .collect();
    Ok(axum::Json(BatchResponse { results }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteRequest {
    pub project_id: Uuid,
    #[serde(default)]
    pub source_ids: Vec<String>,
    #[serde(default)]
    pub ids: Vec<Uuid>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResult {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// False when nothing was stored under the id.
    pub deleted: bool,
}

#[derive(Debug, Serialize)]
pub struct DeleteResponse {
    pub results: Vec<DeleteResult>,
}

impl From<DeleteRequest> for DeleteCommand {
    fn from(r: DeleteRequest) -> Self {
        DeleteCommand {
            project_id: r.project_id,
            source_ids: r.source_ids,
            ids: r.ids,
        }
    }
}

impl From<Deleted> for DeleteResult {
    fn from(d: Deleted) -> Self {
        DeleteResult {
            id: d.id,
            source_id: d.source_id,
            deleted: d.deleted,
        }
    }
}

/// Delete datapoints by `sourceId` or server `id`. Results come back in
/// request order, `sourceIds` first.
pub async fn delete(
    State(services): State<Services>,
    Json(req): Json<DeleteRequest>,
) -> Result<axum::Json<DeleteResponse>, ApiError> {
    let deleted = services.datapoints.delete(req.into()).await?;
    Ok(axum::Json(DeleteResponse {
        results: deleted.into_iter().map(DeleteResult::from).collect(),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    pub project_id: Uuid,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub source_id: Option<String>,
    pub audience: Option<String>,
    pub label: Option<String>,
    pub occurred_from: Option<DateTime<Utc>>,
    pub occurred_to: Option<DateTime<Utc>>,
    #[serde(default)]
    pub include_content: bool,
}

impl From<ListParams> for ListQuery {
    fn from(p: ListParams) -> Self {
        ListQuery {
            project_id: p.project_id,
            limit: p.limit,
            cursor: p.cursor,
            source_id: p.source_id,
            audience: p.audience,
            label: p.label,
            occurred_from: p.occurred_from,
            occurred_to: p.occurred_to,
            include_content: p.include_content,
        }
    }
}

/// A stored datapoint as listed: its metadata without the vector, and its
/// content only when asked for.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatapointView {
    pub id: Uuid,
    pub project_id: Uuid,
    pub source_id: Option<String>,
    pub label: String,
    pub timestamp: DateTime<Utc>,
    pub metadata: Map<String, Value>,
    pub audience: Option<String>,
    pub content_tokens: usize,
    pub content_sha256: Option<String>,
    pub embedding_model: String,
    pub embedding_dim: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl From<Listed> for DatapointView {
    fn from(Listed { meta, content }: Listed) -> Self {
        DatapointView {
            id: meta.id,
            project_id: meta.project_id,
            source_id: meta.source_id,
            label: meta.label,
            timestamp: meta.timestamp,
            metadata: meta.metadata,
            audience: meta.audience,
            content_tokens: meta.content_tokens,
            content_sha256: meta.content_sha256,
            embedding_model: meta.embedding.model,
            embedding_dim: meta.embedding.dim,
            content,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatapointList {
    pub datapoints: Vec<DatapointView>,
    /// Pass as `cursor` for the next page; `null` on the last one.
    pub next_cursor: Option<String>,
}

/// List a project's stored datapoints. Works while the index is loading.
pub async fn list(
    State(services): State<Services>,
    query: Result<Query<ListParams>, QueryRejection>,
) -> Result<axum::Json<DatapointList>, ApiError> {
    let Query(params) = query.map_err(|e| ApiError::Malformed(e.body_text()))?;
    let page = services.datapoints.list(params.into()).await?;
    Ok(axum::Json(DatapointList {
        datapoints: page
            .datapoints
            .into_iter()
            .map(DatapointView::from)
            .collect(),
        next_cursor: page.next_cursor,
    }))
}
