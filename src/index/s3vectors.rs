use std::collections::HashMap;

use anyhow::Context;
use async_trait::async_trait;
use aws_sdk_s3vectors::Client;
use aws_sdk_s3vectors::types::{DataType, DistanceMetric, PutInputVector, VectorData};
use aws_smithy_types::Document;
use uuid::Uuid;

use super::VectorIndex;

/// Most vectors one PutVectors call accepts.
const PUT_BATCH: usize = 500;
const PROJECT_KEY: &str = "projectId";

/// Amazon S3 Vectors index. One index per embedding model inside the
/// configured vector bucket, so switching models never mixes dimensions.
/// Vectors are keyed by datapoint id and carry `projectId` as filterable
/// metadata.
pub struct S3VectorsIndex {
    client: Client,
    bucket: String,
    index: String,
}

/// Index name for a model: `muninn-` plus the model name with anything
/// other than lowercase letters, digits and hyphens turned into hyphens.
pub fn index_name(model: &str) -> String {
    let safe: String = model
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut name = format!("muninn-{safe}");
    name.truncate(63);
    name.trim_end_matches('-').to_string()
}

fn project_filter(project: Uuid) -> Document {
    let eq = Document::Object(HashMap::from([(
        "$eq".to_string(),
        Document::String(project.to_string()),
    )]));
    Document::Object(HashMap::from([(PROJECT_KEY.to_string(), eq)]))
}

fn project_metadata(project: Uuid) -> Document {
    Document::Object(HashMap::from([(
        PROJECT_KEY.to_string(),
        Document::String(project.to_string()),
    )]))
}

/// The index uses cosine distance (1 - cosine similarity); callers expect
/// a similarity score where higher is better.
fn score(distance: f32) -> f32 {
    1.0 - distance
}

impl S3VectorsIndex {
    pub async fn connect(
        sdk: &aws_config::SdkConfig,
        bucket: String,
        model: &str,
        dim: usize,
    ) -> anyhow::Result<S3VectorsIndex> {
        let idx = S3VectorsIndex {
            client: Client::new(sdk),
            bucket,
            index: index_name(model),
        };
        idx.ensure(dim).await?;
        Ok(idx)
    }

    pub fn index(&self) -> &str {
        &self.index
    }

    /// Create the vector bucket and index if missing, and check an existing
    /// index has the model's dimension.
    async fn ensure(&self, dim: usize) -> anyhow::Result<()> {
        let got = self
            .client
            .get_index()
            .vector_bucket_name(&self.bucket)
            .index_name(&self.index)
            .send()
            .await;
        match got {
            Ok(out) => {
                let existing = out.index().map(|i| i.dimension());
                if existing != Some(dim as i32) {
                    anyhow::bail!(
                        "S3 Vectors index {}/{} has dimension {existing:?}, model needs {dim}",
                        self.bucket,
                        self.index
                    );
                }
                return Ok(());
            }
            Err(e)
                if e.as_service_error()
                    .is_some_and(|e| e.is_not_found_exception()) => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("reading S3 Vectors index {}/{}", self.bucket, self.index)
                });
            }
        }

        // NotFound may mean the bucket or only the index is missing.
        match self
            .client
            .create_vector_bucket()
            .vector_bucket_name(&self.bucket)
            .send()
            .await
        {
            Ok(_) => tracing::info!(bucket = %self.bucket, "created vector bucket"),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|e| e.is_conflict_exception()) => {}
            Err(e) => {
                return Err(e).with_context(|| format!("creating vector bucket {}", self.bucket));
            }
        }
        match self
            .client
            .create_index()
            .vector_bucket_name(&self.bucket)
            .index_name(&self.index)
            .data_type(DataType::Float32)
            .dimension(dim as i32)
            .distance_metric(DistanceMetric::Cosine)
            .send()
            .await
        {
            Ok(_) => {
                tracing::info!(bucket = %self.bucket, index = %self.index, dim, "created vector index")
            }
            Err(e)
                if e.as_service_error()
                    .is_some_and(|e| e.is_conflict_exception()) => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("creating S3 Vectors index {}/{}", self.bucket, self.index)
                });
            }
        }
        Ok(())
    }
}

#[async_trait]
impl VectorIndex for S3VectorsIndex {
    async fn upsert(&self, project: Uuid, id: Uuid, vector: Vec<f32>) -> anyhow::Result<()> {
        self.upsert_many(vec![(project, id, vector)]).await
    }

    async fn upsert_many(&self, items: Vec<(Uuid, Uuid, Vec<f32>)>) -> anyhow::Result<()> {
        let mut items = items.into_iter().peekable();
        while items.peek().is_some() {
            let mut req = self
                .client
                .put_vectors()
                .vector_bucket_name(&self.bucket)
                .index_name(&self.index);
            for (project, id, vector) in items.by_ref().take(PUT_BATCH) {
                req = req.vectors(
                    PutInputVector::builder()
                        .key(id.to_string())
                        .data(VectorData::Float32(vector))
                        .metadata(project_metadata(project))
                        .build()?,
                );
            }
            req.send().await.context("S3 Vectors PutVectors")?;
        }
        Ok(())
    }

    async fn search(
        &self,
        project: Uuid,
        query: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<(Uuid, f32)>> {
        let mut hits = Vec::new();
        let mut token = None;
        loop {
            let out = self
                .client
                .query_vectors()
                .vector_bucket_name(&self.bucket)
                .index_name(&self.index)
                .query_vector(VectorData::Float32(query.to_vec()))
                .top_k(k as i32)
                .filter(project_filter(project))
                .return_distance(true)
                .set_next_token(token)
                .send()
                .await
                .context("S3 Vectors QueryVectors")?;
            for v in out.vectors() {
                let Ok(id) = v.key().parse::<Uuid>() else {
                    tracing::warn!(key = v.key(), "vector key is not a uuid; skipping");
                    continue;
                };
                hits.push((id, score(v.distance().unwrap_or(1.0))));
            }
            token = out
                .next_token()
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if hits.len() >= k || token.is_none() {
                break;
            }
        }
        hits.sort_by(|a, b| b.1.total_cmp(&a.1));
        hits.truncate(k);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_names_are_valid() {
        assert_eq!(
            index_name("multilingual-e5-small"),
            "muninn-multilingual-e5-small"
        );
        assert_eq!(index_name("bge-small-en-v1.5"), "muninn-bge-small-en-v1-5");
        let long = index_name(&"x".repeat(100));
        assert!(long.len() <= 63);
    }

    #[test]
    fn filter_matches_project_exactly() {
        let p = Uuid::nil();
        let Document::Object(f) = project_filter(p) else {
            panic!("object")
        };
        let Document::Object(eq) = &f["projectId"] else {
            panic!("object")
        };
        assert_eq!(eq["$eq"], Document::String(p.to_string()));
    }

    #[test]
    fn distance_converts_to_similarity() {
        assert_eq!(score(0.0), 1.0);
        assert!(score(0.2) > score(0.5));
    }
}
