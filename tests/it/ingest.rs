use crate::common;
use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use muninn::datapoints::NewDatapoint;
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
async fn ingest_with_key_dedupes_identical_retry() {
    let (app, state) = common::app().await;
    let p = common::project();
    let body = json!({ "projectId": p, "label": "first", "content": "c", "idempotencyKey": "K" });
    let (s1, b1) = common::post(&app, "/api/v1/ingest", body.clone()).await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, b2) = common::post(&app, "/api/v1/ingest", body).await;
    assert_eq!(s2, StatusCode::OK, "{b2}");
    assert_eq!(b2["deduplicated"], true);
    assert_eq!(b2["id"], b1["id"]);
    assert_eq!(
        b2["timestamp"], b1["timestamp"],
        "defaulted timestamp is kept"
    );
    assert_eq!(common::index_len(&state, p).await, 1);
}

#[tokio::test]
#[ignore]
async fn reusing_key_with_different_body_is_422() {
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
    assert_eq!(s2, StatusCode::UNPROCESSABLE_ENTITY, "{b2}");
    assert_eq!(b2["error"]["code"], "idempotency-key-reused");
    let id: Uuid = b1["id"].as_str().unwrap().parse().unwrap();
    let meta = state.store.get_meta(p, id).await.unwrap().unwrap();
    assert_eq!(meta.label, "first");
    assert_eq!(common::index_len(&state, p).await, 1);
}

#[tokio::test]
#[ignore]
async fn half_written_datapoint_rejects_different_body() {
    let (app, state) = common::app().await;
    let p = common::project();
    let original: NewDatapoint =
        serde_json::from_value(json!({ "projectId": p, "label": "a", "content": "c" })).unwrap();
    let x = Uuid::now_v7();
    state
        .store
        .create_marker(p, "K", x, &original.fingerprint())
        .await
        .unwrap();
    let (status, body) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "label": "a", "content": "changed", "idempotencyKey": "K" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(state.store.get_meta(p, x).await.unwrap().is_none());
}

#[tokio::test]
#[ignore]
async fn marker_without_fingerprint_is_honoured() {
    // Markers written before fingerprints existed hold only the id.
    let (app, state) = common::app().await;
    let p = common::project();
    let x = Uuid::now_v7();
    common::put_raw(
        state.store.bucket(),
        &muninn::store::keys::marker(p, "K"),
        json!({ "id": x }).to_string().as_bytes(),
    )
    .await;
    let (status, body) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "label": "a", "content": "c", "idempotencyKey": "K" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], json!(x));
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
    let req = json!({ "projectId": p, "label": "a", "content": "c", "idempotencyKey": "K" });
    let new: NewDatapoint = serde_json::from_value(req.clone()).unwrap();
    assert!(matches!(
        state
            .store
            .create_marker(p, "K", x, &new.fingerprint())
            .await
            .unwrap(),
        MarkerOutcome::Created
    ));
    let (status, body) = common::post(&app, "/api/v1/ingest", req).await;
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
async fn batch_retry_with_changed_item_is_422_for_that_item() {
    let (app, state) = common::app().await;
    let p = common::project();
    let mut list = items(p, 3);
    let (_, first) = common::post(
        &app,
        "/api/v1/ingest/batch",
        json!({ "idempotencyKey": "B", "items": list.clone() }),
    )
    .await;
    list[1]["content"] = json!("changed");
    let (status, second) = common::post(
        &app,
        "/api/v1/ingest/batch",
        json!({ "idempotencyKey": "B", "items": list }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let r = &second["results"];
    assert_eq!(r[0]["status"], 200, "{second}");
    assert_eq!(
        r[0]["datapoint"]["id"],
        first["results"][0]["datapoint"]["id"]
    );
    assert_eq!(r[1]["status"], 422, "{second}");
    assert_eq!(r[1]["error"]["code"], "idempotency-key-reused");
    assert_eq!(r[2]["error"]["code"], "skipped");
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
    let fresh = muninn::app::Infra {
        index: std::sync::Arc::new(muninn::index::MemoryIndex::new()),
        ..state.clone()
    };
    let app2 = muninn::http::router(muninn::app::Services::new(fresh.clone()));
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

#[tokio::test]
#[ignore]
async fn source_id_creates_then_dedupes_identical_resend() {
    let (app, state) = common::app().await;
    let p = common::project();
    let body = json!({ "projectId": p, "sourceId": "crm-1", "label": "L", "content": "c" });
    let (s1, b1) = common::post(&app, "/api/v1/ingest", body.clone()).await;
    assert_eq!(s1, StatusCode::CREATED, "{b1}");
    assert_eq!(b1["sourceId"], "crm-1");
    assert_eq!(b1["replaced"], false);
    let id: Uuid = b1["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(id, muninn::datapoints::source_uuid(p, "crm-1"));

    let (s2, b2) = common::post(&app, "/api/v1/ingest", body).await;
    assert_eq!(s2, StatusCode::OK, "{b2}");
    assert_eq!(b2["deduplicated"], true);
    assert_eq!(b2["replaced"], false);
    assert_eq!(b2["id"], b1["id"]);
    assert_eq!(b2["timestamp"], b1["timestamp"], "stored time is kept");
    assert_eq!(common::index_len(&state, p).await, 1);
}

#[tokio::test]
#[ignore]
async fn source_id_resend_with_changes_replaces_and_keeps_omitted_fields() {
    let (app, state) = common::app().await;
    let p = common::project();
    let (_, b1) = common::post(
        &app,
        "/api/v1/ingest",
        json!({
            "projectId": p, "sourceId": "crm-1", "label": "L", "content": "old",
            "timestamp": "2026-01-01T00:00:00Z", "metadata": { "k": "v" }, "audience": "eng"
        }),
    )
    .await;
    let (status, b2) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "sourceId": "crm-1", "content": "new" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{b2}");
    assert_eq!(b2["replaced"], true);
    assert_eq!(b2["deduplicated"], false);
    assert_eq!(b2["id"], b1["id"]);
    assert_eq!(b2["label"], "L");
    assert_eq!(b2["timestamp"], "2026-01-01T00:00:00Z");
    assert_eq!(b2["metadata"], json!({ "k": "v" }));
    assert_eq!(b2["audience"], "eng");

    let id: Uuid = b1["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        state.store.get_content(p, id).await.unwrap().as_deref(),
        Some("new")
    );
    assert_eq!(common::index_len(&state, p).await, 1);

    let (_, b3) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "sourceId": "crm-1", "content": "new", "audience": "project" }),
    )
    .await;
    assert_eq!(b3["replaced"], true, "{b3}");
    assert_eq!(b3["audience"], json!(null));
}

#[tokio::test]
#[ignore]
async fn replaced_content_is_what_recall_finds() {
    let (app, _) = common::app().await;
    let p = common::project();
    for content in ["the old text", "the new text"] {
        common::post(
            &app,
            "/api/v1/ingest",
            json!({ "projectId": p, "sourceId": "doc", "label": "L", "content": content }),
        )
        .await;
    }
    let (status, body) = common::post(
        &app,
        "/api/v1/recall",
        json!({ "projectId": p, "query": "text" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let docs = body["results"].as_array().unwrap();
    assert_eq!(docs.len(), 1, "{body}");
    assert_eq!(docs[0]["content"], "the new text");
    assert_eq!(docs[0]["sourceId"], "doc");
}

#[tokio::test]
#[ignore]
async fn source_id_rules() {
    let (app, _) = common::app().await;
    let p = common::project();
    let cases = [
        // A new sourceId needs a label.
        json!({ "projectId": p, "sourceId": "new", "content": "c" }),
        json!({ "projectId": p, "sourceId": " ", "label": "L", "content": "c" }),
        json!({ "projectId": p, "sourceId": "s", "idempotencyKey": "k", "label": "L", "content": "c" }),
        json!({ "projectId": p, "label": "L", "content": "c", "audience": "Engineering" }),
    ];
    for case in cases {
        let (status, body) = common::post(&app, "/api/v1/ingest", case.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case} -> {body}");
        assert_eq!(body["error"]["code"], "validation", "{case}");
    }
}

#[tokio::test]
#[ignore]
async fn batch_with_source_ids_upserts_and_checks_labels_up_front() {
    let (app, state) = common::app().await;
    let p = common::project();
    let (status, body) = common::post(
        &app,
        "/api/v1/ingest/batch",
        json!({ "idempotencyKey": "B", "items": [
            { "projectId": p, "sourceId": "a", "label": "A", "content": "one" },
            { "projectId": p, "sourceId": "a", "content": "two" },
            { "projectId": p, "label": "keyed", "content": "three" }
        ] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let r = &body["results"];
    assert_eq!(r[0]["status"], 201, "{body}");
    assert_eq!(r[1]["status"], 200);
    assert_eq!(r[1]["replaced"], true);
    assert_eq!(r[1]["datapoint"]["label"], "A");
    assert_eq!(r[2]["status"], 201);
    assert_eq!(common::index_len(&state, p).await, 2);

    let (status, body) = common::post(
        &app,
        "/api/v1/ingest/batch",
        json!({ "items": [
            { "projectId": p, "sourceId": "a", "content": "fine, stored" },
            { "projectId": p, "sourceId": "unknown", "content": "no label" }
        ] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["index"], 1);
    let id = muninn::datapoints::source_uuid(p, "a");
    assert_eq!(
        state.store.get_content(p, id).await.unwrap().as_deref(),
        Some("two"),
        "nothing written"
    );
}
