use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{self, post};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{EventOutcome, MAX_BODY_BYTES, Settings, Webhook, WebhookView};
use crate::app::Services;
use crate::http::{ApiError, Json};

pub fn routes() -> Router<Services> {
    Router::new()
        .route("/api/v1/webhooks", post(create).get(list))
        .route(
            "/api/v1/webhooks/{id}",
            routing::get(get).put(update).delete(delete),
        )
        .route(
            "/api/v1/webhooks/{id}/{token}",
            post(receive).layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
        )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookRequest {
    pub project_id: Uuid,
    #[serde(flatten)]
    pub settings: Settings,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectQuery {
    pub project_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct CreatedWebhook {
    #[serde(flatten)]
    pub webhook: WebhookView,
    /// Where to send events. The token in it is the only credential and is
    /// not shown again.
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct WebhookList {
    pub webhooks: Vec<WebhookView>,
}

#[derive(Debug, Serialize)]
pub struct DeletedWebhook {
    pub id: String,
    pub deleted: bool,
}

fn project_of(query: Result<Query<ProjectQuery>, QueryRejection>) -> Result<Uuid, ApiError> {
    query
        .map(|Query(q)| q.project_id)
        .map_err(|e| ApiError::Malformed(e.body_text()))
}

pub async fn create(
    State(services): State<Services>,
    Json(req): Json<WebhookRequest>,
) -> Result<(StatusCode, axum::Json<CreatedWebhook>), ApiError> {
    let (webhook, token) = services
        .webhooks
        .create(req.project_id, req.settings)
        .await?;
    let path = format!("/api/v1/webhooks/{}/{token}", webhook.id);
    Ok((
        StatusCode::CREATED,
        axum::Json(CreatedWebhook {
            webhook: webhook.view(),
            path,
        }),
    ))
}

pub async fn list(
    State(services): State<Services>,
    query: Result<Query<ProjectQuery>, QueryRejection>,
) -> Result<axum::Json<WebhookList>, ApiError> {
    let project = project_of(query)?;
    let webhooks = services.webhooks.list(project).await?;
    Ok(axum::Json(WebhookList {
        webhooks: webhooks.into_iter().map(Webhook::view).collect(),
    }))
}

pub async fn get(
    State(services): State<Services>,
    Path(id): Path<String>,
    query: Result<Query<ProjectQuery>, QueryRejection>,
) -> Result<axum::Json<WebhookView>, ApiError> {
    let project = project_of(query)?;
    Ok(axum::Json(
        services.webhooks.get(project, &id).await?.view(),
    ))
}

/// Replace a webhook's settings. Its id and token stay the same.
pub async fn update(
    State(services): State<Services>,
    Path(id): Path<String>,
    Json(req): Json<WebhookRequest>,
) -> Result<axum::Json<WebhookView>, ApiError> {
    let webhook = services
        .webhooks
        .update(req.project_id, &id, req.settings)
        .await?;
    Ok(axum::Json(webhook.view()))
}

/// Delete a webhook. Datapoints it created stay.
pub async fn delete(
    State(services): State<Services>,
    Path(id): Path<String>,
    query: Result<Query<ProjectQuery>, QueryRejection>,
) -> Result<axum::Json<DeletedWebhook>, ApiError> {
    let project = project_of(query)?;
    let deleted = services.webhooks.delete(project, &id).await?;
    Ok(axum::Json(DeletedWebhook { id, deleted }))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventResponse {
    pub event_id: String,
    /// `ingested` or `dropped`.
    pub verdict: &'static str,
    /// Why the event was dropped: the first filter it failed, or the field
    /// it lacked.
    pub dropped_by: Option<String>,
    pub datapoint_id: Option<Uuid>,
    /// True when the event changed nothing that was already stored.
    pub unchanged: bool,
}

/// Turn an event into a datapoint. The webhook is checked before the body,
/// so a wrong id or token is a `404` whatever was sent.
pub async fn receive(
    State(services): State<Services>,
    Path((id, token)): Path<(String, String)>,
    body: Result<Bytes, BytesRejection>,
) -> Result<(StatusCode, axum::Json<EventResponse>), ApiError> {
    let webhook = services.webhooks.authenticate(&id, &token).await?;
    let body = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::WebhookBodyTooLarge
        } else {
            ApiError::Malformed(e.body_text())
        }
    })?;
    let event = services.webhooks.receive(&webhook, &body).await?;
    let (status, resp) = match event.outcome {
        EventOutcome::Dropped { reason } => (
            StatusCode::OK,
            EventResponse {
                event_id: event.id,
                verdict: "dropped",
                dropped_by: Some(reason),
                datapoint_id: None,
                unchanged: false,
            },
        ),
        EventOutcome::Ingested {
            datapoint_id,
            unchanged,
        } => (
            StatusCode::ACCEPTED,
            EventResponse {
                event_id: event.id,
                verdict: "ingested",
                dropped_by: None,
                datapoint_id: Some(datapoint_id),
                unchanged,
            },
        ),
    };
    Ok((status, axum::Json(resp)))
}
