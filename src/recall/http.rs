use axum::Router;
use axum::extract::State;
use axum::routing::post;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Level, RecallQuery, RecalledItem, Sort};
use crate::app::Services;
use crate::datapoints::Datapoint;
use crate::http::{ApiError, Json};

pub fn routes() -> Router<Services> {
    Router::new().route("/api/v1/recall", post(handler))
}

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

impl From<RecallRequest> for RecallQuery {
    fn from(r: RecallRequest) -> Self {
        RecallQuery {
            project_id: r.project_id,
            query: r.query,
            limit: r.limit,
            level: r.level,
            max_tokens: r.max_tokens,
            query_timestamp: r.query_timestamp,
            occurred_from: r.occurred_from,
            occurred_to: r.occurred_to,
            sort: r.sort,
        }
    }
}

impl From<RecalledItem> for RecallItem {
    fn from(i: RecalledItem) -> Self {
        RecallItem {
            rank: i.rank,
            score: i.score,
            datapoint: i.datapoint,
            content_tokens: i.content_tokens,
        }
    }
}

pub async fn handler(
    State(services): State<Services>,
    Json(req): Json<RecallRequest>,
) -> Result<axum::Json<RecallResponse>, ApiError> {
    let recalled = services.recall.recall(req.into()).await?;
    Ok(axum::Json(RecallResponse {
        results: recalled.results.into_iter().map(RecallItem::from).collect(),
        used_tokens: recalled.used_tokens,
        budget_tokens: recalled.budget_tokens,
    }))
}
