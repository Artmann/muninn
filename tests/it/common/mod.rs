#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use muninn::app::{Infra, Services};
use muninn::config::Config;
use muninn::embed::FakeEmbedder;
use muninn::index::{MemoryIndex, VectorIndex};
use muninn::store::S3Store;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

/// A store on a fresh bucket, configured from the environment
/// (`AWS_ENDPOINT_URL` pointing at LocalStack for local runs).
pub async fn store() -> S3Store {
    store_with_bucket().await.0
}

/// Like [`store`], also returning the bucket name for raw writes.
pub async fn store_with_bucket() -> (S3Store, String) {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    let bucket = format!("muninn-test-{}", Uuid::now_v7());
    env.insert("BUCKET".into(), bucket.clone());
    let cfg = Config::from_map(&env).expect("config");
    let store = S3Store::from_config(&cfg).await.expect("store");
    store.ensure_bucket().await.expect("bucket");
    (store, bucket)
}

/// Write arbitrary bytes to `key`, bypassing the store, to plant bad data.
pub async fn put_raw(bucket: &str, key: &str, body: &[u8]) {
    let sdk = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let mut conf = aws_sdk_s3::config::Builder::from(&sdk);
    if let Ok(url) = std::env::var("AWS_ENDPOINT_URL") {
        conf = conf.endpoint_url(url).force_path_style(true);
    }
    aws_sdk_s3::Client::from_conf(conf.build())
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(body.to_vec().into())
        .send()
        .await
        .expect("raw put");
}

pub fn project() -> Uuid {
    Uuid::now_v7()
}

/// The full router over a fresh bucket, a fake embedder and an in-memory
/// index, marked ready, and the infrastructure behind it.
pub async fn app() -> (axum::Router, Infra) {
    let infra = Infra {
        store: store().await,
        index: Arc::new(MemoryIndex::new()),
        embedder: Arc::new(FakeEmbedder::new(64)),
        ready: Arc::new(AtomicBool::new(true)),
    };
    (muninn::http::router(Services::new(infra.clone())), infra)
}

pub async fn post(router: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(router, req).await
}

/// Any method with an optional JSON body.
pub async fn call(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    send(router, req).await
}

/// POST raw bytes, as a third-party webhook sender would.
pub async fn post_raw(
    router: &axum::Router,
    path: &str,
    content_type: &str,
    body: impl Into<Body>,
) -> (StatusCode, Value) {
    let req = Request::post(path)
        .header("content-type", content_type)
        .body(body.into())
        .unwrap();
    send(router, req).await
}

pub async fn get(router: &axum::Router, path: &str) -> (StatusCode, Value) {
    send(router, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn send(router: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// How many vectors the index holds for a project.
pub async fn index_len(state: &Infra, project: Uuid) -> usize {
    let probe = vec![0.0; state.embedder.dim()];
    state
        .index
        .search(project, &probe, usize::MAX)
        .await
        .unwrap()
        .len()
}

/// How long [`SlowIndex`] pauses.
pub const DELAY: Duration = Duration::from_millis(300);

/// Pauses before each upsert and after each remove, so a caller that gives
/// up early stops in the middle of a write.
pub struct SlowIndex(MemoryIndex);

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

/// Services over a [`SlowIndex`].
pub async fn slow_services() -> (Services, Infra) {
    let infra = Infra {
        store: store().await,
        index: Arc::new(SlowIndex(MemoryIndex::new())),
        embedder: Arc::new(FakeEmbedder::new(64)),
        ready: Arc::new(AtomicBool::new(true)),
    };
    (Services::new(infra.clone()), infra)
}
