mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};

async fn ingest(app: &axum::Router, body: Value) -> Value {
    let (status, resp) = common::post(app, "/api/v1/ingest", body).await;
    assert_eq!(status, StatusCode::CREATED, "{resp}");
    resp
}

fn labels(body: &Value) -> Vec<String> {
    let mut labels: Vec<String> = body["datapoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["label"].as_str().unwrap().to_string())
        .collect();
    labels.sort();
    labels
}

#[tokio::test]
#[ignore]
async fn lists_a_project_page_by_page() {
    let (app, _) = common::app().await;
    let (p, other) = (common::project(), common::project());
    for i in 0..5 {
        ingest(
            &app,
            json!({ "projectId": p, "label": format!("dp{i}"), "content": format!("content {i}") }),
        )
        .await;
    }
    ingest(
        &app,
        json!({ "projectId": other, "label": "elsewhere", "content": "x" }),
    )
    .await;

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut path = format!("/api/v1/datapoints?projectId={p}&limit=2");
        if let Some(c) = &cursor {
            path.push_str(&format!("&cursor={c}"));
        }
        let (status, body) = common::get(&app, &path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        pages += 1;
        for d in body["datapoints"].as_array().unwrap() {
            assert!(d.get("content").is_none(), "content only on request");
            assert!(d["contentTokens"].as_u64().unwrap() > 0);
            assert_eq!(d["embeddingModel"], "fake");
            seen.push(d["label"].as_str().unwrap().to_string());
        }
        match body["nextCursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    seen.sort();
    assert_eq!(seen, ["dp0", "dp1", "dp2", "dp3", "dp4"]);
}

#[tokio::test]
#[ignore]
async fn filters() {
    let (app, _) = common::app().await;
    let p = common::project();
    ingest(
        &app,
        json!({ "projectId": p, "sourceId": "crm-1", "label": "Kickoff call", "content": "agenda",
                "timestamp": "2026-01-01T00:00:00Z", "audience": "sales" }),
    )
    .await;
    ingest(
        &app,
        json!({ "projectId": p, "label": "Infra decision", "content": "s3 vectors",
                "timestamp": "2026-06-01T00:00:00Z" }),
    )
    .await;
    ingest(
        &app,
        json!({ "projectId": p, "label": "Follow-up call", "content": "notes",
                "timestamp": "2026-09-01T00:00:00Z", "audience": "sales" }),
    )
    .await;

    let list = |query: &str| {
        let app = app.clone();
        let path = format!("/api/v1/datapoints?projectId={p}&{query}");
        async move {
            let (status, body) = common::get(&app, &path).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body
        }
    };

    assert_eq!(
        labels(&list("audience=sales").await),
        ["Follow-up call", "Kickoff call"]
    );
    assert_eq!(labels(&list("audience=project").await), ["Infra decision"]);
    assert_eq!(
        labels(&list("label=CALL").await),
        ["Follow-up call", "Kickoff call"]
    );
    assert_eq!(
        labels(&list("occurredFrom=2026-05-01T00:00:00Z&occurredTo=2026-07-01T00:00:00Z").await),
        ["Infra decision"]
    );
    assert_eq!(
        labels(&list("label=call&occurredFrom=2026-08-01T00:00:00Z").await),
        ["Follow-up call"]
    );

    let body = list("sourceId=crm-1&includeContent=true").await;
    assert_eq!(body["datapoints"][0]["content"], "agenda");
    assert_eq!(body["datapoints"][0]["sourceId"], "crm-1");
    assert!(body["nextCursor"].is_null());
    assert_eq!(
        labels(&list("sourceId=crm-1&audience=project").await).len(),
        0
    );
    assert_eq!(labels(&list("sourceId=never-stored").await).len(), 0);
}

#[tokio::test]
#[ignore]
async fn works_while_loading() {
    let (app, state) = common::app().await;
    let p = common::project();
    ingest(
        &app,
        json!({ "projectId": p, "label": "a", "content": "b" }),
    )
    .await;
    state
        .ready
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = common::get(&app, &format!("/api/v1/datapoints?projectId={p}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(labels(&body), ["a"]);
}

#[tokio::test]
#[ignore]
async fn list_validation() {
    let (app, _) = common::app().await;
    let p = common::project();
    for query in [
        String::new(),
        "projectId=nope".into(),
        format!("projectId={p}&limit=0"),
        format!("projectId={p}&limit=201"),
        format!("projectId={p}&cursor=nope"),
        format!("projectId={p}&audience=Sales"),
        format!("projectId={p}&label=%20"),
        format!("projectId={p}&sourceId="),
        format!("projectId={p}&occurredFrom=yesterday"),
    ] {
        let (status, body) = common::get(&app, &format!("/api/v1/datapoints?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["error"]["code"], "validation", "{query}");
    }
    let (status, body) = common::get(
        &app,
        &format!(
            "/api/v1/datapoints?projectId={p}&occurredFrom=2026-02-01T00:00:00Z&occurredTo=2026-01-01T00:00:00Z"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "recall-range-invalid");
}
