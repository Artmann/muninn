use std::collections::HashMap;

use anyhow::Context;
use axum::extract::State;
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::state::AppState;
use crate::api::{ApiError, Json};
use crate::domain::{Datapoint, Meta};
use crate::recall::{Candidate, Level, SelectParams, Sort, select};

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 200;
/// Most hits fetched from the index before time filtering.
const MAX_SEARCH_K: usize = 1000;
const FETCH_CONCURRENCY: usize = 16;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallRequest {
    pub project_id: Uuid,
    pub query: String,
    pub limit: Option<usize>,
    pub level: Option<Level>,
    pub max_tokens: Option<usize>,
    pub query_timestamp: Option<DateTime<Utc>>,
    pub occurred_from: Option<DateTime<Utc>>,
    pub occurred_to: Option<DateTime<Utc>>,
    pub sort: Option<Sort>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallItem {
    pub rank: usize,
    pub score: f32,
    #[serde(flatten)]
    pub datapoint: Datapoint,
    pub content_tokens: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallResponse {
    pub results: Vec<RecallItem>,
    pub used_tokens: usize,
    pub budget_tokens: usize,
}

/// The token budget: `level` or `maxTokens`, never both; `low` by default.
pub fn resolve_budget(level: Option<Level>, max_tokens: Option<usize>) -> Result<usize, ApiError> {
    match (level, max_tokens) {
        (Some(_), Some(_)) => Err(ApiError::BudgetConflict),
        (_, Some(0)) => Err(ApiError::Validation("maxTokens must be at least 1".into())),
        (_, Some(n)) => Ok(n),
        (level, None) => Ok(level.unwrap_or_default().tokens()),
    }
}

pub fn validate(req: &RecallRequest) -> Result<(), ApiError> {
    if req.query.trim().is_empty() {
        return Err(ApiError::Validation("query must not be empty".into()));
    }
    if let Some(limit) = req.limit
        && !(1..=MAX_LIMIT).contains(&limit)
    {
        return Err(ApiError::Validation(format!(
            "limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    if let (Some(from), Some(to)) = (req.occurred_from, req.occurred_to)
        && from > to
    {
        return Err(ApiError::RecallRangeInvalid);
    }
    Ok(())
}

pub async fn handler(
    State(state): State<AppState>,
    Json(req): Json<RecallRequest>,
) -> Result<axum::Json<RecallResponse>, ApiError> {
    state.ensure_ready()?;
    validate(&req)?;
    let budget_tokens = resolve_budget(req.level, req.max_tokens)?;
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT);
    let project = req.project_id;

    let embedder = state.embedder.clone();
    let query = req.query;
    let qvec = tokio::task::spawn_blocking(move || embedder.embed_query(&query))
        .await
        .context("embedding task panicked")??;

    let hits = state
        .index
        .search(project, &qvec, (limit * 4).min(MAX_SEARCH_K))
        .await?;

    let store = &state.store;
    let metas: Vec<(f32, Option<Meta>)> = futures::stream::iter(hits)
        .map(|(id, score)| async move { store.get_meta(project, id).await.map(|m| (score, m)) })
        .buffer_unordered(FETCH_CONCURRENCY)
        .try_collect()
        .await?;
    let mut by_id: HashMap<Uuid, Meta> = HashMap::new();
    let mut candidates = Vec::with_capacity(metas.len());
    for (score, meta) in metas {
        let Some(meta) = meta else { continue };
        candidates.push(Candidate {
            id: meta.id,
            score,
            timestamp: meta.timestamp,
            content_tokens: meta.content_tokens,
        });
        by_id.insert(meta.id, meta);
    }

    let selected = select(
        candidates,
        &SelectParams {
            limit,
            budget_tokens,
            query_timestamp: req.query_timestamp.unwrap_or_else(Utc::now),
            occurred_from: req.occurred_from,
            occurred_to: req.occurred_to,
            sort: req.sort.unwrap_or_default(),
        },
    );

    let contents: HashMap<Uuid, Option<String>> =
        futures::stream::iter(selected.items.iter().map(|s| s.id).collect::<Vec<_>>())
            .map(|id| async move { store.get_content(project, id).await.map(|c| (id, c)) })
            .buffer_unordered(FETCH_CONCURRENCY)
            .try_collect()
            .await?;

    let mut results = Vec::with_capacity(selected.items.len());
    for s in selected.items {
        let meta = by_id
            .remove(&s.id)
            .context("selected id without metadata")?;
        let content = contents
            .get(&s.id)
            .cloned()
            .flatten()
            .with_context(|| format!("datapoint {} has metadata but no content", s.id))?;
        let content_tokens = meta.content_tokens;
        results.push(RecallItem {
            rank: s.rank,
            score: s.score,
            datapoint: meta.into_datapoint(content),
            content_tokens,
        });
    }

    Ok(axum::Json(RecallResponse {
        results,
        used_tokens: selected.used_tokens,
        budget_tokens,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> RecallRequest {
        serde_json::from_value(serde_json::json!({
            "projectId": "019a0000-0000-7000-8000-000000000000",
            "query": "q"
        }))
        .unwrap()
    }

    #[test]
    fn default_budget_is_8000() {
        assert_eq!(resolve_budget(None, None).unwrap(), 8000);
        assert_eq!(resolve_budget(Some(Level::High), None).unwrap(), 24000);
        assert_eq!(resolve_budget(None, Some(50)).unwrap(), 50);
    }

    #[test]
    fn both_is_conflict() {
        assert!(matches!(
            resolve_budget(Some(Level::Low), Some(5)),
            Err(ApiError::BudgetConflict)
        ));
    }

    #[test]
    fn zero_max_tokens_rejected() {
        assert!(matches!(
            resolve_budget(None, Some(0)),
            Err(ApiError::Validation(_))
        ));
    }

    #[test]
    fn range_invalid() {
        let mut r = req();
        r.occurred_from = Some("2026-10-02T00:00:00Z".parse().unwrap());
        r.occurred_to = Some("2026-10-01T00:00:00Z".parse().unwrap());
        assert!(matches!(validate(&r), Err(ApiError::RecallRangeInvalid)));
        r.occurred_to = r.occurred_from;
        assert!(validate(&r).is_ok());
    }

    #[test]
    fn limit_bounds() {
        for (limit, ok) in [(0, false), (201, false), (1, true), (200, true)] {
            let mut r = req();
            r.limit = Some(limit);
            assert_eq!(validate(&r).is_ok(), ok, "limit {limit}");
        }
    }

    #[test]
    fn blank_query_rejected() {
        let mut r = req();
        r.query = "  \n".into();
        assert!(matches!(validate(&r), Err(ApiError::Validation(_))));
    }
}
