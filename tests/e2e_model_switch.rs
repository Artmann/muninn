//! Switching `EMBED_MODEL` re-embeds existing data on the next start.
//! Separate from `e2e` because it downloads `bge-small-en-v1.5` (~130 MB)
//! on first run.
//!
//! ```sh
//! set -a; . ./.env.example; set +a
//! cargo test --test e2e_model_switch -- --ignored
//! ```

mod e2e_support;

use e2e_support::{Server, labels};
use muninn::config::Config;
use muninn::store::S3Store;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore]
async fn switching_model_reembeds_existing_data() {
    let bucket = format!("muninn-e2e-{}", Uuid::now_v7());
    let project = Uuid::now_v7();

    let first = Server::start_with(&bucket, Some("bge-small-en-v1.5")).await;
    let stored = first
        .ingest(json!({
            "projectId": project,
            "label": "Storage decision",
            "content": "We store all memories in S3 so there is no separate database to run."
        }))
        .await;
    first
        .ingest(json!({
            "projectId": project,
            "label": "Banana bread",
            "content": "Three ripe bananas, flour, eggs and butter."
        }))
        .await;
    first.stop();

    let second = Server::start_with(&bucket, None).await;
    let resp = second
        .recall(json!({ "projectId": project, "query": "where do we keep data" }))
        .await;
    assert_eq!(labels(&resp)[0], "Storage decision");

    let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
    env.insert("BUCKET".into(), bucket);
    let store = S3Store::from_config(&Config::from_map(&env).unwrap())
        .await
        .unwrap();
    let id = stored["id"].as_str().unwrap().parse().unwrap();
    let meta = store.get_meta(project, id).await.unwrap().unwrap();
    assert_eq!(meta.embedding.model, "multilingual-e5-small");
}
