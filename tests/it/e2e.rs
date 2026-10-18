//! End-to-end tests: the real binary, the real embedding model, LocalStack.
//!
//! ```sh
//! set -a; . ./.env.example; set +a
//! cargo test --test it e2e:: -- --ignored
//! ```
//!
//! Server logs go to `target/e2e-logs/`.

use crate::e2e_support::{Server, labels};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

/// Memories an agent might have collected in one project, in English and
/// Swedish.
const NOTES: &[(&str, &str)] = &[
    (
        "Storage decision",
        "We store all memories in S3 so there is no separate database to run. LocalStack emulates S3 in development.",
    ),
    (
        "Vector search",
        "Search uses an in-memory vector index that is rebuilt from S3 on startup. S3 Vectors is planned for production.",
    ),
    (
        "Postgres ruled out",
        "We decided against Postgres with pgvector to avoid running and backing up a database server.",
    ),
    (
        "Deploy owner",
        "Maria owns deployments. Ask her before changing the release pipeline.",
    ),
    (
        "Deploy notes location",
        "Deploymentanteckningarna ligger i Notion under Infra.",
    ),
    (
        "Kanelbullar recipe",
        "Kanelbullar: smör, socker, kanel och kardemumma. Grädda i 225 grader i åtta minuter.",
    ),
    (
        "Banana bread",
        "Banana bread: three ripe bananas, flour, eggs and butter. Bake for an hour at 175 degrees.",
    ),
    (
        "Auth postponed",
        "We postponed authentication. The service runs inside the private network for now.",
    ),
    (
        "Embedding model",
        "The default embedding model is multilingual-e5-small because it handles both Swedish and English.",
    ),
    ("Standup time", "Daily standup moved to 09:30 on weekdays."),
    (
        "On-call rotation",
        "On-call rotates weekly. Johan is on call this week and Aisha next week.",
    ),
    (
        "Batch limit",
        "Batch ingest is capped at 100 items. Larger batches get a batch-too-large error.",
    ),
    (
        "Office move",
        "Stockholmskontoret flyttar till Södermalm i december.",
    ),
    (
        "Coffee machine",
        "The coffee machine on floor 3 is broken, use the one in the kitchen.",
    ),
    (
        "Token budget",
        "Recall fits results into a token budget so agents can paste them straight into a prompt.",
    ),
    (
        "Midsommar",
        "Midsommar är en företagsledig dag och ingen förväntas vara online.",
    ),
    (
        "Acme renewal",
        "Acme Corp renewed their contract for two years and asked for single sign-on.",
    ),
    (
        "No external AI",
        "We dropped the OpenAI embeddings API. All embedding now happens locally with no external AI calls.",
    ),
];

/// A query and the label that must be among its top 3 results.
const QUERIES: &[(&str, &str)] = &[
    ("storage", "Storage decision"),
    ("where is our data persisted", "Storage decision"),
    ("why don't we run a database", "Postgres ruled out"),
    ("who owns deployments", "Deploy owner"),
    ("where are the deployment notes", "Deploy notes location"),
    ("kanelbullar", "Kanelbullar recipe"),
    ("cinnamon buns recipe", "Kanelbullar recipe"),
    ("vad hände med autentiseringen", "Auth postponed"),
    ("who is on call", "On-call rotation"),
    ("when is the standup meeting", "Standup time"),
    ("when does the office move", "Office move"),
    ("is midsummer a day off", "Midsommar"),
    ("which customer wants SSO", "Acme renewal"),
    ("do we call OpenAI", "No external AI"),
];

async fn seed(server: &Server, project: Uuid) {
    let items: Vec<Value> = NOTES
        .iter()
        .map(|(label, content)| json!({ "projectId": project, "label": label, "content": content }))
        .collect();
    let (status, resp) = server
        .post("/api/v1/ingest/batch", &json!({ "items": items }))
        .await;
    assert_eq!(status, 200, "{resp}");
}

#[tokio::test]
#[ignore]
async fn relevant_memories_rank_in_top_three() {
    let server = Server::start().await;
    let project = Uuid::now_v7();
    seed(&server, project).await;

    let mut misses = Vec::new();
    for (query, expected) in QUERIES {
        let resp = server
            .recall(json!({ "projectId": project, "query": query, "limit": 3 }))
            .await;
        let got = labels(&resp);
        if !got.iter().any(|l| l == expected) {
            misses.push(format!("{query:?}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(
        misses.is_empty(),
        "{} of {} queries missed:\n{}",
        misses.len(),
        QUERIES.len(),
        misses.join("\n")
    );
}

#[tokio::test]
#[ignore]
async fn recall_is_scoped_to_project() {
    let server = Server::start().await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    seed(&server, a).await;
    let resp = server
        .recall(json!({ "projectId": b, "query": "storage" }))
        .await;
    assert_eq!(resp["results"], json!([]));
}

#[tokio::test]
#[ignore]
async fn memories_and_idempotency_survive_restart() {
    let first = Server::start().await;
    let bucket = first.bucket.clone();
    let project = Uuid::now_v7();
    let request = json!({
        "idempotencyKey": "decision-1",
        "projectId": project,
        "label": "Storage decision",
        "content": "We store all memories in S3."
    });
    let original = first.ingest(request.clone()).await;
    first.stop();

    let second = Server::start_with(&bucket, None).await;
    let resp = second
        .recall(json!({ "projectId": project, "query": "storage" }))
        .await;
    assert_eq!(resp["results"][0]["id"], original["id"]);

    let (status, again) = second.post("/api/v1/ingest", &request).await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["deduplicated"], true);
    assert_eq!(again["id"], original["id"]);

    let (status, reused) = second
        .post(
            "/api/v1/ingest",
            &json!({
                "idempotencyKey": "decision-1",
                "projectId": project,
                "label": "Something else",
                "content": "Different content"
            }),
        )
        .await;
    assert_eq!(status, 422, "{reused}");
    assert_eq!(reused["error"]["code"], "idempotency-key-reused");
}

#[tokio::test]
#[ignore]
async fn time_window_and_query_timestamp() {
    let server = Server::start().await;
    let project = Uuid::now_v7();
    let now = Utc::now();
    for (label, days) in [("three days", 3), ("two days", 2), ("one day", 1)] {
        server
            .ingest(json!({
                "projectId": project,
                "label": label,
                "content": format!("Release notes from {label} ago"),
                "timestamp": now - Duration::days(days)
            }))
            .await;
    }
    let sorted = |v: &Value| {
        let mut l = labels(v);
        l.sort();
        l
    };

    let from = server
        .recall(json!({
            "projectId": project, "query": "release notes",
            "occurredFrom": now - Duration::days(2) - Duration::hours(1)
        }))
        .await;
    assert_eq!(sorted(&from), vec!["one day", "two days"]);

    let to = server
        .recall(json!({
            "projectId": project, "query": "release notes",
            "occurredTo": now - Duration::days(2) - Duration::hours(1)
        }))
        .await;
    assert_eq!(labels(&to), vec!["three days"]);

    let as_of = server
        .recall(json!({
            "projectId": project, "query": "release notes",
            "queryTimestamp": now - Duration::days(2) + Duration::hours(1)
        }))
        .await;
    assert_eq!(sorted(&as_of), vec!["three days", "two days"]);

    let (status, resp) = server
        .post(
            "/api/v1/recall",
            &json!({
                "projectId": project, "query": "release notes",
                "occurredFrom": now, "occurredTo": now - Duration::days(1)
            }),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(resp["error"]["code"], "recall-range-invalid");
}

#[tokio::test]
#[ignore]
async fn future_timestamps() {
    let server = Server::start().await;
    let project = Uuid::now_v7();
    let (status, resp) = server
        .post(
            "/api/v1/ingest",
            &json!({
                "projectId": project, "label": "later", "content": "From the future",
                "timestamp": Utc::now() + Duration::hours(1)
            }),
        )
        .await;
    assert_eq!(status, 400, "{resp}");
    assert_eq!(resp["error"]["code"], "validation");

    // A little clock skew is accepted. The memory becomes visible once its
    // timestamp has passed, or with a later queryTimestamp.
    let soon = Utc::now() + Duration::minutes(1);
    server
        .ingest(json!({
            "projectId": project, "label": "soon", "content": "Slightly ahead of the server clock",
            "timestamp": soon
        }))
        .await;
    let now = server
        .recall(json!({ "projectId": project, "query": "clock" }))
        .await;
    assert_eq!(labels(&now), Vec::<String>::new());
    let later = server
        .recall(json!({
            "projectId": project, "query": "clock",
            "queryTimestamp": soon + Duration::seconds(1)
        }))
        .await;
    assert_eq!(labels(&later), vec!["soon"]);
}

#[tokio::test]
#[ignore]
async fn budget_limit_and_sort() {
    let server = Server::start().await;
    let project = Uuid::now_v7();
    let now = Utc::now();
    server
        .ingest(json!({
            "projectId": project, "label": "long",
            "content": "Storage design notes. ".repeat(400),
            "timestamp": now - Duration::days(4)
        }))
        .await;
    for (label, days) in [("s3", 3), ("bucket", 2), ("localstack", 1)] {
        server
            .ingest(json!({
                "projectId": project, "label": label,
                "content": format!("Storage note about {label}."),
                "timestamp": now - Duration::days(days)
            }))
            .await;
    }

    let budget = server
        .recall(json!({ "projectId": project, "query": "storage design", "maxTokens": 300 }))
        .await;
    let results = budget["results"].as_array().unwrap();
    assert!(!labels(&budget).contains(&"long".to_string()));
    assert_eq!(results.len(), 3);
    let sum: u64 = results
        .iter()
        .map(|r| r["contentTokens"].as_u64().unwrap())
        .sum();
    assert_eq!(budget["usedTokens"].as_u64().unwrap(), sum);
    assert!(sum <= 300);
    assert_eq!(budget["budgetTokens"], 300);

    let limited = server
        .recall(json!({ "projectId": project, "query": "storage", "limit": 2 }))
        .await;
    assert_eq!(limited["results"].as_array().unwrap().len(), 2);

    let relevance = server
        .recall(json!({ "projectId": project, "query": "storage" }))
        .await;
    let r = relevance["results"].as_array().unwrap();
    assert_eq!(r.len(), 4);
    for (i, item) in r.iter().enumerate() {
        assert_eq!(item["rank"], i + 1);
    }
    for pair in r.windows(2) {
        assert!(pair[0]["score"].as_f64() >= pair[1]["score"].as_f64());
    }

    let chrono = server
        .recall(json!({ "projectId": project, "query": "storage", "sort": "chronological" }))
        .await;
    let c = chrono["results"].as_array().unwrap();
    assert_eq!(labels(&chrono), vec!["long", "s3", "bucket", "localstack"]);
    for item in c {
        let same = r.iter().find(|x| x["id"] == item["id"]).unwrap();
        assert_eq!(
            item["rank"], same["rank"],
            "rank kept for {}",
            item["label"]
        );
    }
}

#[tokio::test]
#[ignore]
async fn batch_retry_does_not_duplicate() {
    let server = Server::start().await;
    let project = Uuid::now_v7();
    let batch = json!({
        "idempotencyKey": "import-1",
        "items": [
            { "projectId": project, "label": "a", "content": "Storage note a" },
            { "projectId": project, "label": "b", "content": "Storage note b" },
            { "projectId": project, "label": "c", "content": "Storage note c" }
        ]
    });
    let (status, first) = server.post("/api/v1/ingest/batch", &batch).await;
    assert_eq!(status, 200, "{first}");
    let (status, second) = server.post("/api/v1/ingest/batch", &batch).await;
    assert_eq!(status, 200, "{second}");
    for (a, b) in first["results"]
        .as_array()
        .unwrap()
        .iter()
        .zip(second["results"].as_array().unwrap())
    {
        assert_eq!(a["status"], 201);
        assert_eq!(b["status"], 200);
        assert_eq!(b["deduplicated"], true);
        assert_eq!(a["datapoint"]["id"], b["datapoint"]["id"]);
    }
    let resp = server
        .recall(json!({ "projectId": project, "query": "storage note" }))
        .await;
    assert_eq!(resp["results"].as_array().unwrap().len(), 3);
}
