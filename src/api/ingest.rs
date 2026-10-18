use std::collections::HashSet;

use anyhow::Context;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::error::ItemError;
use crate::api::state::AppState;
use crate::api::{ApiError, Json};
use crate::domain::{Datapoint, Meta, NewDatapoint, StoredEmbedding, content_hash};
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
    pub replaced: bool,
}

pub async fn handler(
    State(state): State<AppState>,
    Json(req): Json<IngestRequest>,
) -> Result<(StatusCode, axum::Json<IngestResponse>), ApiError> {
    state.ensure_ready()?;
    if req.idempotency_key.is_some() && req.datapoint.source_id.is_some() {
        return Err(ApiError::Validation(
            "send idempotencyKey or sourceId, not both; resending a sourceId is already safe"
                .into(),
        ));
    }
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

/// The in-process lock serialising writes and deletes of one datapoint.
pub fn datapoint_lock(project: Uuid, id: Uuid) -> String {
    format!("{project}:datapoint:{id}")
}

/// Store one datapoint. With a `sourceId`, the datapoint is stored under
/// it and `key` is ignored. With an idempotency key, a repeat of the same
/// request returns the original datapoint, a half-finished earlier attempt
/// is completed under the id it already reserved, and a different request
/// reusing the key is rejected.
pub async fn ingest_one(
    state: &AppState,
    new: NewDatapoint,
    key: Option<&str>,
) -> Result<(StatusCode, IngestResponse), ApiError> {
    validate_key(key)?;
    new.validate().map_err(ApiError::Validation)?;
    if new.source_id.is_some() {
        return upsert(state, new).await;
    }
    let project = new.project_id;
    let fingerprint = new.fingerprint();
    let mut id = new.id();

    let _guard = match key {
        Some(key) => Some(state.locks.lock(format!("{project}:key:{key}")).await),
        None => None,
    };
    if let Some(key) = key
        && let MarkerOutcome::Exists {
            id: existing_id,
            fingerprint: recorded,
        } = state
            .store
            .create_marker(project, key, id, &fingerprint)
            .await?
    {
        // Markers from before fingerprints existed are trusted as before.
        if recorded.is_some_and(|f| f != fingerprint) {
            return Err(ApiError::IdempotencyKeyReused);
        }
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
                    replaced: false,
                },
            ));
        }
        // An earlier attempt reserved this id but did not finish.
        id = existing_id;
    }

    let datapoint = new
        .into_parts(id, None, Utc::now())
        .map_err(ApiError::Validation)?;
    let datapoint = write(state, datapoint).await?;
    Ok((
        StatusCode::CREATED,
        IngestResponse {
            datapoint,
            deduplicated: false,
            replaced: false,
        },
    ))
}

/// Store a datapoint under its `sourceId`. A resend that changes nothing
/// returns the stored datapoint; one that changes anything replaces it.
async fn upsert(
    state: &AppState,
    new: NewDatapoint,
) -> Result<(StatusCode, IngestResponse), ApiError> {
    let project = new.project_id;
    let id = new.id();
    let _guard = state.locks.lock(datapoint_lock(project, id)).await;
    let existing = state.store.get_meta(project, id).await?;
    let datapoint = new
        .into_parts(id, existing.as_ref(), Utc::now())
        .map_err(ApiError::Validation)?;
    let (status, deduplicated, replaced, datapoint) = match existing {
        Some(meta) if meta.matches(&datapoint, state.embedder.model_name()) => {
            // An earlier send may have stopped before the index upsert.
            state
                .index
                .upsert(project, id, meta.embedding.vector)
                .await?;
            (StatusCode::OK, true, false, datapoint)
        }
        Some(_) => (StatusCode::OK, false, true, write(state, datapoint).await?),
        None => (
            StatusCode::CREATED,
            false,
            false,
            write(state, datapoint).await?,
        ),
    };
    Ok((
        status,
        IngestResponse {
            datapoint,
            deduplicated,
            replaced,
        },
    ))
}

/// Embed, then write content, then metadata (the commit point), then index.
async fn write(state: &AppState, datapoint: Datapoint) -> Result<Datapoint, ApiError> {
    let id = datapoint.id;
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
        source_id: datapoint.source_id.clone(),
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
        content_sha256: Some(content_hash(&datapoint.content)),
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

/// Why each item would fail, checked before anything is written. An item
/// with a `sourceId` and no label needs a stored datapoint, or an earlier
/// item in the batch that creates it, to take its label from.
async fn invalid_items(state: &AppState, items: &[NewDatapoint]) -> anyhow::Result<Vec<ItemError>> {
    let mut details = Vec::new();
    let mut labelled = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        if let Err(message) = item.validate() {
            details.push(ItemError { index, message });
            continue;
        }
        let Some(source) = &item.source_id else {
            continue;
        };
        let id = item.id();
        if item.label.is_some() {
            labelled.insert(id);
        } else if !labelled.contains(&id)
            && state.store.get_meta(item.project_id, id).await?.is_none()
        {
            details.push(ItemError {
                index,
                message: format!(
                    "label is required: no datapoint with sourceId {source:?} is stored"
                ),
            });
        }
    }
    Ok(details)
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
    let details = invalid_items(&state, &req.items).await?;
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
                replaced: resp.replaced,
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
