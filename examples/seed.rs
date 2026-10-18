//! Seed a running Muninn with sample datapoints for trying out recall.
//!
//! ```sh
//! cargo run --example seed                     # http://localhost:8080
//! cargo run --example seed -- http://host:8080
//! ```
//!
//! Every item has a `sourceId`, so running it again updates the same
//! datapoints instead of adding copies.

use chrono::{Duration, Utc};
use serde_json::{Value, json};

/// The project all sample datapoints go into.
const PROJECT_ID: &str = "0192f0c0-5eed-7000-8000-000000000001";

/// (sourceId, label, days ago, audience, content)
const DATAPOINTS: &[(&str, &str, i64, Option<&str>, &str)] = &[
    (
        "seed:decision-s3-vectors",
        "decision",
        42,
        None,
        "We picked Amazon S3 Vectors for the search index over pgvector and OpenSearch. \
         It keeps S3 as the only storage dependency, needs no cluster to run, and costs \
         almost nothing at our volume. The in-memory index stays for local development.",
    ),
    (
        "seed:decision-local-embeddings",
        "decision",
        40,
        None,
        "Embeddings are computed locally with multilingual-e5-small through fastembed \
         instead of calling the OpenAI embeddings API. No API keys, no per-token cost, \
         and the data never leaves our infrastructure. Quality is good enough for recall.",
    ),
    (
        "seed:decision-rust",
        "decision",
        60,
        None,
        "The service is written in Rust with axum and tokio. The team already maintains \
         two Rust services, and a single static binary keeps the Docker image small.",
    ),
    (
        "seed:incident-2026-09-03",
        "incident",
        45,
        None,
        "Incident: recall returned empty results for about 20 minutes after a deploy. \
         The new container started serving before the embedding model finished loading. \
         Fix: /healthz now reports loading until the model is ready and the load balancer \
         waits for ok.",
    ),
    (
        "seed:incident-2026-09-21",
        "incident",
        27,
        None,
        "Incident: ingest requests failed with AccessDenied in eu-west-3. The IAM policy \
         was missing s3vectors:PutVectors on the new vector bucket. Error messages now \
         name the denied action and resource.",
    ),
    (
        "seed:meeting-2026-09-10",
        "meeting-note",
        38,
        None,
        "Planning meeting: agreed to ship batch ingest with up to 100 items per request. \
         Anna raised that Slack exports can be thousands of messages; clients will chunk \
         them. Delete by sourceId is next after batch.",
    ),
    (
        "seed:meeting-2026-09-24",
        "meeting-note",
        24,
        None,
        "Retro: the idempotency key design held up well during the AccessDenied incident, \
         retried batches did not create duplicates. Action item: document how sourceId \
         and idempotencyKey differ.",
    ),
    (
        "seed:meeting-2026-10-08",
        "meeting-note",
        10,
        None,
        "Roadmap sync: webhooks for new datapoints are in progress, with a per-webhook \
         rate limit. Multi-region replication is postponed to next quarter.",
    ),
    (
        "seed:customer-acme-onboarding",
        "customer",
        20,
        Some("sales"),
        "Acme Corp onboarding call: they want to feed support tickets from Zendesk into \
         Muninn and let their agents recall past resolutions. Main concern is data \
         residency; they need everything to stay in the EU.",
    ),
    (
        "seed:customer-acme-pricing",
        "customer",
        15,
        Some("sales"),
        "Acme Corp asked for a quote for 5 million datapoints. We offered the team plan \
         with a 20% discount for an annual contract. Decision expected by end of October.",
    ),
    (
        "seed:customer-globex-churn",
        "customer",
        8,
        Some("sales"),
        "Globex is considering leaving because recall latency spiked above 800 ms during \
         their peak hours. They are on the in-memory index with 2 million datapoints.",
    ),
    (
        "seed:runbook-reindex",
        "runbook",
        30,
        Some("ops"),
        "Runbook: to rebuild the S3 Vectors index after changing the embedding model, \
         set EMBED_MODEL to the new model and restart. A new index named after the model \
         is created and datapoints are re-embedded on boot. The old index can be deleted \
         once recall looks right.",
    ),
    (
        "seed:runbook-rotate-keys",
        "runbook",
        18,
        Some("ops"),
        "Runbook: rotate AWS credentials by creating a new access key for the muninn IAM \
         user, updating the secret in the deployment, redeploying, then deactivating the \
         old key after 24 hours.",
    ),
    (
        "seed:fact-token-budget",
        "fact",
        35,
        None,
        "Recall levels map to token budgets: low, medium and high. The response packs \
         the best-scoring datapoints into the budget so agents can paste them straight \
         into a prompt.",
    ),
    (
        "seed:fact-audiences",
        "fact",
        12,
        None,
        "Audiences are lowercase slugs of 2 to 40 characters. A datapoint without an \
         audience is visible to everyone in the project; the special audience 'project' \
         means the same thing.",
    ),
    (
        "seed:person-anna",
        "person",
        50,
        None,
        "Anna Lindqvist leads the platform team. She owns the storage layer and is the \
         person to ask about S3 bucket layout and IAM policies.",
    ),
    (
        "seed:person-ravi",
        "person",
        50,
        None,
        "Ravi Patel works on recall quality. He ran the embedding model comparison and \
         prefers bge-m3 for long documents but agreed e5-small is fine for now.",
    ),
    (
        "seed:preference-coffee",
        "preference",
        5,
        None,
        "The team prefers to hold planning meetings on Tuesday mornings, never on \
         Fridays. Ravi works from Bangalore, so meetings start before 11:00 CET.",
    ),
    (
        "seed:sv-beslut-backup",
        "decision",
        14,
        None,
        "Beslut: vi tar dagliga säkerhetskopior av S3-bucketen till ett annat konto med \
         versionering påslagen. Återställning ska testas en gång per kvartal.",
    ),
    (
        "seed:de-kunde-feedback",
        "customer",
        6,
        Some("sales"),
        "Kundenfeedback von Initech: Die Suche funktioniert auch auf Deutsch sehr gut. \
         Sie wünschen sich einen Export aller Datenpunkte als JSON.",
    ),
    (
        "seed:bug-timezone",
        "bug",
        3,
        None,
        "Bug: occurredFrom filters were off by one hour for clients sending local times \
         without an offset. Timestamps must be RFC 3339 with a timezone; the API now \
         rejects naive timestamps.",
    ),
    (
        "seed:idea-summaries",
        "idea",
        2,
        None,
        "Idea: nightly job that summarises each day's datapoints into one digest \
         datapoint, so recall over long periods returns fewer, denser results.",
    ),
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://localhost:8080".into());
    let base = base.trim_end_matches('/');
    let now = Utc::now();

    let items: Vec<Value> = DATAPOINTS
        .iter()
        .map(|(source_id, label, days_ago, audience, content)| {
            let mut item = json!({
                "projectId": PROJECT_ID,
                "sourceId": source_id,
                "label": label,
                "content": content,
                "timestamp": now - Duration::days(*days_ago),
                "metadata": { "seed": true },
            });
            if let Some(audience) = audience {
                item["audience"] = json!(audience);
            }
            item
        })
        .collect();

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/api/v1/ingest/batch"))
        .json(&json!({ "items": items }))
        .send()
        .await?;
    let status = response.status();
    let body: Value = response.json().await?;
    if !status.is_success() {
        anyhow::bail!("seeding failed with {status}: {body:#}");
    }

    let results = body["results"].as_array().cloned().unwrap_or_default();
    let failed: Vec<&Value> = results
        .iter()
        .filter(|r| r["status"].as_u64().is_none_or(|s| s >= 300))
        .collect();
    println!(
        "Seeded {} datapoints into project {PROJECT_ID}",
        results.len() - failed.len()
    );
    for failure in &failed {
        println!("  failed: {failure}");
    }
    println!(
        "\nTry:\n  curl -s {base}/api/v1/recall -H 'content-type: application/json' \\\n    \
         -d '{{\"projectId\":\"{PROJECT_ID}\",\"query\":\"why did we choose S3 Vectors\"}}'"
    );
    if !failed.is_empty() {
        anyhow::bail!("{} items failed", failed.len());
    }
    Ok(())
}
