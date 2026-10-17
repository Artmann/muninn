#![allow(dead_code)]

use std::collections::HashMap;

use muninn::config::Config;
use muninn::store::S3Store;
use uuid::Uuid;

/// A store on a fresh bucket, configured from the environment
/// (`AWS_ENDPOINT_URL` pointing at LocalStack for local runs).
pub async fn store() -> S3Store {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.insert(
        "MUNINN_BUCKET".into(),
        format!("muninn-test-{}", Uuid::now_v7()),
    );
    let cfg = Config::from_map(&env).expect("config");
    let store = S3Store::from_config(&cfg).await.expect("store");
    store.ensure_bucket().await.expect("bucket");
    store
}

pub fn project() -> Uuid {
    Uuid::now_v7()
}
