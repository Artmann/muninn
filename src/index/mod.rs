mod memory;

pub use memory::MemoryIndex;

use async_trait::async_trait;
use uuid::Uuid;

/// Nearest-neighbour search over datapoint embeddings, scoped by project.
#[async_trait]
pub trait VectorIndex: Send + Sync {
    /// Store a vector for `id`, replacing any vector it already had.
    async fn upsert(&self, project: Uuid, id: Uuid, vector: Vec<f32>) -> anyhow::Result<()>;

    /// The `k` best matches by dot product, highest score first.
    async fn search(
        &self,
        project: Uuid,
        query: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<(Uuid, f32)>>;
}
