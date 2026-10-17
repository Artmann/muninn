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
