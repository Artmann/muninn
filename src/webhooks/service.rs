use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use super::rate::RateLimiter;
use super::{MAX_PER_PROJECT, Settings, Webhook, is_webhook_id};
use crate::app::Infra;
use crate::datapoints::{DatapointService, IngestOutcome};
use crate::error::ServiceError;
use crate::locks::KeyLocks;

/// Most events one webhook accepts per minute.
pub const EVENTS_PER_MINUTE: u32 = 600;

/// What became of an inbound event.
#[derive(Debug)]
pub struct Event {
    pub id: String,
    pub outcome: EventOutcome,
}

#[derive(Debug)]
pub enum EventOutcome {
    /// Not ingested: the first filter the event failed, or the field it
    /// lacked.
    Dropped { reason: String },
    Ingested {
        datapoint_id: Uuid,
        /// True when the event changed nothing that was already stored.
        unchanged: bool,
    },
}

/// Manages a project's webhooks and turns their events into datapoints.
#[derive(Clone)]
pub struct WebhookService {
    infra: Infra,
    locks: KeyLocks,
    /// Events accepted per webhook per minute.
    rate: RateLimiter,
    datapoints: DatapointService,
}

fn not_found(id: &str) -> ServiceError {
    ServiceError::NotFound(format!("no webhook {id:?} in this project"))
}

/// The in-process lock serialising changes to one project's webhooks.
fn webhooks_lock(project: Uuid) -> String {
    format!("{project}:webhooks")
}

impl WebhookService {
    pub(crate) fn new(
        infra: Infra,
        locks: KeyLocks,
        datapoints: DatapointService,
    ) -> WebhookService {
        WebhookService {
            infra,
            locks,
            rate: RateLimiter::default(),
            datapoints,
        }
    }

    /// A new webhook and the token for its URL. The token is the only
    /// credential and cannot be recovered later.
    pub async fn create(
        &self,
        project: Uuid,
        settings: Settings,
    ) -> Result<(Webhook, String), ServiceError> {
        settings.validate().map_err(ServiceError::Validation)?;
        let _guard = self.locks.lock(webhooks_lock(project)).await;
        if self.infra.store.list_webhooks(project).await?.len() >= MAX_PER_PROJECT {
            return Err(ServiceError::Validation(format!(
                "a project holds at most {MAX_PER_PROJECT} webhooks; delete one first"
            )));
        }
        let (webhook, token) = Webhook::create(project, settings, Utc::now());
        self.infra.store.put_webhook(&webhook).await?;
        Ok((webhook, token))
    }

    pub async fn list(&self, project: Uuid) -> Result<Vec<Webhook>, ServiceError> {
        Ok(self.infra.store.list_webhooks(project).await?)
    }

    /// The webhook, if it exists and belongs to `project`.
    pub async fn get(&self, project: Uuid, id: &str) -> Result<Webhook, ServiceError> {
        if !is_webhook_id(id) {
            return Err(not_found(id));
        }
        match self.infra.store.get_webhook(id).await? {
            Some(w) if w.project_id == project => Ok(w),
            _ => Err(not_found(id)),
        }
    }

    /// Replace a webhook's settings. Its id and token stay the same.
    pub async fn update(
        &self,
        project: Uuid,
        id: &str,
        settings: Settings,
    ) -> Result<Webhook, ServiceError> {
        settings.validate().map_err(ServiceError::Validation)?;
        let _guard = self.locks.lock(webhooks_lock(project)).await;
        let mut webhook = self.get(project, id).await?;
        webhook.settings = settings;
        self.infra.store.put_webhook(&webhook).await?;
        Ok(webhook)
    }

    /// Delete a webhook. Datapoints it created stay. False when there was
    /// no such webhook.
    pub async fn delete(&self, project: Uuid, id: &str) -> Result<bool, ServiceError> {
        let _guard = self.locks.lock(webhooks_lock(project)).await;
        match self.get(project, id).await {
            Ok(_) => {
                self.infra.store.delete_webhook(id).await?;
                self.rate.forget(id);
                Ok(true)
            }
            Err(ServiceError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The webhook an event URL names. The token is the only credential, so
    /// a wrong id and a wrong token look the same.
    pub async fn authenticate(&self, id: &str, token: &str) -> Result<Webhook, ServiceError> {
        let unknown = || ServiceError::NotFound("no such webhook".into());
        if !is_webhook_id(id) {
            return Err(unknown());
        }
        match self.infra.store.get_webhook(id).await? {
            Some(w) if w.token_matches(token) => Ok(w),
            _ => Err(unknown()),
        }
    }

    /// Turn an event for an authenticated webhook into a datapoint.
    pub async fn receive(&self, webhook: &Webhook, body: &[u8]) -> Result<Event, ServiceError> {
        let id = &webhook.id;
        if webhook.settings.paused {
            return Err(ServiceError::WebhookPaused);
        }
        if !self
            .rate
            .allow(id, EVENTS_PER_MINUTE, Duration::from_secs(60))
        {
            return Err(ServiceError::RateLimited);
        }
        self.infra.ensure_ready()?;

        let event_id = format!("evt_{}", Uuid::now_v7().simple());
        let new = match webhook.map_event(body) {
            Ok(new) => new,
            Err(reason) => {
                tracing::info!(
                    webhook = id,
                    event = event_id,
                    reason,
                    "webhook event dropped"
                );
                return Ok(Event {
                    id: event_id,
                    outcome: EventOutcome::Dropped { reason },
                });
            }
        };
        // Without an id from the event, an identical event is keyed on what it
        // maps to, so a resend is not stored twice.
        let key = new
            .source_id
            .is_none()
            .then(|| format!("webhook:{id}:{}", new.fingerprint()));
        let ingested = self.datapoints.ingest_one(new, key.as_deref()).await?;
        let datapoint_id = ingested.datapoint.id;
        tracing::info!(webhook = id, event = event_id, datapoint = %datapoint_id, "webhook event ingested");
        Ok(Event {
            id: event_id,
            outcome: EventOutcome::Ingested {
                datapoint_id,
                unchanged: ingested.outcome == IngestOutcome::Deduplicated,
            },
        })
    }
}
