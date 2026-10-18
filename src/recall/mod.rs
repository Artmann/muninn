//! Recall: the stored datapoints most relevant to a query, within a token
//! budget.

pub mod http;

mod select;

pub use select::{Candidate, Level, SelectParams, Selected, Selection, Sort, select};

use std::collections::HashMap;

use anyhow::Context;
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use uuid::Uuid;

use crate::app::Infra;
use crate::datapoints::{Datapoint, Meta};
use crate::error::ServiceError;

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 200;
/// Most hits fetched from the index before time filtering.
const MAX_SEARCH_K: usize = 1000;
const FETCH_CONCURRENCY: usize = 16;

/// What to recall from a project.
#[derive(Debug)]
pub struct RecallQuery {
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

#[derive(Debug)]
pub struct RecalledItem {
    pub rank: usize,
    pub score: f32,
    pub datapoint: Datapoint,
    pub content_tokens: usize,
}

#[derive(Debug)]
pub struct RecallResult {
    pub results: Vec<RecalledItem>,
    pub used_tokens: usize,
    pub budget_tokens: usize,
}

/// The token budget: `level` or `maxTokens`, never both; `low` by default.
pub fn resolve_budget(
    level: Option<Level>,
    max_tokens: Option<usize>,
) -> Result<usize, ServiceError> {
    match (level, max_tokens) {
        (Some(_), Some(_)) => Err(ServiceError::BudgetConflict),
        (_, Some(0)) => Err(ServiceError::Validation(
            "maxTokens must be at least 1".into(),
        )),
        (_, Some(n)) => Ok(n),
        (level, None) => Ok(level.unwrap_or_default().tokens()),
    }
}

pub fn validate(q: &RecallQuery) -> Result<(), ServiceError> {
    if q.query.trim().is_empty() {
        return Err(ServiceError::Validation("query must not be empty".into()));
    }
    if let Some(limit) = q.limit
        && !(1..=MAX_LIMIT).contains(&limit)
    {
        return Err(ServiceError::Validation(format!(
            "limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    if let (Some(from), Some(to)) = (q.occurred_from, q.occurred_to)
        && from > to
    {
        return Err(ServiceError::RecallRangeInvalid);
    }
    Ok(())
}

/// Finds the stored datapoints most relevant to a query, within a token
/// budget.
#[derive(Clone)]
pub struct RecallService {
    infra: Infra,
}

impl RecallService {
    pub(crate) fn new(infra: Infra) -> RecallService {
        RecallService { infra }
    }

    pub async fn recall(&self, q: RecallQuery) -> Result<RecallResult, ServiceError> {
        self.infra.ensure_ready()?;
        validate(&q)?;
        let budget_tokens = resolve_budget(q.level, q.max_tokens)?;
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
        let project = q.project_id;

        let embedder = self.infra.embedder.clone();
        let query = q.query;
        let qvec = tokio::task::spawn_blocking(move || embedder.embed_query(&query))
            .await
            .context("embedding task panicked")??;

        let hits = self
            .infra
            .index
            .search(project, &qvec, (limit * 4).min(MAX_SEARCH_K))
            .await?;

        let store = &self.infra.store;
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
                query_timestamp: q.query_timestamp.unwrap_or_else(Utc::now),
                occurred_from: q.occurred_from,
                occurred_to: q.occurred_to,
                sort: q.sort.unwrap_or_default(),
            },
        );

        let contents: HashMap<Uuid, Option<String>> =
            futures::stream::iter(selected.items.iter().map(|s| s.id).collect::<Vec<_>>())
                .map(|id| async move { store.get_content(project, id).await.map(|c| (id, c)) })
                .buffer_unordered(FETCH_CONCURRENCY)
                .try_collect()
                .await?;

        let mut used_tokens = selected.used_tokens;
        let mut results = Vec::with_capacity(selected.items.len());
        for s in selected.items {
            let meta = by_id
                .remove(&s.id)
                .context("selected id without metadata")?;
            let content_tokens = meta.content_tokens;
            let Some(content) = contents.get(&s.id).cloned().flatten() else {
                // Only reachable if the content object was removed outside the
                // service. Skip it rather than fail every query that finds it.
                tracing::warn!(project = %project, id = %s.id, "metadata without content; skipping");
                used_tokens -= content_tokens;
                continue;
            };
            results.push(RecalledItem {
                rank: s.rank,
                score: s.score,
                datapoint: meta.into_datapoint(content),
                content_tokens,
            });
        }

        Ok(RecallResult {
            results,
            used_tokens,
            budget_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> RecallQuery {
        RecallQuery {
            project_id: Uuid::now_v7(),
            query: "q".into(),
            limit: None,
            level: None,
            max_tokens: None,
            query_timestamp: None,
            occurred_from: None,
            occurred_to: None,
            sort: None,
        }
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
            Err(ServiceError::BudgetConflict)
        ));
    }

    #[test]
    fn zero_max_tokens_rejected() {
        assert!(matches!(
            resolve_budget(None, Some(0)),
            Err(ServiceError::Validation(_))
        ));
    }

    #[test]
    fn range_invalid() {
        let mut q = query();
        q.occurred_from = Some("2026-10-02T00:00:00Z".parse().unwrap());
        q.occurred_to = Some("2026-10-01T00:00:00Z".parse().unwrap());
        assert!(matches!(
            validate(&q),
            Err(ServiceError::RecallRangeInvalid)
        ));
        q.occurred_to = q.occurred_from;
        assert!(validate(&q).is_ok());
    }

    #[test]
    fn limit_bounds() {
        for (limit, ok) in [(0, false), (201, false), (1, true), (200, true)] {
            let mut q = query();
            q.limit = Some(limit);
            assert_eq!(validate(&q).is_ok(), ok, "limit {limit}");
        }
    }

    #[test]
    fn blank_query_rejected() {
        let mut q = query();
        q.query = "  \n".into();
        assert!(matches!(validate(&q), Err(ServiceError::Validation(_))));
    }
}
