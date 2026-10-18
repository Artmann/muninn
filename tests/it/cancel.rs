//! A client that disconnects drops its request handler part way. Writes
//! must still finish, or storage and the index disagree until the next
//! start.

use crate::common;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
use muninn::app::{Infra, Services};
use muninn::datapoints::{DeleteCommand, NewDatapoint};
use muninn::embed::FakeEmbedder;
use muninn::index::{MemoryIndex, VectorIndex};
use serde_json::json;
use uuid::Uuid;

const DELAY: Duration = Duration::from_millis(300);

/// Pauses before each upsert and after each remove, so a caller that gives
/// up early stops in the middle of a write.
struct SlowIndex(MemoryIndex);

#[async_trait]
impl VectorIndex for SlowIndex {
    async fn upsert(&self, project: Uuid, id: Uuid, vector: Vec<f32>) -> anyhow::Result<()> {
        tokio::time::sleep(DELAY).await;
        self.0.upsert(project, id, vector).await
    }

    async fn remove(&self, project: Uuid, id: Uuid) -> anyhow::Result<()> {
        self.0.remove(project, id).await?;
        tokio::time::sleep(DELAY).await;
        Ok(())
    }

    async fn search(
        &self,
        project: Uuid,
        query: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<(Uuid, f32)>> {
        self.0.search(project, query, k).await
    }
}

async fn services() -> (Services, Infra) {
    let infra = Infra {
        store: common::store().await,
        index: Arc::new(SlowIndex(MemoryIndex::new())),
        embedder: Arc::new(FakeEmbedder::new(64)),
        ready: Arc::new(AtomicBool::new(true)),
    };
    (Services::new(infra.clone()), infra)
}

fn new_datapoint(p: Uuid) -> NewDatapoint {
    serde_json::from_value(json!({ "projectId": p, "label": "L", "content": "c" })).unwrap()
}

#[tokio::test]
#[ignore]
async fn abandoned_ingest_still_reaches_the_index() {
    let (services, infra) = services().await;
    let p = common::project();
    let mut new = new_datapoint(p);
    new.source_id = Some("crm-1".into());
    let ingest = services.datapoints.ingest(new, None);
    let abandoned = tokio::time::timeout(DELAY / 2, ingest).await;
    assert!(abandoned.is_err(), "the caller gave up before the upsert");

    tokio::time::sleep(DELAY * 2).await;
    let id = muninn::datapoints::source_uuid(p, "crm-1");
    assert!(
        infra.store.get_meta(p, id).await.unwrap().is_some(),
        "stored"
    );
    assert_eq!(common::index_len(&infra, p).await, 1, "and indexed");
}

#[tokio::test]
#[ignore]
async fn abandoned_delete_still_removes_from_storage() {
    let (services, infra) = services().await;
    let p = common::project();
    let id = services
        .datapoints
        .ingest(new_datapoint(p), None)
        .await
        .unwrap()
        .datapoint
        .id;
    assert_eq!(common::index_len(&infra, p).await, 1);

    let delete = services.datapoints.delete(DeleteCommand {
        project_id: p,
        source_ids: vec![],
        ids: vec![id],
    });
    let abandoned = tokio::time::timeout(DELAY / 2, delete).await;
    assert!(
        abandoned.is_err(),
        "the caller gave up after the index removal"
    );

    tokio::time::sleep(DELAY * 2).await;
    assert_eq!(common::index_len(&infra, p).await, 0);
    assert!(
        infra.store.get_meta(p, id).await.unwrap().is_none(),
        "a datapoint left in storage would be invisible to recall"
    );
}
