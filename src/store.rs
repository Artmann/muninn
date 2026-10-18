use std::time::Duration;

use crate::aws_error::{explain, s3_permissions};
use anyhow::Context;
use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::Config;
use crate::domain::Meta;
use crate::webhook::Webhook;

/// Object key layout. See ARCHITECTURE.md.
pub mod keys {
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    pub fn content(project: Uuid, id: Uuid) -> String {
        format!("projects/{project}/datapoints/{id}/content")
    }

    pub fn meta(project: Uuid, id: Uuid) -> String {
        format!("projects/{project}/datapoints/{id}/meta.json")
    }

    /// The marker for an idempotency key. The key is hashed so any string is
    /// a safe object name.
    pub fn marker(project: Uuid, key: &str) -> String {
        let digest = Sha256::digest(key.as_bytes());
        format!("projects/{project}/idempotency/{}", hex::encode(digest))
    }

    /// A webhook's settings. Webhooks sit outside `projects/` because an
    /// event's URL names the webhook but not its project.
    pub fn webhook(id: &str) -> String {
        format!("webhooks/{id}.json")
    }

    /// `(project, id)` from a `meta.json` key, or `None` for any other key.
    pub fn parse_meta_key(key: &str) -> Option<(Uuid, Uuid)> {
        let rest = key.strip_prefix("projects/")?;
        let (project, rest) = rest.split_once("/datapoints/")?;
        let id = rest.strip_suffix("/meta.json")?;
        Some((project.parse().ok()?, id.parse().ok()?))
    }
}

pub enum MarkerOutcome {
    Created,
    /// Another request holds the key. `fingerprint` is `None` for markers
    /// written before fingerprints were recorded.
    Exists {
        id: Uuid,
        fingerprint: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
struct MarkerBody {
    id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
}

#[derive(Clone)]
pub struct S3Store {
    client: Client,
    bucket: String,
}

/// Without a region the SDK fails at the first request with a vague
/// "dispatch failure"; say what to set instead.
fn require_region(region: Option<&aws_config::Region>) -> anyhow::Result<()> {
    if region.is_none() {
        anyhow::bail!(
            "no AWS region configured: set AWS_REGION, or copy .env.example to .env for LocalStack"
        );
    }
    Ok(())
}

/// The HTTP status of a service error, if the request got that far.
fn status_of<E>(err: &SdkError<E, HttpResponse>) -> Option<u16> {
    match err {
        SdkError::ServiceError(se) => Some(se.raw().status().as_u16()),
        _ => None,
    }
}

impl S3Store {
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn new(client: Client, bucket: String) -> S3Store {
        S3Store { client, bucket }
    }

    /// Shared AWS SDK config, failing clearly when no region is set.
    pub async fn load_sdk() -> anyhow::Result<aws_config::SdkConfig> {
        let sdk = aws_config::load_defaults(BehaviorVersion::latest()).await;
        require_region(sdk.region())?;
        Ok(sdk)
    }

    pub async fn from_config(cfg: &Config) -> anyhow::Result<S3Store> {
        Ok(S3Store::from_sdk(&S3Store::load_sdk().await?, cfg))
    }

    pub fn from_sdk(sdk: &aws_config::SdkConfig, cfg: &Config) -> S3Store {
        let mut builder = aws_sdk_s3::config::Builder::from(sdk);
        if let Some(url) = &cfg.endpoint_url {
            builder = builder.endpoint_url(url).force_path_style(true);
        }
        S3Store::new(Client::from_conf(builder.build()), cfg.bucket.clone())
    }

    pub async fn ensure_bucket(&self) -> anyhow::Result<()> {
        if self
            .client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .is_ok()
        {
            return Ok(());
        }
        match self
            .client
            .create_bucket()
            .bucket(&self.bucket)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if status_of(&e) == Some(403) => {
                tracing::warn!(bucket = %self.bucket, "no permission to create bucket; assuming it exists");
                Ok(())
            }
            Err(e)
                if e.as_service_error().is_some_and(|se| {
                    se.is_bucket_already_owned_by_you() || se.is_bucket_already_exists()
                }) =>
            {
                Ok(())
            }
            Err(e) => Err(e).with_context(|| format!("creating bucket {}", self.bucket)),
        }
    }

    async fn put(&self, key: String, body: Vec<u8>, content_type: &str) -> anyhow::Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .content_type(content_type)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|e| explain(e, format!("putting {key}"), &s3_permissions(&self.bucket)))?;
        Ok(())
    }

    async fn delete(&self, key: String) -> anyhow::Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| explain(e, format!("deleting {key}"), &s3_permissions(&self.bucket)))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => {
                let bytes = out
                    .body
                    .collect()
                    .await
                    .with_context(|| format!("reading {key}"))?;
                Ok(Some(bytes.to_vec()))
            }
            Err(e) if e.as_service_error().is_some_and(|se| se.is_no_such_key()) => Ok(None),
            Err(e) => Err(explain(
                e,
                format!("getting {key}"),
                &s3_permissions(&self.bucket),
            )),
        }
    }

    pub async fn put_content(&self, project: Uuid, id: Uuid, content: &str) -> anyhow::Result<()> {
        self.put(
            keys::content(project, id),
            content.as_bytes().to_vec(),
            "text/plain; charset=utf-8",
        )
        .await
    }

    pub async fn get_content(&self, project: Uuid, id: Uuid) -> anyhow::Result<Option<String>> {
        match self.get(&keys::content(project, id)).await? {
            Some(bytes) => Ok(Some(
                String::from_utf8(bytes).context("content is not valid UTF-8")?,
            )),
            None => Ok(None),
        }
    }

    pub async fn put_meta(&self, meta: &Meta) -> anyhow::Result<()> {
        self.put(
            keys::meta(meta.project_id, meta.id),
            serde_json::to_vec(meta)?,
            "application/json",
        )
        .await
    }

    pub async fn get_meta(&self, project: Uuid, id: Uuid) -> anyhow::Result<Option<Meta>> {
        let key = keys::meta(project, id);
        match self.get(&key).await? {
            Some(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).with_context(|| format!("parsing {key}"))?,
            )),
            None => Ok(None),
        }
    }

    /// Delete a datapoint's `meta.json`, which makes it gone, then its
    /// content. Deleting a missing datapoint is fine.
    pub async fn delete_datapoint(&self, project: Uuid, id: Uuid) -> anyhow::Result<()> {
        self.delete(keys::meta(project, id)).await?;
        self.delete(keys::content(project, id)).await
    }

    /// Write the idempotency marker only if it does not exist yet. If another
    /// request got there first, return the id and fingerprint it recorded.
    pub async fn create_marker(
        &self,
        project: Uuid,
        key: &str,
        id: Uuid,
        fingerprint: &str,
    ) -> anyhow::Result<MarkerOutcome> {
        let object_key = keys::marker(project, key);
        let body = serde_json::to_vec(&MarkerBody {
            id,
            fingerprint: Some(fingerprint.to_string()),
        })?;
        let result = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&object_key)
            .content_type("application/json")
            .if_none_match("*")
            .body(ByteStream::from(body))
            .send()
            .await;
        match result {
            Ok(_) => Ok(MarkerOutcome::Created),
            Err(e) if matches!(status_of(&e), Some(412) | Some(409)) => {
                for _ in 0..3 {
                    if let Some(bytes) = self.get(&object_key).await? {
                        let marker: MarkerBody = serde_json::from_slice(&bytes)
                            .with_context(|| format!("parsing {object_key}"))?;
                        return Ok(MarkerOutcome::Exists {
                            id: marker.id,
                            fingerprint: marker.fingerprint,
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                anyhow::bail!("marker {object_key} reported as existing but could not be read")
            }
            Err(e) => Err(explain(
                e,
                format!("creating marker {object_key}"),
                &s3_permissions(&self.bucket),
            )),
        }
    }

    pub async fn put_webhook(&self, webhook: &Webhook) -> anyhow::Result<()> {
        self.put(
            keys::webhook(&webhook.id),
            serde_json::to_vec(webhook)?,
            "application/json",
        )
        .await
    }

    pub async fn get_webhook(&self, id: &str) -> anyhow::Result<Option<Webhook>> {
        let key = keys::webhook(id);
        match self.get(&key).await? {
            Some(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).with_context(|| format!("parsing {key}"))?,
            )),
            None => Ok(None),
        }
    }

    pub async fn delete_webhook(&self, id: &str) -> anyhow::Result<()> {
        self.delete(keys::webhook(id)).await
    }

    /// A project's webhooks, oldest first. Reads every webhook in the
    /// bucket, which is fine at a few dozen projects.
    pub async fn list_webhooks(&self, project: Uuid) -> anyhow::Result<Vec<Webhook>> {
        let keys = self.list_keys("webhooks/").await?;
        let raw: Vec<(String, Option<Vec<u8>>)> = futures::stream::iter(keys)
            .map(|key| async move { self.get(&key).await.map(|bytes| (key, bytes)) })
            .buffer_unordered(16)
            .try_collect()
            .await?;
        let mut found: Vec<Webhook> = raw
            .into_iter()
            .filter_map(
                |(key, bytes)| match serde_json::from_slice::<Webhook>(&bytes?) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        tracing::warn!(key, error = %e, "unparseable webhook; skipping");
                        None
                    }
                },
            )
            .filter(|w| w.project_id == project)
            .collect();
        found.sort_by_key(|w| w.created_at);
        Ok(found)
    }

    async fn list_keys(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| {
                explain(
                    e,
                    format!("listing objects in {}", self.bucket),
                    &s3_permissions(&self.bucket),
                )
            })?;
            keys.extend(
                page.contents()
                    .iter()
                    .filter_map(|o| o.key().map(String::from)),
            );
        }
        Ok(keys)
    }

    /// Every datapoint's metadata across all projects.
    pub async fn list_metas(&self) -> anyhow::Result<Vec<Meta>> {
        let found: Vec<(Uuid, Uuid)> = self
            .list_keys("projects/")
            .await?
            .iter()
            .filter_map(|key| keys::parse_meta_key(key))
            .collect();
        let raw: Vec<(String, Option<Vec<u8>>)> = futures::stream::iter(found)
            .map(|(project, id)| async move {
                let key = keys::meta(project, id);
                self.get(&key).await.map(|bytes| (key, bytes))
            })
            .buffer_unordered(16)
            .try_collect()
            .await?;
        // A corrupt object is skipped rather than failing startup, which
        // would crash-loop until someone fixed the bucket by hand.
        Ok(raw
            .into_iter()
            .filter_map(|(key, bytes)| match serde_json::from_slice(&bytes?) {
                Ok(meta) => Some(meta),
                Err(e) => {
                    tracing::warn!(key, error = %e, "unparseable meta.json; skipping");
                    None
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_region_is_a_clear_error() {
        let err = require_region(None).unwrap_err().to_string();
        assert!(err.contains("AWS_REGION"), "{err}");
        assert!(err.contains(".env"), "{err}");
        require_region(Some(&aws_config::Region::new("us-east-1"))).unwrap();
    }

    #[test]
    fn key_layout() {
        let p = Uuid::nil();
        let id = Uuid::max();
        assert_eq!(
            keys::content(p, id),
            format!("projects/{p}/datapoints/{id}/content")
        );
        assert_eq!(
            keys::meta(p, id),
            format!("projects/{p}/datapoints/{id}/meta.json")
        );
        assert_eq!(
            keys::marker(p, "abc"),
            format!(
                "projects/{p}/idempotency/ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            )
        );
    }

    #[test]
    fn parse_meta_key_roundtrip() {
        let (p, id) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(keys::parse_meta_key(&keys::meta(p, id)), Some((p, id)));
        assert_eq!(
            keys::parse_meta_key("projects/x/datapoints/y/content"),
            None
        );
    }
}
