mod common;

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use muninn::store::MarkerOutcome;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore]
async fn ingest_returns_201_with_server_id_and_defaults() {
    let (app, state) = common::app().await;
    let p = common::project();
    let (status, body) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "label": "L", "content": "the content" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(id.get_version_num(), 7);
    assert_eq!(body["metadata"], json!({}));
    assert!(body["timestamp"].is_string());
    assert_eq!(body["deduplicated"], false);
    assert_eq!(body["projectId"], json!(p));
    assert_eq!(
        state.store.get_content(p, id).await.unwrap().as_deref(),
        Some("the content")
    );
    assert_eq!(common::index_len(&state, p).await, 1);
}

#[tokio::test]
#[ignore]
async fn ingest_with_key_dedupes_and_keeps_first_body() {
    let (app, state) = common::app().await;
    let p = common::project();
    let (s1, b1) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "label": "first", "content": "c", "idempotencyKey": "K" }),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, b2) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "label": "second", "content": "c", "idempotencyKey": "K" }),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "{b2}");
    assert_eq!(b2["deduplicated"], true);
    assert_eq!(b2["label"], "first");
    assert_eq!(b2["id"], b1["id"]);
    assert_eq!(common::index_len(&state, p).await, 1);
}

#[tokio::test]
#[ignore]
async fn same_key_different_projects_are_distinct() {
    let (app, _state) = common::app().await;
    let (p1, p2) = (common::project(), common::project());
    let (s1, b1) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p1, "label": "a", "content": "c", "idempotencyKey": "K" }),
    )
    .await;
    let (s2, b2) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p2, "label": "a", "content": "c", "idempotencyKey": "K" }),
    )
    .await;
    assert_eq!((s1, s2), (StatusCode::CREATED, StatusCode::CREATED));
    assert_ne!(b1["id"], b2["id"]);
}

#[tokio::test]
#[ignore]
async fn ingest_completes_half_written_datapoint() {
    let (app, state) = common::app().await;
    let p = common::project();
    let x = Uuid::now_v7();
    assert!(matches!(
        state.store.create_marker(p, "K", x).await.unwrap(),
        MarkerOutcome::Created
    ));
    let (status, body) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "label": "a", "content": "c", "idempotencyKey": "K" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], json!(x));
    assert_eq!(body["deduplicated"], false);
    assert!(state.store.get_meta(p, x).await.unwrap().is_some());
}

#[tokio::test]
#[ignore]
async fn validation_errors_are_400() {
    let (app, _state) = common::app().await;
    let p = common::project();
    let cases = [
        json!({ "projectId": p, "label": "a" }),
        json!({ "projectId": p, "label": "a", "content": "c", "metadata": [1] }),
        json!({ "projectId": "nope", "label": "a", "content": "c" }),
        json!({ "projectId": p, "label": "   ", "content": "c" }),
        json!({ "projectId": p, "label": "a", "content": "\n" }),
    ];
    for case in cases {
        let (status, body) = common::post(&app, "/api/v1/ingest", case.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case} -> {body}");
        assert_eq!(body["error"]["code"], "validation", "{case}");
    }
}

#[tokio::test]
#[ignore]
async fn loading_returns_503() {
    let (app, state) = common::app().await;
    state.ready.store(false, Ordering::SeqCst);
    let (status, body) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": common::project(), "label": "a", "content": "c" }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "loading");
}

fn items(project: Uuid, n: usize) -> Vec<serde_json::Value> {
    (0..n)
        .map(|i| json!({ "projectId": project, "label": format!("item {i}"), "content": format!("content {i}") }))
        .collect()
}

#[tokio::test]
#[ignore]
async fn batch_ingests_in_order_with_per_item_status() {
    let (app, state) = common::app().await;
    let p = common::project();
    let (status, body) = common::post(
        &app,
        "/api/v1/ingest/batch",
        json!({ "items": items(p, 3) }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    let mut ids = Vec::new();
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r["status"], 201, "{r}");
        assert_eq!(r["deduplicated"], false);
        assert_eq!(r["datapoint"]["label"], format!("item {i}"));
        ids.push(
            r["datapoint"]["id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap(),
        );
    }
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(common::index_len(&state, p).await, 3);
}

#[tokio::test]
#[ignore]
async fn batch_validates_all_before_writing() {
    let (app, state) = common::app().await;
    let p = common::project();
    let mut list = items(p, 2);
    list[1]["label"] = json!("");
    let (status, body) = common::post(&app, "/api/v1/ingest/batch", json!({ "items": list })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "validation");
    assert_eq!(body["error"]["details"][0]["index"], 1);
    assert_eq!(common::index_len(&state, p).await, 0);
}

#[tokio::test]
#[ignore]
async fn batch_retry_with_key_dedupes_per_item() {
    let (app, state) = common::app().await;
    let p = common::project();
    let req = json!({ "idempotencyKey": "B", "items": items(p, 3) });
    let (_, first) = common::post(&app, "/api/v1/ingest/batch", req.clone()).await;
    let (status, second) = common::post(&app, "/api/v1/ingest/batch", req).await;
    assert_eq!(status, StatusCode::OK);
    for i in 0..3 {
        let r = &second["results"][i];
        assert_eq!(r["status"], 200, "{r}");
        assert_eq!(r["deduplicated"], true);
        assert_eq!(r["datapoint"]["id"], first["results"][i]["datapoint"]["id"]);
    }
    assert_eq!(common::index_len(&state, p).await, 3);
}

#[tokio::test]
#[ignore]
async fn batch_too_large_is_400() {
    let (app, _state) = common::app().await;
    let (status, body) = common::post(
        &app,
        "/api/v1/ingest/batch",
        json!({ "items": items(common::project(), 101) }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "batch-too-large");
}

#[tokio::test]
#[ignore]
async fn dedupe_restores_missing_index_entry() {
    let (app, state) = common::app().await;
    let p = common::project();
    let body = json!({ "idempotencyKey": "k", "projectId": p, "label": "l", "content": "c" });
    let (status, _) = common::post(&app, "/api/v1/ingest", body.clone()).await;
    assert_eq!(status, StatusCode::CREATED);

    // Same S3 data, but the index lost the entry (request cancelled after
    // meta.json was written, before the index upsert).
    let fresh = muninn::api::AppState {
        index: std::sync::Arc::new(muninn::index::MemoryIndex::new()),
        ..state.clone()
    };
    let app2 = muninn::api::router(fresh.clone());
    let (status, resp) = common::post(&app2, "/api/v1/ingest", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["deduplicated"], true);
    assert_eq!(common::index_len(&fresh, p).await, 1);
}

#[tokio::test]
#[ignore]
async fn blank_idempotency_key_is_rejected() {
    let (app, _) = common::app().await;
    let p = common::project();
    for key in ["", "   "] {
        let (status, resp) = common::post(
            &app,
            "/api/v1/ingest",
            json!({ "idempotencyKey": key, "projectId": p, "label": "l", "content": "c" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key:?}");
        assert_eq!(resp["error"]["code"], "validation");
        let (status, resp) = common::post(
            &app,
            "/api/v1/ingest/batch",
            json!({ "idempotencyKey": key, "items": [{ "projectId": p, "label": "l", "content": "c" }] }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "batch {key:?}");
        assert_eq!(resp["error"]["code"], "validation");
    }
}

#[tokio::test]
#[ignore]
async fn body_limit_is_ten_mib_with_json_errors() {
    let (app, _) = common::app().await;
    let p = common::project();
    let three_mb = "x".repeat(3 * 1024 * 1024);
    let (status, resp) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "label": "big", "content": three_mb }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{}", resp["error"]);

    let eleven_mb = "x".repeat(11 * 1024 * 1024);
    let (status, resp) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "label": "huge", "content": eleven_mb }),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(resp["error"]["code"], "payload-too-large");
}
