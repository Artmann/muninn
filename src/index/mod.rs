mod memory;
mod s3vectors;

pub use memory::MemoryIndex;
pub use s3vectors::{S3VectorsIndex, index_name};

use async_trait::async_trait;
use uuid::Uuid;

/// Nearest-neighbour search over datapoint embeddings, scoped by project.
#[async_trait]
pub trait VectorIndex: Send + Sync {
    /// Store a vector for `id`, replacing any vector it already had.
    async fn upsert(&self, project: Uuid, id: Uuid, vector: Vec<f32>) -> anyhow::Result<()>;

    /// Store many `(project, id, vector)` entries. Backends with a batch
    /// API override this.
    async fn upsert_many(&self, items: Vec<(Uuid, Uuid, Vec<f32>)>) -> anyhow::Result<()> {
        for (project, id, vector) in items {
            self.upsert(project, id, vector).await?;
        }
        Ok(())
    }

    /// The `k` best matches by dot product, highest score first.
    async fn search(
        &self,
        project: Uuid,
        query: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<(Uuid, f32)>>;
}
