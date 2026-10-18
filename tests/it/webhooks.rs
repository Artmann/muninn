use crate::common;
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

/// Create a webhook and return its id and event path.
async fn create(app: &axum::Router, project: Uuid, settings: Value) -> (String, String) {
    let mut body = json!({ "projectId": project });
    body.as_object_mut()
        .unwrap()
        .extend(settings.as_object().unwrap().clone());
    let (status, resp) = common::post(app, "/api/v1/webhooks", body).await;
    assert_eq!(status, StatusCode::CREATED, "{resp}");
    (
        resp["id"].as_str().unwrap().to_string(),
        resp["path"].as_str().unwrap().to_string(),
    )
}

async fn send(app: &axum::Router, path: &str, event: Value) -> (StatusCode, Value) {
    common::post_raw(app, path, "application/json", event.to_string()).await
}

#[tokio::test]
#[ignore]
async fn manage_webhooks() {
    let (app, _) = common::app().await;
    let (p, other) = (common::project(), common::project());
    let settings = json!({
        "name": "Jira",
        "contentPath": "$.issue.description",
        "filters": [{ "path": "$.webhookEvent", "op": "is", "value": "jira:issue_created" }]
    });
    let (id, path) = create(&app, p, settings.clone()).await;
    assert!(id.starts_with("whk_"));
    assert!(path.starts_with(&format!("/api/v1/webhooks/{id}/")));

    let (status, list) = common::get(&app, &format!("/api/v1/webhooks?projectId={p}")).await;
    assert_eq!(status, StatusCode::OK);
    let hooks = list["webhooks"].as_array().unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0]["name"], "Jira");
    assert_eq!(hooks[0]["filters"][0]["op"], "is");
    assert!(hooks[0].get("tokenSha256").is_none());
    assert!(!list.to_string().contains(path.rsplit('/').next().unwrap()));

    let (_, list) = common::get(&app, &format!("/api/v1/webhooks?projectId={other}")).await;
    assert_eq!(list["webhooks"], json!([]), "scoped to the project");
    let (status, _) = common::get(&app, &format!("/api/v1/webhooks/{id}?projectId={other}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, updated) = common::call(
        &app,
        "PUT",
        &format!("/api/v1/webhooks/{id}"),
        Some(json!({ "projectId": p, "name": "Jira 2", "contentPath": "$", "paused": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let (_, got) = common::get(&app, &format!("/api/v1/webhooks/{id}?projectId={p}")).await;
    assert_eq!(got["name"], "Jira 2");
    assert_eq!(got["paused"], true);
    assert_eq!(got["filters"], json!([]));

    let (status, resp) = send(&app, &path, json!({ "a": 1 })).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(resp["error"]["code"], "webhook-paused");

    let del = format!("/api/v1/webhooks/{id}?projectId={p}");
    let (status, resp) = common::call(&app, "DELETE", &del, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["deleted"], true);
    let (_, resp) = common::call(&app, "DELETE", &del, None).await;
    assert_eq!(resp["deleted"], false);
    let (status, _) = send(&app, &path, json!({ "a": 1 })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore]
async fn create_validation_and_limit() {
    let (app, _) = common::app().await;
    let p = common::project();
    for bad in [
        json!({ "projectId": p, "name": "x" }),
        json!({ "projectId": p, "name": "x", "contentPath": "$..a" }),
        json!({ "projectId": p, "name": "", "contentPath": "$" }),
        json!({ "projectId": p, "name": "x", "contentPath": "$", "filters": [{ "path": "$.a", "op": "is-any-of", "values": [] }] }),
    ] {
        let (status, body) = common::post(&app, "/api/v1/webhooks", bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} -> {body}");
        assert_eq!(body["error"]["code"], "validation");
    }
    let (status, _) = common::get(&app, "/api/v1/webhooks").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "projectId is required");

    for i in 0..muninn::webhooks::MAX_PER_PROJECT {
        create(
            &app,
            p,
            json!({ "name": format!("w{i}"), "contentPath": "$" }),
        )
        .await;
    }
    let (status, body) = common::post(
        &app,
        "/api/v1/webhooks",
        json!({ "projectId": p, "name": "one too many", "contentPath": "$" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
#[ignore]
async fn events_become_datapoints() {
    let (app, state) = common::app().await;
    let p = common::project();
    let (id, path) = create(
        &app,
        p,
        json!({
            "name": "Jira",
            "contentPath": "$.issue.description",
            "labelPath": "$.issue.summary",
            "occurredAtPath": "$.timestamp",
            "idPath": "$.issue.key",
            "audience": "support",
            "filters": [{ "path": "$.webhookEvent", "op": "is-any-of", "values": ["jira:issue_created", "jira:issue_updated"] }]
        }),
    )
    .await;
    let event = |kind: &str, description: &str| {
        json!({
            "webhookEvent": kind,
            "timestamp": 1_760_000_000_000_i64,
            "issue": { "key": "SUP-1042", "summary": "Login broken", "description": description }
        })
    };

    let (status, resp) = send(
        &app,
        &path,
        event("jira:issue_created", "Customer cannot log in"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
    assert_eq!(resp["verdict"], "ingested");
    assert_eq!(resp["droppedBy"], Value::Null);
    assert_eq!(resp["unchanged"], false);
    assert!(resp["eventId"].as_str().unwrap().starts_with("evt_"));
    let datapoint = resp["datapointId"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let source_id = format!("{id}:SUP-1042");
    assert_eq!(datapoint, muninn::datapoints::source_uuid(p, &source_id));

    let meta = state.store.get_meta(p, datapoint).await.unwrap().unwrap();
    assert_eq!(meta.label, "Login broken");
    assert_eq!(meta.audience.as_deref(), Some("support"));
    assert_eq!(meta.timestamp.timestamp(), 1_760_000_000);
    assert_eq!(meta.metadata["source"], "webhook");
    assert_eq!(meta.metadata["webhookId"], json!(id));

    let (status, resp) = send(
        &app,
        &path,
        event("jira:issue_created", "Customer cannot log in"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(resp["unchanged"], true);

    let (_, resp) = send(
        &app,
        &path,
        event("jira:issue_updated", "Fixed by resetting SSO"),
    )
    .await;
    assert_eq!(resp["unchanged"], false);
    assert_eq!(
        resp["datapointId"],
        json!(datapoint),
        "the update replaces it"
    );
    assert_eq!(
        state
            .store
            .get_content(p, datapoint)
            .await
            .unwrap()
            .as_deref(),
        Some("Fixed by resetting SSO")
    );
    assert_eq!(common::index_len(&state, p).await, 1);

    let (status, resp) = send(&app, &path, event("jira:issue_deleted", "x")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["verdict"], "dropped");
    assert_eq!(
        resp["droppedBy"],
        "$.webhookEvent is any of jira:issue_created, jira:issue_updated"
    );
    assert_eq!(resp["datapointId"], Value::Null);
    assert_eq!(common::index_len(&state, p).await, 1);

    // Deleting by sourceId works like any other datapoint.
    let (_, resp) = common::post(
        &app,
        "/api/v1/delete",
        json!({ "projectId": p, "sourceIds": [source_id] }),
    )
    .await;
    assert_eq!(resp["results"][0]["deleted"], true);
}

#[tokio::test]
#[ignore]
async fn events_without_an_id_dedupe_identical_resends() {
    let (app, state) = common::app().await;
    let p = common::project();
    let (_, path) = create(&app, p, json!({ "name": "Alerts", "contentPath": "$" })).await;

    let (status, first) = common::post_raw(&app, &path, "text/plain", "disk almost full").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let (_, again) = common::post_raw(&app, &path, "text/plain", "disk almost full").await;
    assert_eq!(again["unchanged"], true);
    assert_eq!(again["datapointId"], first["datapointId"]);
    let (_, other) = common::post_raw(&app, &path, "text/plain", "disk full").await;
    assert_ne!(other["datapointId"], first["datapointId"]);
    assert_eq!(common::index_len(&state, p).await, 2);

    let id: Uuid = first["datapointId"].as_str().unwrap().parse().unwrap();
    let meta = state.store.get_meta(p, id).await.unwrap().unwrap();
    assert_eq!(meta.label, "Alerts", "label defaults to the webhook's name");
    assert_eq!(
        state.store.get_content(p, id).await.unwrap().as_deref(),
        Some("disk almost full")
    );
}

#[tokio::test]
#[ignore]
async fn refusals() {
    let (app, _) = common::app().await;
    let p = common::project();
    let (id, path) = create(&app, p, json!({ "name": "w", "contentPath": "$" })).await;

    for wrong in [
        format!("/api/v1/webhooks/{id}/{}", "0".repeat(32)),
        format!("/api/v1/webhooks/whk_{}/x", "0".repeat(32)),
        "/api/v1/webhooks/..%2F..%2Fx/y".to_string(),
    ] {
        let (status, body) = send(&app, &wrong, json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{wrong}");
        assert_eq!(body["error"]["code"], "not-found");
    }

    let big = vec![b'a'; muninn::webhooks::MAX_BODY_BYTES + 1];
    let (status, body) = common::post_raw(&app, &path, "text/plain", big).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error"]["code"], "webhook-body-too-large");

    let (status, _) = common::post_raw(&app, &path, "text/plain", "ok").await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
#[ignore]
async fn rate_limited_per_webhook() {
    let (app, _) = common::app().await;
    let p = common::project();
    let (_, path) = create(
        &app,
        p,
        json!({ "name": "w", "contentPath": "$.a", "filters": [{ "path": "$.drop", "op": "exists" }] }),
    )
    .await;
    // Dropped events are cheap and still count.
    for _ in 0..muninn::webhooks::EVENTS_PER_MINUTE {
        let (status, _) = send(&app, &path, json!({ "a": "x" })).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, body) = send(&app, &path, json!({ "a": "x" })).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["code"], "rate-limited");

    let (_, other) = create(&app, p, json!({ "name": "w2", "contentPath": "$" })).await;
    let (status, _) = send(&app, &other, json!({ "a": "x" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "limits are per webhook");
}
