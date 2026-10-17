use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use uuid::Uuid;

use super::VectorIndex;

type Entries = HashMap<Uuid, Vec<f32>>;

/// Brute-force in-process index. Rebuilt from storage at startup.
#[derive(Default)]
pub struct MemoryIndex {
    projects: RwLock<HashMap<Uuid, Entries>>,
}

impl MemoryIndex {
    pub fn new() -> MemoryIndex {
        MemoryIndex::default()
    }

    pub fn len(&self, project: Uuid) -> usize {
        self.projects
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&project)
            .map_or(0, HashMap::len)
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[async_trait]
impl VectorIndex for MemoryIndex {
    async fn upsert(&self, project: Uuid, id: Uuid, vector: Vec<f32>) -> anyhow::Result<()> {
        let mut projects = self.projects.write().unwrap_or_else(|e| e.into_inner());
        projects.entry(project).or_default().insert(id, vector);
        Ok(())
    }

    async fn search(
        &self,
        project: Uuid,
        query: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<(Uuid, f32)>> {
        let projects = self.projects.read().unwrap_or_else(|e| e.into_inner());
        let Some(entries) = projects.get(&project) else {
            return Ok(Vec::new());
        };
        let mut scored: Vec<(Uuid, f32)> =
            entries.iter().map(|(id, v)| (*id, dot(query, v))).collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        Ok(scored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_orders_by_dot_product_and_caps_k() {
        let idx = MemoryIndex::new();
        let p = Uuid::now_v7();
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        idx.upsert(p, a, vec![1.0, 0.0]).await.unwrap();
        idx.upsert(p, b, vec![0.0, 1.0]).await.unwrap();
        idx.upsert(p, c, vec![0.7, 0.7]).await.unwrap();
        let hits = idx.search(p, &[1.0, 0.0], 2).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, a);
        assert_eq!(hits[1].0, c);
        assert!(hits[0].1 >= hits[1].1);
    }

    #[tokio::test]
    async fn projects_are_isolated() {
        let idx = MemoryIndex::new();
        let (p1, p2) = (Uuid::now_v7(), Uuid::now_v7());
        idx.upsert(p1, Uuid::now_v7(), vec![1.0]).await.unwrap();
        assert!(idx.search(p2, &[1.0], 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upsert_replaces_existing_id() {
        let idx = MemoryIndex::new();
        let (p, id) = (Uuid::now_v7(), Uuid::now_v7());
        idx.upsert(p, id, vec![1.0, 0.0]).await.unwrap();
        idx.upsert(p, id, vec![0.0, 1.0]).await.unwrap();
        assert_eq!(idx.len(p), 1);
        let hits = idx.search(p, &[0.0, 1.0], 10).await.unwrap();
        assert_eq!(hits, vec![(id, 1.0)]);
    }

    #[tokio::test]
    async fn empty_project_returns_empty() {
        let idx = MemoryIndex::new();
        assert!(
            idx.search(Uuid::now_v7(), &[1.0], 5)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(idx.len(Uuid::now_v7()), 0);
    }
}
