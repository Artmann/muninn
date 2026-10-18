mod common;

use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
#[ignore]
async fn delete_by_source_id_and_id() {
    let (app, state) = common::app().await;
    let p = common::project();
    common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "sourceId": "crm-1", "label": "a", "content": "first" }),
    )
    .await;
    let (_, unsourced) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "label": "b", "content": "second" }),
    )
    .await;
    let (_, kept) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "label": "c", "content": "third" }),
    )
    .await;
    assert_eq!(common::index_len(&state, p).await, 3);

    let (status, body) = common::post(
        &app,
        "/api/v1/delete",
        json!({ "projectId": p, "sourceIds": ["crm-1", "never-stored"], "ids": [unsourced["id"]] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let r = &body["results"];
    assert_eq!(r[0]["sourceId"], "crm-1");
    assert_eq!(r[0]["deleted"], true);
    assert_eq!(r[1]["deleted"], false);
    assert_eq!(r[2]["id"], unsourced["id"]);
    assert_eq!(r[2]["deleted"], true);
    assert!(r[2].get("sourceId").is_none());

    assert_eq!(common::index_len(&state, p).await, 1);
    let id = muninn::domain::source_uuid(p, "crm-1");
    assert!(state.store.get_meta(p, id).await.unwrap().is_none());
    assert!(state.store.get_content(p, id).await.unwrap().is_none());

    let (_, recall) = common::post(
        &app,
        "/api/v1/recall",
        json!({ "projectId": p, "query": "anything" }),
    )
    .await;
    let docs = recall["results"].as_array().unwrap();
    assert_eq!(docs.len(), 1, "{recall}");
    assert_eq!(docs[0]["id"], kept["id"]);

    // Deleting again is not an error, and the sourceId can be reused.
    let (_, again) = common::post(
        &app,
        "/api/v1/delete",
        json!({ "projectId": p, "sourceIds": ["crm-1"] }),
    )
    .await;
    assert_eq!(again["results"][0]["deleted"], false);
    let (status, _) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p, "sourceId": "crm-1", "label": "a", "content": "back" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
#[ignore]
async fn delete_is_scoped_to_project() {
    let (app, state) = common::app().await;
    let (p1, p2) = (common::project(), common::project());
    let (_, b) = common::post(
        &app,
        "/api/v1/ingest",
        json!({ "projectId": p1, "label": "a", "content": "c" }),
    )
    .await;
    let (_, body) = common::post(
        &app,
        "/api/v1/delete",
        json!({ "projectId": p2, "ids": [b["id"]] }),
    )
    .await;
    assert_eq!(body["results"][0]["deleted"], false);
    assert_eq!(common::index_len(&state, p1).await, 1);
}

#[tokio::test]
#[ignore]
async fn delete_validation() {
    let (app, _) = common::app().await;
    let p = common::project();
    let too_many: Vec<String> = (0..1001).map(|i| i.to_string()).collect();
    for case in [
        json!({ "projectId": p }),
        json!({ "projectId": p, "sourceIds": [] }),
        json!({ "projectId": p, "sourceIds": [""] }),
        json!({ "projectId": p, "sourceIds": too_many }),
        json!({ "projectId": p, "ids": ["not-a-uuid"] }),
    ] {
        let (status, body) = common::post(&app, "/api/v1/delete", case).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "validation");
    }
}
