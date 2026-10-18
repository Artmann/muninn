use std::collections::HashSet;

use anyhow::Context;
use chrono::Utc;

use super::{DatapointService, datapoint_lock};
use crate::datapoints::{Datapoint, Meta, NewDatapoint, StoredEmbedding, content_hash};
use crate::embed::build_input;
use crate::error::{ItemError, ServiceError};
use crate::store::MarkerOutcome;
use crate::tokens::count_tokens;

/// Most items accepted in one batch.
pub const MAX_BATCH: usize = 100;

/// What an ingest did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestOutcome {
    /// A new datapoint was stored.
    Created,
    /// The datapoint was already stored as sent; nothing changed.
    Deduplicated,
    /// The stored datapoint with the same `sourceId` was replaced.
    Replaced,
}

#[derive(Debug)]
pub struct Ingested {
    pub datapoint: Datapoint,
    pub outcome: IngestOutcome,
}

/// How one item of a batch went.
#[derive(Debug)]
pub enum BatchItem {
    Ingested(Ingested),
    Failed(ServiceError),
    /// Not attempted, because an earlier item failed.
    Skipped,
}

/// An empty key would silently dedupe every later write that also sends one.
fn validate_key(key: Option<&str>) -> Result<(), ServiceError> {
    match key {
        Some(k) if k.trim().is_empty() => Err(ServiceError::Validation(
            "idempotencyKey must not be empty; omit it instead".into(),
        )),
        _ => Ok(()),
    }
}

impl DatapointService {
    /// Store one datapoint, made safe to retry by its `sourceId` or by an
    /// idempotency `key`, not both.
    pub async fn ingest(
        &self,
        new: NewDatapoint,
        key: Option<&str>,
    ) -> Result<Ingested, ServiceError> {
        self.infra.ensure_ready()?;
        if key.is_some() && new.source_id.is_some() {
            return Err(ServiceError::Validation(
                "send idempotencyKey or sourceId, not both; resending a sourceId is already safe"
                    .into(),
            ));
        }
        self.ingest_one(new, key).await
    }

    /// Store up to [`MAX_BATCH`] datapoints in order, stopping at the first
    /// failure. Every item is validated before anything is written. Item
    /// `i` uses the idempotency key `"{key}#{i}"`.
    pub async fn ingest_batch(
        &self,
        items: Vec<NewDatapoint>,
        key: Option<&str>,
    ) -> Result<Vec<BatchItem>, ServiceError> {
        self.infra.ensure_ready()?;
        validate_key(key)?;
        if items.len() > MAX_BATCH {
            return Err(ServiceError::BatchTooLarge);
        }
        let details = self.invalid_items(&items).await?;
        if !details.is_empty() {
            return Err(ServiceError::ValidationDetailed {
                message: "invalid items".into(),
                details,
            });
        }

        let total = items.len();
        let mut results = Vec::with_capacity(total);
        for (i, item) in items.into_iter().enumerate() {
            let item_key = key.map(|k| format!("{k}#{i}"));
            match self.ingest_one(item, item_key.as_deref()).await {
                Ok(ingested) => results.push(BatchItem::Ingested(ingested)),
                Err(e) => {
                    results.push(BatchItem::Failed(e));
                    results.extend((i + 1..total).map(|_| BatchItem::Skipped));
                    break;
                }
            }
        }
        Ok(results)
    }

    /// Why each item would fail, checked before anything is written. An item
    /// with a `sourceId` and no label needs a stored datapoint, or an earlier
    /// item in the batch that creates it, to take its label from.
    async fn invalid_items(&self, items: &[NewDatapoint]) -> anyhow::Result<Vec<ItemError>> {
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
                && self
                    .infra
                    .store
                    .get_meta(item.project_id, id)
                    .await?
                    .is_none()
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

    /// Store one datapoint. With a `sourceId`, the datapoint is stored under
    /// it and `key` is ignored. With an idempotency key, a repeat of the same
    /// request returns the original datapoint, a half-finished earlier attempt
    /// is completed under the id it already reserved, and a different request
    /// reusing the key is rejected.
    pub(crate) async fn ingest_one(
        &self,
        new: NewDatapoint,
        key: Option<&str>,
    ) -> Result<Ingested, ServiceError> {
        validate_key(key)?;
        new.validate().map_err(ServiceError::Validation)?;
        if new.source_id.is_some() {
            return self.upsert(new).await;
        }
        let infra = &self.infra;
        let project = new.project_id;
        let fingerprint = new.fingerprint();
        let mut id = new.id();

        let _guard = match key {
            Some(key) => Some(self.locks.lock(format!("{project}:key:{key}")).await),
            None => None,
        };
        if let Some(key) = key
            && let MarkerOutcome::Exists {
                id: existing_id,
                fingerprint: recorded,
            } = infra
                .store
                .create_marker(project, key, id, &fingerprint)
                .await?
        {
            // Markers from before fingerprints existed are trusted as before.
            if recorded.is_some_and(|f| f != fingerprint) {
                return Err(ServiceError::IdempotencyKeyReused);
            }
            if let Some(meta) = infra.store.get_meta(project, existing_id).await? {
                let content = infra
                    .store
                    .get_content(project, existing_id)
                    .await?
                    .with_context(|| {
                        format!("datapoint {existing_id} has metadata but no content")
                    })?;
                // The earlier attempt may have stopped after meta.json but
                // before the index upsert. Upserting again is harmless.
                if meta.embedding.model == infra.embedder.model_name() {
                    infra
                        .index
                        .upsert(project, existing_id, meta.embedding.vector.clone())
                        .await?;
                }
                return Ok(Ingested {
                    datapoint: meta.into_datapoint(content),
                    outcome: IngestOutcome::Deduplicated,
                });
            }
            // An earlier attempt reserved this id but did not finish.
            id = existing_id;
        }

        let datapoint = new
            .into_parts(id, None, Utc::now())
            .map_err(ServiceError::Validation)?;
        Ok(Ingested {
            datapoint: self.write(datapoint).await?,
            outcome: IngestOutcome::Created,
        })
    }

    /// Store a datapoint under its `sourceId`. A resend that changes nothing
    /// returns the stored datapoint; one that changes anything replaces it.
    async fn upsert(&self, new: NewDatapoint) -> Result<Ingested, ServiceError> {
        let project = new.project_id;
        let id = new.id();
        let _guard = self.locks.lock(datapoint_lock(project, id)).await;
        let existing = self.infra.store.get_meta(project, id).await?;
        let datapoint = new
            .into_parts(id, existing.as_ref(), Utc::now())
            .map_err(ServiceError::Validation)?;
        let (outcome, datapoint) = match existing {
            Some(meta) if meta.matches(&datapoint, self.infra.embedder.model_name()) => {
                // An earlier send may have stopped before the index upsert.
                self.infra
                    .index
                    .upsert(project, id, meta.embedding.vector)
                    .await?;
                (IngestOutcome::Deduplicated, datapoint)
            }
            Some(_) => (IngestOutcome::Replaced, self.write(datapoint).await?),
            None => (IngestOutcome::Created, self.write(datapoint).await?),
        };
        Ok(Ingested { datapoint, outcome })
    }

    /// Embed, then write content, then metadata (the commit point), then index.
    async fn write(&self, datapoint: Datapoint) -> anyhow::Result<Datapoint> {
        let infra = &self.infra;
        let id = datapoint.id;
        let input = build_input(&datapoint.label, &datapoint.content, &datapoint.metadata);
        let embedder = infra.embedder.clone();
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
                model: infra.embedder.model_name().to_string(),
                dim: vector.len(),
                vector: vector.clone(),
            },
            content_tokens: count_tokens(&datapoint.content),
            content_sha256: Some(content_hash(&datapoint.content)),
        };
        infra
            .store
            .put_content(datapoint.project_id, id, &datapoint.content)
            .await?;
        infra.store.put_meta(&meta).await?;
        infra.index.upsert(datapoint.project_id, id, vector).await?;
        Ok(datapoint)
    }
}
