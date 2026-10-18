use crate::common;
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

async fn ingest(app: &axum::Router, body: Value) -> Value {
    let (status, resp) = common::post(app, "/api/v1/ingest", body).await;
    assert_eq!(status, StatusCode::CREATED, "{resp}");
    resp
}

async fn recall(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    common::post(app, "/api/v1/recall", body).await
}

fn labels(body: &Value) -> Vec<String> {
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
#[ignore]
async fn recall_returns_most_similar_first() {
    let (app, _) = common::app().await;
    let p = common::project();
    for (label, content) in [
        ("rust", "rust memory service"),
        ("bread", "banana bread recipe"),
        ("pg", "postgres index tuning"),
    ] {
        ingest(
            &app,
            json!({ "projectId": p, "label": label, "content": content }),
        )
        .await;
    }
    let (status, body) = recall(
        &app,
        json!({ "projectId": p, "query": "memory service in rust" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let r = &body["results"];
    assert_eq!(r[0]["label"], "rust");
    assert_eq!(r[0]["rank"], 1);
    assert_eq!(r[0]["content"], "rust memory service");
    assert!(r[0]["score"].as_f64().unwrap() > r[1]["score"].as_f64().unwrap());
    assert!(r[0]["contentTokens"].as_u64().unwrap() > 0);
    assert_eq!(body["budgetTokens"], 8000);
}

#[tokio::test]
#[ignore]
async fn recall_scopes_to_project() {
    let (app, _) = common::app().await;
    let (p1, p2) = (common::project(), common::project());
    ingest(
        &app,
        json!({ "projectId": p1, "label": "one", "content": "shared words" }),
    )
    .await;
    ingest(
        &app,
        json!({ "projectId": p2, "label": "two", "content": "shared words" }),
    )
    .await;
    let (_, body) = recall(&app, json!({ "projectId": p1, "query": "shared words" })).await;
    assert_eq!(labels(&body), vec!["one"]);
    let (status, empty) = recall(
        &app,
        json!({ "projectId": Uuid::now_v7(), "query": "shared" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["results"], json!([]));
    assert_eq!(empty["usedTokens"], 0);
}

#[tokio::test]
#[ignore]
async fn recall_respects_time_window_and_query_timestamp() {
    let (app, _) = common::app().await;
    let p = common::project();
    let t = Utc::now() - Duration::hours(1);
    for (label, at) in [
        ("two-days", t - Duration::days(2)),
        ("one-day", t - Duration::days(1)),
        ("now", t),
    ] {
        ingest(
            &app,
            json!({ "projectId": p, "label": label, "content": "note", "timestamp": at }),
        )
        .await;
    }
    let (_, from) = recall(
        &app,
        json!({ "projectId": p, "query": "note", "occurredFrom": t - Duration::days(1) }),
    )
    .await;
    let mut got = labels(&from);
    got.sort();
    assert_eq!(got, vec!["now", "one-day"]);

    let (_, asof) = recall(
        &app,
        json!({ "projectId": p, "query": "note", "queryTimestamp": t - Duration::days(1) }),
    )
    .await;
    let mut got = labels(&asof);
    got.sort();
    assert_eq!(got, vec!["one-day", "two-days"]);
}

#[tokio::test]
#[ignore]
async fn recall_chronological_keeps_rank() {
    let (app, _) = common::app().await;
    let p = common::project();
    let t = Utc::now() - Duration::hours(1);
    ingest(
        &app,
        json!({ "projectId": p, "label": "newer-match", "content": "alpha beta gamma", "timestamp": t }),
    )
    .await;
    ingest(
        &app,
        json!({
            "projectId": p,
            "label": "older-weak",
            "content": "alpha unrelated words here",
            "timestamp": t - Duration::days(1)
        }),
    )
    .await;
    let (_, body) = recall(
        &app,
        json!({ "projectId": p, "query": "alpha beta gamma", "sort": "chronological" }),
    )
    .await;
    assert_eq!(labels(&body), vec!["older-weak", "newer-match"]);
    assert_eq!(body["results"][0]["rank"], 2);
    assert_eq!(body["results"][1]["rank"], 1);
}

#[tokio::test]
#[ignore]
async fn recall_budget_omits_whole_documents() {
    let (app, _) = common::app().await;
    let p = common::project();
    let long = "memory ".repeat(200);
    ingest(
        &app,
        json!({ "projectId": p, "label": "long", "content": long }),
    )
    .await;
    ingest(
        &app,
        json!({ "projectId": p, "label": "short", "content": "memory note" }),
    )
    .await;
    let (status, body) = recall(
        &app,
        json!({ "projectId": p, "query": "memory", "maxTokens": 50 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(labels(&body), vec!["short"]);
    assert_eq!(body["usedTokens"], body["results"][0]["contentTokens"]);
    assert_eq!(body["budgetTokens"], 50);
}

#[tokio::test]
#[ignore]
async fn recall_errors() {
    let (app, state) = common::app().await;
    let p = common::project();
    let cases = [
        (
            json!({ "projectId": p, "query": "q", "level": "low", "maxTokens": 10 }),
            "budget-conflict",
        ),
        (
            json!({
                "projectId": p,
                "query": "q",
                "occurredFrom": "2026-10-02T00:00:00Z",
                "occurredTo": "2026-10-01T00:00:00Z"
            }),
            "recall-range-invalid",
        ),
        (
            json!({ "projectId": p, "query": "q", "limit": 0 }),
            "validation",
        ),
        (
            json!({ "projectId": p, "query": "q", "level": "huge" }),
            "validation",
        ),
        (json!({ "projectId": p }), "validation"),
    ];
    for (body, code) in cases {
        let (status, resp) = recall(&app, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {resp}");
        assert_eq!(resp["error"]["code"], code, "{body}");
    }
    state
        .ready
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (status, _) = recall(&app, json!({ "projectId": p, "query": "q" })).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
#[ignore]
async fn recall_skips_datapoint_with_missing_content() {
    let (app, state) = common::app().await;
    let p = common::project();
    let kept = ingest(
        &app,
        json!({ "projectId": p, "label": "kept", "content": "shared words" }),
    )
    .await;
    // An orphan: meta.json and an index entry, but no content object.
    let mut orphan = state
        .store
        .get_meta(p, kept["id"].as_str().unwrap().parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    orphan.id = Uuid::now_v7();
    orphan.label = "orphan".into();
    state.store.put_meta(&orphan).await.unwrap();
    state
        .index
        .upsert(p, orphan.id, orphan.embedding.vector.clone())
        .await
        .unwrap();

    let (status, body) = recall(&app, json!({ "projectId": p, "query": "shared words" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(labels(&body), vec!["kept"]);
    assert_eq!(body["usedTokens"], body["results"][0]["contentTokens"]);
}
