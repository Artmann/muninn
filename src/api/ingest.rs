use anyhow::Context;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::error::ItemError;
use crate::api::state::AppState;
use crate::api::{ApiError, Json};
use crate::domain::{Datapoint, Meta, NewDatapoint, StoredEmbedding};
use crate::embed::build_input;
use crate::store::MarkerOutcome;
use crate::tokens::count_tokens;

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
}

pub async fn handler(
    State(state): State<AppState>,
    Json(req): Json<IngestRequest>,
) -> Result<(StatusCode, axum::Json<IngestResponse>), ApiError> {
    state.ensure_ready()?;
    let (status, body) = ingest_one(&state, req.datapoint, req.idempotency_key.as_deref()).await?;
    Ok((status, axum::Json(body)))
}

/// An empty key would silently dedupe every later write that also sends one.
fn validate_key(key: Option<&str>) -> Result<(), ApiError> {
    match key {
        Some(k) if k.trim().is_empty() => Err(ApiError::Validation(
            "idempotencyKey must not be empty; omit it instead".into(),
        )),
        _ => Ok(()),
    }
}

/// Store one datapoint. With an idempotency key, a repeat returns the
/// original datapoint, and a half-finished earlier attempt is completed
/// under the id it already reserved.
pub async fn ingest_one(
    state: &AppState,
    new: NewDatapoint,
    key: Option<&str>,
) -> Result<(StatusCode, IngestResponse), ApiError> {
    validate_key(key)?;
    new.validate().map_err(ApiError::Validation)?;
    let project = new.project_id;
    let mut id = Uuid::now_v7();

    let _guard = match key {
        Some(key) => Some(state.locks.lock(format!("{project}:{key}")).await),
        None => None,
    };
    if let Some(key) = key
        && let MarkerOutcome::Exists(existing_id) =
            state.store.create_marker(project, key, id).await?
    {
        if let Some(meta) = state.store.get_meta(project, existing_id).await? {
            let content = state
                .store
                .get_content(project, existing_id)
                .await?
                .with_context(|| format!("datapoint {existing_id} has metadata but no content"))?;
            // The earlier attempt may have stopped after meta.json but
            // before the index upsert. Upserting again is harmless.
            if meta.embedding.model == state.embedder.model_name() {
                state
                    .index
                    .upsert(project, existing_id, meta.embedding.vector.clone())
                    .await?;
            }
            return Ok((
                StatusCode::OK,
                IngestResponse {
                    datapoint: meta.into_datapoint(content),
                    deduplicated: true,
                },
            ));
        }
        // An earlier attempt reserved this id but did not finish.
        id = existing_id;
    }

    let datapoint = write(state, new, id).await?;
    Ok((
        StatusCode::CREATED,
        IngestResponse {
            datapoint,
            deduplicated: false,
        },
    ))
}

/// Embed, then write content, then metadata (the commit point), then index.
async fn write(state: &AppState, new: NewDatapoint, id: Uuid) -> Result<Datapoint, ApiError> {
    let datapoint = new.into_parts(id, Utc::now());
    let input = build_input(&datapoint.label, &datapoint.content, &datapoint.metadata);
    let embedder = state.embedder.clone();
    let vector = tokio::task::spawn_blocking(move || embedder.embed_passages(&[input]))
        .await
        .context("embedding task panicked")??
        .pop()
        .context("embedder returned no vector")?;

    let meta = Meta {
        id,
        project_id: datapoint.project_id,
        label: datapoint.label.clone(),
        timestamp: datapoint.timestamp,
        metadata: datapoint.metadata.clone(),
        audience: datapoint.audience.clone(),
        embedding: StoredEmbedding {
            model: state.embedder.model_name().to_string(),
            dim: vector.len(),
            vector: vector.clone(),
        },
        content_tokens: count_tokens(&datapoint.content),
    };
    state
        .store
        .put_content(datapoint.project_id, id, &datapoint.content)
        .await?;
    state.store.put_meta(&meta).await?;
    state.index.upsert(datapoint.project_id, id, vector).await?;
    Ok(datapoint)
}

/// Most items accepted in one batch.
pub const MAX_BATCH: usize = 100;

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

pub async fn batch_handler(
    State(state): State<AppState>,
    Json(req): Json<BatchRequest>,
) -> Result<axum::Json<BatchResponse>, ApiError> {
    state.ensure_ready()?;
    validate_key(req.idempotency_key.as_deref())?;
    if req.items.len() > MAX_BATCH {
        return Err(ApiError::BatchTooLarge);
    }
    let details: Vec<ItemError> = req
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            item.validate()
                .err()
                .map(|message| ItemError { index, message })
        })
        .collect();
    if !details.is_empty() {
        return Err(ApiError::ValidationDetailed {
            message: "invalid items".into(),
            details,
        });
    }

    let total = req.items.len();
    let mut results = Vec::with_capacity(total);
    for (i, item) in req.items.into_iter().enumerate() {
        let key = req.idempotency_key.as_ref().map(|k| format!("{k}#{i}"));
        match ingest_one(&state, item, key.as_deref()).await {
            Ok((status, resp)) => results.push(BatchItemResult::Ok {
                status: status.as_u16(),
                deduplicated: resp.deduplicated,
                datapoint: resp.datapoint,
            }),
            Err(e) => {
                if let ApiError::Internal(inner) = &e {
                    tracing::error!(error = ?inner, item = i, "batch item failed");
                }
                results.push(BatchItemResult::Err {
                    status: e.status().as_u16(),
                    error: ItemErrorBody {
                        code: e.code().into(),
                        message: e.to_string(),
                    },
                });
                for _ in i + 1..total {
                    results.push(BatchItemResult::Err {
                        status: 0,
                        error: ItemErrorBody {
                            code: "skipped".into(),
                            message: "not attempted after an earlier item failed".into(),
                        },
                    });
                }
                break;
            }
        }
    }
    Ok(axum::Json(BatchResponse { results }))
}
