use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::state::AppState;
use crate::domain::{Meta, PROJECT_AUDIENCE, source_uuid, validate_audience};

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;
/// Most datapoints read per request. With narrow filters a page can come
/// back short, with a cursor to carry on from.
pub const MAX_SCAN: usize = 1000;
/// Datapoints read per round when filtering.
const CHUNK: usize = 100;
const FETCH_CONCURRENCY: usize = 16;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
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

impl DatapointView {
    fn new(meta: Meta, content: Option<String>) -> DatapointView {
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

/// Which stored datapoints to list.
#[derive(Debug, Default)]
struct Filter {
    /// `Some(None)` is the whole-project audience.
    audience: Option<Option<String>>,
    /// Lowercased.
    label: Option<String>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

impl Filter {
    fn from_query(q: &ListQuery) -> Result<Filter, ApiError> {
        if let Some(audience) = &q.audience {
            validate_audience(audience).map_err(ApiError::Validation)?;
        }
        if q.label.as_ref().is_some_and(|l| l.trim().is_empty()) {
            return Err(ApiError::Validation(
                "label must not be empty; omit it instead".into(),
            ));
        }
        if let (Some(from), Some(to)) = (q.occurred_from, q.occurred_to)
            && from > to
        {
            return Err(ApiError::RecallRangeInvalid);
        }
        Ok(Filter {
            audience: q
                .audience
                .clone()
                .map(|a| (a != PROJECT_AUDIENCE).then_some(a)),
            label: q.label.as_ref().map(|l| l.to_lowercase()),
            from: q.occurred_from,
            to: q.occurred_to,
        })
    }

    fn is_empty(&self) -> bool {
        self.audience.is_none() && self.label.is_none() && self.from.is_none() && self.to.is_none()
    }

    fn matches(&self, meta: &Meta) -> bool {
        self.audience.as_ref().is_none_or(|a| *a == meta.audience)
            && self
                .label
                .as_ref()
                .is_none_or(|l| meta.label.to_lowercase().contains(l))
            && self.from.is_none_or(|from| meta.timestamp >= from)
            && self.to.is_none_or(|to| meta.timestamp <= to)
    }
}

/// List a project's stored datapoints, read straight from storage, in id
/// order. Meant for debugging: it works while the index is still loading.
pub async fn list(
    State(state): State<AppState>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<axum::Json<DatapointList>, ApiError> {
    let Query(q) = query.map_err(|e| ApiError::Validation(e.body_text()))?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::Validation(format!(
            "limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    if q.source_id.as_ref().is_some_and(|s| s.trim().is_empty()) {
        return Err(ApiError::Validation(
            "sourceId must not be empty; omit it instead".into(),
        ));
    }
    let cursor = q
        .cursor
        .as_deref()
        .map(|c| {
            c.parse::<Uuid>()
                .map_err(|_| ApiError::Validation(format!("cursor {c:?} is not valid")))
        })
        .transpose()?;
    let filter = Filter::from_query(&q)?;
    let project = q.project_id;

    let (metas, next) = match &q.source_id {
        Some(source_id) => {
            let meta = state
                .store
                .get_meta(project, source_uuid(project, source_id))
                .await?;
            (
                meta.into_iter().filter(|m| filter.matches(m)).collect(),
                None,
            )
        }
        None => scan(&state, project, cursor, limit, &filter).await?,
    };

    let include_content = q.include_content;
    let datapoints = futures::stream::iter(metas)
        .map(|meta| {
            let state = &state;
            async move {
                let content = if include_content {
                    state.store.get_content(project, meta.id).await?
                } else {
                    None
                };
                anyhow::Ok(DatapointView::new(meta, content))
            }
        })
        .buffered(FETCH_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(axum::Json(DatapointList {
        datapoints,
        next_cursor: next.map(|id| id.to_string()),
    }))
}

/// Up to `limit` matching datapoints after `after`, reading at most
/// [`MAX_SCAN`], and the id to continue after when there may be more.
async fn scan(
    state: &AppState,
    project: Uuid,
    mut after: Option<Uuid>,
    limit: usize,
    filter: &Filter,
) -> anyhow::Result<(Vec<Meta>, Option<Uuid>)> {
    let mut found = Vec::new();
    let mut scanned = 0;
    loop {
        // Without a filter every datapoint read is returned, so read no more
        // than the page needs.
        let want = if filter.is_empty() {
            limit - found.len()
        } else {
            CHUNK
        };
        let chunk = want.min(MAX_SCAN - scanned);
        let ids = state
            .store
            .list_datapoint_ids(project, after, chunk)
            .await?;
        let exhausted = ids.len() < chunk;
        let metas: Vec<Option<Meta>> = futures::stream::iter(ids.iter().copied())
            .map(|id| state.store.get_meta(project, id))
            .buffered(FETCH_CONCURRENCY)
            .try_collect()
            .await?;
        let last = ids.last().copied();
        for (id, meta) in ids.into_iter().zip(metas) {
            after = Some(id);
            scanned += 1;
            // Gone since listing, or filtered out.
            let Some(meta) = meta.filter(|m| filter.matches(m)) else {
                continue;
            };
            found.push(meta);
            if found.len() == limit {
                let more = !(exhausted && Some(id) == last);
                return Ok((found, more.then_some(id)));
            }
        }
        if exhausted {
            return Ok((found, None));
        }
        if scanned >= MAX_SCAN {
            return Ok((found, after));
        }
    }
}
