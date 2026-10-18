use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::ingest::ingest_one;
use crate::api::state::AppState;
use crate::api::{ApiError, Json};
use crate::webhook::{MAX_PER_PROJECT, Settings, Webhook, WebhookView, is_webhook_id};

/// Most events one webhook accepts per minute.
pub const EVENTS_PER_MINUTE: u32 = 600;

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
        .map_err(|e| ApiError::Validation(e.body_text()))
}

fn not_found(id: &str) -> ApiError {
    ApiError::NotFound(format!("no webhook {id:?} in this project"))
}

/// The webhook, if it exists and belongs to `project`.
async fn load(state: &AppState, project: Uuid, id: &str) -> Result<Webhook, ApiError> {
    if !is_webhook_id(id) {
        return Err(not_found(id));
    }
    match state.store.get_webhook(id).await? {
        Some(w) if w.project_id == project => Ok(w),
        _ => Err(not_found(id)),
    }
}

/// The in-process lock serialising changes to one project's webhooks.
fn webhooks_lock(project: Uuid) -> String {
    format!("{project}:webhooks")
}

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<WebhookRequest>,
) -> Result<(StatusCode, axum::Json<CreatedWebhook>), ApiError> {
    req.settings.validate().map_err(ApiError::Validation)?;
    let project = req.project_id;
    let _guard = state.locks.lock(webhooks_lock(project)).await;
    if state.store.list_webhooks(project).await?.len() >= MAX_PER_PROJECT {
        return Err(ApiError::Validation(format!(
            "a project holds at most {MAX_PER_PROJECT} webhooks; delete one first"
        )));
    }
    let (webhook, token) = Webhook::create(project, req.settings, Utc::now());
    state.store.put_webhook(&webhook).await?;
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
    State(state): State<AppState>,
    query: Result<Query<ProjectQuery>, QueryRejection>,
) -> Result<axum::Json<WebhookList>, ApiError> {
    let project = project_of(query)?;
    let webhooks = state.store.list_webhooks(project).await?;
    Ok(axum::Json(WebhookList {
        webhooks: webhooks.into_iter().map(Webhook::view).collect(),
    }))
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<ProjectQuery>, QueryRejection>,
) -> Result<axum::Json<WebhookView>, ApiError> {
    let project = project_of(query)?;
    Ok(axum::Json(load(&state, project, &id).await?.view()))
}

/// Replace a webhook's settings. Its id and token stay the same.
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<WebhookRequest>,
) -> Result<axum::Json<WebhookView>, ApiError> {
    req.settings.validate().map_err(ApiError::Validation)?;
    let _guard = state.locks.lock(webhooks_lock(req.project_id)).await;
    let mut webhook = load(&state, req.project_id, &id).await?;
    webhook.settings = req.settings;
    state.store.put_webhook(&webhook).await?;
    Ok(axum::Json(webhook.view()))
}

/// Delete a webhook. Datapoints it created stay.
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<ProjectQuery>, QueryRejection>,
) -> Result<axum::Json<DeletedWebhook>, ApiError> {
    let project = project_of(query)?;
    let _guard = state.locks.lock(webhooks_lock(project)).await;
    let deleted = match load(&state, project, &id).await {
        Ok(_) => {
            state.store.delete_webhook(&id).await?;
            state.webhook_rate.forget(&id);
            true
        }
        Err(ApiError::NotFound(_)) => false,
        Err(e) => return Err(e),
    };
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

/// Turn an event into a datapoint. The token in the URL is the only
/// credential, so a wrong id and a wrong token look the same.
pub async fn receive(
    State(state): State<AppState>,
    Path((id, token)): Path<(String, String)>,
    body: Result<Bytes, BytesRejection>,
) -> Result<(StatusCode, axum::Json<EventResponse>), ApiError> {
    let unknown = || ApiError::NotFound("no such webhook".into());
    if !is_webhook_id(&id) {
        return Err(unknown());
    }
    let webhook = match state.store.get_webhook(&id).await? {
        Some(w) if w.token_matches(&token) => w,
        _ => return Err(unknown()),
    };
    let body = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::WebhookBodyTooLarge
        } else {
            ApiError::Validation(e.body_text())
        }
    })?;
    if webhook.settings.paused {
        return Err(ApiError::WebhookPaused);
    }
    if !state
        .webhook_rate
        .allow(&id, EVENTS_PER_MINUTE, Duration::from_secs(60))
    {
        return Err(ApiError::RateLimited);
    }
    state.ensure_ready()?;

    let event_id = format!("evt_{}", Uuid::now_v7().simple());
    let new = match webhook.map_event(&body) {
        Ok(new) => new,
        Err(reason) => {
            tracing::info!(
                webhook = id,
                event = event_id,
                reason,
                "webhook event dropped"
            );
            return Ok((
                StatusCode::OK,
                axum::Json(EventResponse {
                    event_id,
                    verdict: "dropped",
                    dropped_by: Some(reason),
                    datapoint_id: None,
                    unchanged: false,
                }),
            ));
        }
    };
    // Without an id from the event, an identical event is keyed on what it
    // maps to, so a resend is not stored twice.
    let key = new
        .source_id
        .is_none()
        .then(|| format!("webhook:{id}:{}", new.fingerprint()));
    let (_, resp) = ingest_one(&state, new, key.as_deref()).await?;
    tracing::info!(webhook = id, event = event_id, datapoint = %resp.datapoint.id, "webhook event ingested");
    Ok((
        StatusCode::ACCEPTED,
        axum::Json(EventResponse {
            event_id,
            verdict: "ingested",
            dropped_by: None,
            datapoint_id: Some(resp.datapoint.id),
            unchanged: resp.deduplicated,
        }),
    ))
}
