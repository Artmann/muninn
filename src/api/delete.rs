use axum::extract::State;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::ingest::datapoint_lock;
use crate::api::state::AppState;
use crate::api::{ApiError, Json};
use crate::domain::source_uuid;

/// Most ids accepted in one delete call.
pub const MAX_DELETE: usize = 1000;

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

/// Delete datapoints by `sourceId` or server `id`. Results come back in
/// request order, `sourceIds` first.
pub async fn handler(
    State(state): State<AppState>,
    Json(req): Json<DeleteRequest>,
) -> Result<axum::Json<DeleteResponse>, ApiError> {
    state.ensure_ready()?;
    let total = req.source_ids.len() + req.ids.len();
    if !(1..=MAX_DELETE).contains(&total) {
        return Err(ApiError::Validation(format!(
            "send between 1 and {MAX_DELETE} sourceIds and ids in total, got {total}"
        )));
    }
    if req.source_ids.iter().any(|s| s.trim().is_empty()) {
        return Err(ApiError::Validation("sourceIds must not be empty".into()));
    }
    let project = req.project_id;
    let targets = req
        .source_ids
        .into_iter()
        .map(|s| (source_uuid(project, &s), Some(s)))
        .chain(req.ids.into_iter().map(|id| (id, None)));
    let results = futures::stream::iter(targets)
        .map(|(id, source_id)| {
            let state = &state;
            async move {
                let deleted = delete_one(state, project, id).await?;
                anyhow::Ok(DeleteResult {
                    id,
                    source_id,
                    deleted,
                })
            }
        })
        .buffered(16)
        .try_collect()
        .await?;
    Ok(axum::Json(DeleteResponse { results }))
}

/// Remove from the index first, so a crash part way leaves a stored
/// datapoint that the next start puts back in the index, never a vector
/// pointing at nothing.
async fn delete_one(state: &AppState, project: Uuid, id: Uuid) -> anyhow::Result<bool> {
    let _guard = state.locks.lock(datapoint_lock(project, id)).await;
    if state.store.get_meta(project, id).await?.is_none() {
        return Ok(false);
    }
    state.index.remove(project, id).await?;
    state.store.delete_datapoint(project, id).await?;
    Ok(true)
}
