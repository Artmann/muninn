#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use muninn::api::locks::KeyLocks;
use muninn::api::state::AppState;
use muninn::config::Config;
use muninn::embed::FakeEmbedder;
use muninn::index::MemoryIndex;
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
    env.insert("MUNINN_BUCKET".into(), bucket.clone());
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
/// index, marked ready.
pub async fn app() -> (axum::Router, AppState) {
    let state = AppState {
        store: store().await,
        index: Arc::new(MemoryIndex::new()),
        embedder: Arc::new(FakeEmbedder::new(64)),
        ready: Arc::new(AtomicBool::new(true)),
        locks: KeyLocks::default(),
    };
    (muninn::api::router(state.clone()), state)
}

pub async fn post(router: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
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
pub async fn index_len(state: &AppState, project: Uuid) -> usize {
    let probe = vec![0.0; state.embedder.dim()];
    state
        .index
        .search(project, &probe, usize::MAX)
        .await
        .unwrap()
        .len()
}
