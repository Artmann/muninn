mod common;

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use chrono::Utc;
use muninn::boot::rebuild_index;
use muninn::domain::{Meta, StoredEmbedding};
use std::sync::Arc;

use muninn::embed::{Embedder, FakeEmbedder};
use muninn::index::MemoryIndex;
use serde_json::json;
use uuid::Uuid;

fn fake() -> Arc<dyn Embedder> {
    Arc::new(FakeEmbedder::new(64))
}

fn old_meta(project: Uuid, id: Uuid) -> Meta {
    Meta {
        id,
        project_id: project,
        source_id: None,
        label: "label".into(),
        timestamp: Utc::now(),
        metadata: serde_json::Map::new(),
        audience: None,
        embedding: StoredEmbedding {
            model: "old".into(),
            dim: 3,
            vector: vec![1.0, 0.0, 0.0],
        },
        content_tokens: 0,
        content_sha256: None,
    }
}

#[tokio::test]
#[ignore]
async fn rebuild_loads_existing_metas() {
    let (app, state) = common::app().await;
    let p = common::project();
    for label in ["a", "b"] {
        let (status, _) = common::post(
            &app,
            "/api/v1/ingest",
            json!({ "projectId": p, "label": label, "content": "c" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let fresh = MemoryIndex::new();
    let n = rebuild_index(&state.store, &fresh, fake()).await.unwrap();
    assert_eq!(n, 2);
    assert_eq!(fresh.len(p), 2);
}

#[tokio::test]
#[ignore]
async fn rebuild_reembeds_on_model_mismatch() {
    let store = common::store().await;
    let (p, id) = (common::project(), Uuid::now_v7());
    store.put_content(p, id, "some content here").await.unwrap();
    store.put_meta(&old_meta(p, id)).await.unwrap();

    let index = MemoryIndex::new();
    let n = rebuild_index(&store, &index, fake()).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(index.len(p), 1);
    let meta = store.get_meta(p, id).await.unwrap().unwrap();
    assert_eq!(meta.embedding.model, "fake");
    assert_eq!(meta.embedding.dim, 64);
    assert_eq!(meta.embedding.vector.len(), 64);
    assert!(meta.content_tokens > 0);
}

#[tokio::test]
#[ignore]
async fn rebuild_skips_meta_without_content() {
    let store = common::store().await;
    let p = common::project();
    store.put_meta(&old_meta(p, Uuid::now_v7())).await.unwrap();
    let index = MemoryIndex::new();
    let n = rebuild_index(&store, &index, fake()).await.unwrap();
    assert_eq!(n, 0);
    assert_eq!(index.len(p), 0);
}

#[tokio::test]
#[ignore]
async fn healthz_reports_loading_then_ok() {
    let (app, state) = common::app().await;
    state.ready.store(false, Ordering::SeqCst);
    let (status, body) = common::get(&app, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "loading");
    state.ready.store(true, Ordering::SeqCst);
    let (_, body) = common::get(&app, "/healthz").await;
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
#[ignore]
async fn rebuild_skips_unparseable_meta() {
    let (store, bucket) = common::store_with_bucket().await;
    let (p, good) = (common::project(), Uuid::now_v7());
    store.put_content(p, good, "fine").await.unwrap();
    store.put_meta(&old_meta(p, good)).await.unwrap();
    common::put_raw(
        &bucket,
        &muninn::store::keys::meta(p, Uuid::now_v7()),
        b"{not json",
    )
    .await;

    let index = MemoryIndex::new();
    let n = rebuild_index(&store, &index, fake()).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(index.len(p), 1);
}
