# Muninn memory service Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust HTTP service with `POST /api/v1/ingest`, `POST /api/v1/ingest/batch` and `POST /api/v1/recall` that stores datapoints in S3, embeds them with a local multilingual model, and recalls them by semantic similarity within a token budget.

**Architecture:** axum handlers call three pluggable pieces: an `Embedder` trait (fastembed in prod, a deterministic fake in tests), a `VectorIndex` trait (in-memory brute force in v1), and a concrete `S3Store` for content, metadata and idempotency markers. The recall selection algorithm is a pure function over already-loaded metadata so it is unit-tested without S3. On startup the index is rebuilt by scanning `meta.json` objects in the bucket.

**Tech Stack:** Rust 2024, axum 0.8, tokio, serde/serde_json, aws-sdk-s3 1.x + aws-config, fastembed 7, tiktoken-rs 0.12, uuid 1 (v7), chrono 0.4, sha2, async-trait, thiserror/anyhow, tracing. LocalStack via docker compose for S3.

**Spec:** `docs/superpowers/specs/2026-10-17-memory-service-design.md`

## Global Constraints

- Edition 2024. `cargo fmt` and `cargo clippy -- -D warnings` clean at every commit.
- Commit messages: short imperative subject. No `Co-Authored-By` lines.
- All JSON fields are camelCase on the wire (`#[serde(rename_all = "camelCase")]`).
- Error envelope is exactly `{ "error": { "code": "...", "message": "..." } }`, optional `details` inside `error`.
- Error codes: `validation`, `recall-range-invalid`, `budget-conflict`, `batch-too-large`, `loading`, `internal`.
- Config env vars: `BUCKET` (default `muninn`), `BIND` (default `0.0.0.0:8080`), `EMBED_MODEL` (default `multilingual-e5-small`), `MODEL_CACHE` (default `./.fastembed_cache`), `AWS_ENDPOINT_URL` (path-style when set).
- Model config names and dims: `multilingual-e5-small` 384, `multilingual-e5-base` 768, `bge-m3` 1024, `bge-small-en-v1.5` 384.
- Budget levels: `low` 8000, `medium` 16000, `high` 24000, `extra-high` 32000. `limit` default 100, range 1..=200. Batch max 100 items.
- S3 keys: `projects/{projectId}/datapoints/{id}/content`, `projects/{projectId}/datapoints/{id}/meta.json`, `projects/{projectId}/idempotency/{sha256hex(key)}`.
- Timestamps are RFC 3339; stored and compared in UTC; returned as UTC with `Z`.
- Token counting: `cl100k_base` over content only.
- Vectors are L2-normalised before storage and before search.
- Integration tests that need LocalStack are `#[ignore]` and run with `cargo test -- --ignored`.

## Review Focus

1. Whitespace-only `label` or `content` must be rejected as `400 validation`, not stored as an empty memory. Test in Task 2.
2. `metadata` that is a JSON array or string must be `400 validation` with a readable message, not a 422 or a 500. Test in Task 8.
3. `maxTokens` smaller than every candidate document must return `200` with empty `results` and `usedTokens: 0`, not an error. Test in Task 7.
4. A `timestamp` with a non-UTC offset such as `2026-10-17T14:00:00+02:00` must compare correctly against UTC filters and be returned as `2026-10-17T12:00:00Z`. Test in Task 2.
5. Re-sending an idempotency key with a different body must return `200 deduplicated: true` with the original body, never the new one. Test in Task 8.

---

### Task 1: Scaffold, config, error envelope, healthz

**Files:**
- Modify: `Cargo.toml`
- Create: `src/lib.rs`, `src/main.rs`, `src/config.rs`, `src/api/mod.rs`, `src/api/error.rs`, `docker-compose.yml`, `.env.example`
- Test: `src/config.rs` (unit), `src/api/error.rs` (unit)

**Interfaces:**
- Produces:
  - `config::Config { bucket: String, bind: SocketAddr, embed_model: String, model_cache: PathBuf, endpoint_url: Option<String> }` and `Config::from_env() -> anyhow::Result<Config>` (reads `std::env`, applies defaults above; `embed_model` stays a string here and is parsed in Task 4).
  - `api::error::ApiError` enum: `Validation(String)`, `ValidationDetailed { message: String, details: Vec<ItemError> }`, `RecallRangeInvalid`, `BudgetConflict`, `BatchTooLarge`, `Loading`, `Internal(anyhow::Error)`; `ItemError { index: usize, message: String }`; `impl IntoResponse for ApiError`; `impl From<anyhow::Error> for ApiError` (→ Internal); `impl From<axum::extract::rejection::JsonRejection> for ApiError` (→ Validation with the rejection's body text).
  - `ApiError::code(&self) -> &'static str` and `ApiError::status(&self) -> StatusCode` per the Global Constraints table; `Internal` logs the error with `tracing::error!` and returns message `"internal error"`.
  - `api::Json<T>`: `#[derive(FromRequest)] #[from_request(via(axum::Json), rejection(ApiError))] pub struct Json<T>(pub T);`
  - `api::health_router() -> Router<()>` serving `GET /healthz` → `{"status":"ok"}` for now (readiness flag arrives in Task 11).

- [ ] **Step 1: Add dependencies to `Cargo.toml`**

```toml
[dependencies]
anyhow = "1"
async-trait = "0.1"
aws-config = { version = "1", features = ["behavior-version-latest"] }
aws-sdk-s3 = "1"
aws-smithy-runtime-api = "1"
axum = { version = "0.8", features = ["macros"] }
chrono = { version = "0.4", features = ["serde"] }
fastembed = "7"
futures = "0.3"
hex = "0.4"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.11"
thiserror = "2"
tiktoken-rs = "0.12"
tokio = { version = "1", features = ["full"] }
tower-http = { version = "0.7", features = ["trace", "limit"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
uuid = { version = "1", features = ["v7", "serde"] }

[dev-dependencies]
http-body-util = "0.1"
tower = { version = "0.5", features = ["util"] }
```

- [ ] **Step 2: Write the failing config test in `src/config.rs`**

```rust
#[test]
fn defaults_apply_when_env_unset() {
    let c = Config::from_map(&HashMap::new()).unwrap();
    assert_eq!(c.bucket, "muninn");
    assert_eq!(c.bind.to_string(), "0.0.0.0:8080");
    assert_eq!(c.embed_model, "multilingual-e5-small");
    assert_eq!(c.model_cache, PathBuf::from("./.fastembed_cache"));
    assert_eq!(c.endpoint_url, None);
}
#[test]
fn env_overrides() {
    let m = HashMap::from([("BUCKET".into(), "b".into()), ("BIND".into(), "127.0.0.1:9000".into()), ("AWS_ENDPOINT_URL".into(), "http://localhost:4566".into())]);
    let c = Config::from_map(&m).unwrap();
    assert_eq!(c.bucket, "b");
    assert_eq!(c.bind.port(), 9000);
    assert_eq!(c.endpoint_url.as_deref(), Some("http://localhost:4566"));
}
#[test]
fn bad_bind_is_error() {
    assert!(Config::from_map(&HashMap::from([("BIND".into(), "nope".into())])).is_err());
}
```

- [ ] **Step 3: Run `cargo test config` and confirm it fails to compile**

- [ ] **Step 4: Implement `Config`, `Config::from_map(&HashMap<String,String>) -> anyhow::Result<Config>` and `Config::from_env()` (collects `std::env::vars()` and calls `from_map`) in `src/config.rs`**

- [ ] **Step 5: Write the failing error-envelope test in `src/api/error.rs`**

```rust
#[tokio::test]
async fn envelope_shape_and_status() {
    let resp = ApiError::RecallRangeInvalid.into_response();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"]["code"], "recall-range-invalid");
    assert!(body["error"]["message"].is_string());
}
#[tokio::test]
async fn internal_hides_details() {
    let resp = ApiError::Internal(anyhow::anyhow!("secret s3 thing")).into_response();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body: serde_json::Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"]["message"], "internal error");
}
#[tokio::test]
async fn detailed_validation_includes_details() {
    let e = ApiError::ValidationDetailed { message: "bad items".into(), details: vec![ItemError { index: 2, message: "label empty".into() }] };
    let body: serde_json::Value = /* as above */;
    assert_eq!(body["error"]["details"][0]["index"], 2);
}
```

- [ ] **Step 6: Run `cargo test error` and confirm failure**

- [ ] **Step 7: Implement `ApiError`, `ItemError`, `IntoResponse`, the `From` impls and the `Json` extractor in `src/api/error.rs` and `src/api/mod.rs`**

- [ ] **Step 8: Write `src/lib.rs` declaring `pub mod api; pub mod config;` and `src/main.rs` that initialises `tracing_subscriber` with `EnvFilter`, loads `Config::from_env()`, and serves `api::health_router()` on `config.bind`**

- [ ] **Step 9: Write `docker-compose.yml` and `.env.example`**

`docker-compose.yml`: service `localstack`, image `localstack/localstack:latest`, ports `4566:4566`, environment `SERVICES=s3`.
`.env.example`: `AWS_ENDPOINT_URL=http://localhost:4566`, `AWS_ACCESS_KEY_ID=test`, `AWS_SECRET_ACCESS_KEY=test`, `AWS_REGION=us-east-1`.

- [ ] **Step 10: Run `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`; all pass. Then `cargo run` and `curl localhost:8080/healthz` returns `{"status":"ok"}`**

- [ ] **Step 11: Commit**

```bash
git add -A
git commit -m "Scaffold service with config, error envelope and healthz"
```

---

### Task 2: Domain types and validation

**Files:**
- Create: `src/domain.rs`
- Modify: `src/lib.rs` (add `pub mod domain;`)
- Test: `src/domain.rs` (unit)

**Interfaces:**
- Produces:
  - `NewDatapoint` (Deserialize, camelCase, `deny_unknown_fields` NOT set): `project_id: Uuid, label: String, content: String, timestamp: Option<DateTime<Utc>>, metadata: Option<serde_json::Map<String, Value>>, audience: Option<String>`.
  - `NewDatapoint::validate(&self) -> Result<(), String>`: error if `label.trim()` or `content.trim()` is empty.
  - `Datapoint` (Serialize, camelCase): `id: Uuid, project_id: Uuid, label, content, timestamp: DateTime<Utc>, metadata: Map, audience: Option<String>`.
  - `StoredEmbedding { model: String, dim: usize, vector: Vec<f32> }` (Serialize/Deserialize).
  - `Meta` (Serialize/Deserialize, camelCase): `id, project_id, label, timestamp, metadata, audience, embedding: StoredEmbedding, content_tokens: usize`.
  - `Meta::into_datapoint(self, content: String) -> Datapoint`.
  - `NewDatapoint::into_parts(self, id: Uuid, now: DateTime<Utc>) -> Datapoint` applying defaults: timestamp `now`, metadata `{}`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn rejects_whitespace_label() { let n = base(); let n = NewDatapoint { label: "  ".into(), ..n }; assert!(n.validate().is_err()); }
#[test] fn rejects_whitespace_content() { /* same for content */ }
#[test] fn defaults_timestamp_and_metadata() {
    let now = Utc::now(); let d = base().into_parts(Uuid::now_v7(), now);
    assert_eq!(d.timestamp, now); assert!(d.metadata.is_empty());
}
#[test] fn offset_timestamp_normalises_to_utc() {
    let n: NewDatapoint = serde_json::from_str(r#"{"projectId":"019a0000-0000-7000-8000-000000000000","label":"a","content":"b","timestamp":"2026-10-17T14:00:00+02:00"}"#).unwrap();
    let d = n.into_parts(Uuid::now_v7(), Utc::now());
    assert_eq!(serde_json::to_value(&d)["timestamp"], "2026-10-17T12:00:00Z");
}
#[test] fn metadata_must_be_object() {
    let r = serde_json::from_str::<NewDatapoint>(r#"{"projectId":"019a0000-0000-7000-8000-000000000000","label":"a","content":"b","metadata":[1]}"#);
    assert!(r.is_err());
}
#[test] fn meta_roundtrip_json() { /* serialize a Meta, deserialize, assert_eq on fields incl. embedding.vector */ }
```

Use chrono's default serde for `DateTime<Utc>`, which emits RFC 3339 with `Z`. Do not use a custom timestamp serializer.

- [ ] **Step 2: Run `cargo test domain`, confirm failure**

- [ ] **Step 3: Implement the types and methods in `src/domain.rs`**

- [ ] **Step 4: Run `cargo test domain`, confirm pass; clippy and fmt clean**

- [ ] **Step 5: Commit** `git commit -am "Add datapoint domain types and validation"`

---

### Task 3: Token counting

**Files:**
- Create: `src/tokens.rs`
- Modify: `src/lib.rs`
- Test: `src/tokens.rs` (unit)

**Interfaces:**
- Produces: `tokens::count_tokens(text: &str) -> usize` using `tiktoken_rs::cl100k_base_singleton()` and `encode_with_special_tokens(text).len()`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn empty_is_zero() { assert_eq!(count_tokens(""), 0); }
#[test] fn hello_world_is_two() { assert_eq!(count_tokens("hello world"), 2); }
#[test] fn multilingual_counts_positive() { assert!(count_tokens("Hej världen, hur mår du?") > 2); }
```

- [ ] **Step 2: Run `cargo test tokens`, confirm failure**
- [ ] **Step 3: Implement `count_tokens`**
- [ ] **Step 4: Run tests, clippy, fmt; pass**
- [ ] **Step 5: Commit** `git commit -am "Add cl100k token counting"`

---

### Task 4: Embedder trait, model choice, input construction, fake embedder

**Files:**
- Create: `src/embed.rs`
- Modify: `src/lib.rs`, `src/config.rs` (change `embed_model: String` to `embed_model: ModelChoice`, parse in `from_map`, error on unknown name)
- Test: `src/embed.rs` (unit)

**Interfaces:**
- Produces:
  - `ModelChoice` enum `{ MultilingualE5Small, MultilingualE5Base, BgeM3, BgeSmallEnV15 }` with `impl FromStr` (config names from Global Constraints, exact match), `fn name(&self) -> &'static str` (the config name), `fn dim(&self) -> usize`, `fn fastembed_model(&self) -> fastembed::EmbeddingModel` (`MultilingualE5Small`, `MultilingualE5Base`, `BGEM3`, `BGESmallENV15`), `fn query_prefix(&self) -> &'static str` (`"query: "` for the two E5 models, `""` otherwise), `fn passage_prefix(&self) -> &'static str` (`"passage: "` for E5, `""` otherwise).
  - `embed::build_input(label: &str, content: &str, metadata: &Map<String, Value>) -> String`: label, newline, content, then one `key: value` line per entry with keys sorted; string values raw, other values `serde_json::to_string` compact.
  - `embed::normalize(v: &mut [f32])` L2; leaves an all-zero vector untouched.
  - `trait Embedder: Send + Sync { fn model_name(&self) -> &str; fn dim(&self) -> usize; fn embed_passages(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>; fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>>; }` Both apply the prefix and normalise. Blocking; callers use `spawn_blocking`.
  - `FastEmbedder::new(choice: ModelChoice, cache_dir: &Path) -> anyhow::Result<FastEmbedder>` wrapping `TextEmbedding::try_new(InitOptions::new(choice.fastembed_model()).with_cache_dir(cache_dir.to_path_buf()).with_show_download_progress(false))` in a `std::sync::Mutex` (embed takes `&mut self`).
  - `FakeEmbedder::new(dim: usize) -> FakeEmbedder` with `model_name() == "fake"`: bag-of-words hashing, each lowercase whitespace-split word hashed (std `DefaultHasher`) into bucket `hash % dim`, count incremented, then normalised. Deterministic; shared words give higher cosine. Always compiled (used by `tests/`).

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn parses_config_names() { assert_eq!("bge-m3".parse::<ModelChoice>().unwrap(), ModelChoice::BgeM3); assert!("nope".parse::<ModelChoice>().is_err()); }
#[test] fn dims_match_spec() { assert_eq!(ModelChoice::MultilingualE5Small.dim(), 384); assert_eq!(ModelChoice::BgeM3.dim(), 1024); }
#[test] fn build_input_orders_metadata() {
    let md: Map<_, _> = serde_json::from_str(r#"{"z":"last","a":{"n":1},"m":"mid"}"#).unwrap();
    assert_eq!(build_input("L", "C", &md), "L\nC\na: {\"n\":1}\nm: mid\nz: last");
}
#[test] fn build_input_no_metadata_has_no_trailing_newline() { assert_eq!(build_input("L", "C", &Map::new()), "L\nC"); }
#[test] fn fake_is_deterministic_and_unit_length() {
    let e = FakeEmbedder::new(16);
    let a = e.embed_query("hello world").unwrap(); let b = e.embed_query("hello world").unwrap();
    assert_eq!(a, b); assert!((a.iter().map(|x| x*x).sum::<f32>() - 1.0).abs() < 1e-5);
}
#[test] fn fake_similar_texts_score_higher() {
    let e = FakeEmbedder::new(64);
    let q = e.embed_query("rust memory service").unwrap();
    let near = e.embed_passages(&["we built a memory service in rust".into()]).unwrap().remove(0);
    let far = e.embed_passages(&["banana bread recipe".into()]).unwrap().remove(0);
    assert!(dot(&q, &near) > dot(&q, &far));
}
#[test] fn e5_prefixes() { assert_eq!(ModelChoice::MultilingualE5Small.query_prefix(), "query: "); assert_eq!(ModelChoice::BgeM3.passage_prefix(), ""); }
```

- [ ] **Step 2: Run `cargo test embed`, confirm failure**
- [ ] **Step 3: Implement `src/embed.rs`; update `Config` to hold `ModelChoice` and fix the Task 1 config tests to compare `c.embed_model.name()`**
- [ ] **Step 4: Run all tests, clippy, fmt; pass**
- [ ] **Step 5: Commit** `git commit -am "Add embedder trait with fastembed and fake implementations"`

---

### Task 5: Vector index trait and in-memory implementation

**Files:**
- Create: `src/index/mod.rs`, `src/index/memory.rs`
- Modify: `src/lib.rs`
- Test: `src/index/memory.rs` (unit)

**Interfaces:**
- Produces:
  - `#[async_trait] pub trait VectorIndex: Send + Sync { async fn upsert(&self, project: Uuid, id: Uuid, vector: Vec<f32>) -> anyhow::Result<()>; async fn search(&self, project: Uuid, query: &[f32], k: usize) -> anyhow::Result<Vec<(Uuid, f32)>>; }` Results sorted by score desc, at most `k`. Upsert with an existing id replaces its vector.
  - `MemoryIndex::new() -> MemoryIndex` backed by `RwLock<HashMap<Uuid, Vec<(Uuid, Vec<f32>)>>>` (std or tokio RwLock, builder's choice). `MemoryIndex::len(&self, project: Uuid) -> usize`.

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test] async fn search_orders_by_dot_product_and_caps_k() { /* 3 vectors in one project, query equal to one of them, assert first id and len()==2 with k=2 */ }
#[tokio::test] async fn projects_are_isolated() { /* insert in p1, search in p2 returns empty */ }
#[tokio::test] async fn upsert_replaces_existing_id() { /* upsert same id twice, len()==1, search returns new vector's score */ }
#[tokio::test] async fn empty_project_returns_empty() { }
```

- [ ] **Step 2: Run `cargo test index`, confirm failure**
- [ ] **Step 3: Implement trait and `MemoryIndex`**
- [ ] **Step 4: Tests, clippy, fmt pass**
- [ ] **Step 5: Commit** `git commit -am "Add vector index trait and in-memory implementation"`

---

### Task 6: S3 store

**Files:**
- Create: `src/store.rs`, `tests/common/mod.rs`, `tests/store.rs`
- Modify: `src/lib.rs`
- Test: `src/store.rs` (unit for key functions), `tests/store.rs` (ignored, LocalStack)

**Interfaces:**
- Produces:
  - `store::keys::content(project: Uuid, id: Uuid) -> String`, `keys::meta(project, id) -> String`, `keys::marker(project: Uuid, key: &str) -> String` (sha256 hex of the raw key string), `keys::parse_meta_key(key: &str) -> Option<(Uuid, Uuid)>`.
  - `S3Store::new(client: aws_sdk_s3::Client, bucket: String) -> S3Store` (Clone).
  - `S3Store::from_config(cfg: &Config) -> anyhow::Result<S3Store>`: `aws_config::load_defaults(BehaviorVersion::latest())`, then `aws_sdk_s3::config::Builder::from(&sdk)`, `.endpoint_url(url)` and `.force_path_style(true)` when `cfg.endpoint_url` is set.
  - `async fn ensure_bucket(&self) -> anyhow::Result<()>`: `head_bucket`, on error `create_bucket`; log and ignore a create failure that is a permission error.
  - `async fn put_content(&self, project, id, content: &str) -> anyhow::Result<()>` (content-type `text/plain; charset=utf-8`).
  - `async fn get_content(&self, project, id) -> anyhow::Result<Option<String>>` (`NoSuchKey` → `None`).
  - `async fn put_meta(&self, meta: &Meta) -> anyhow::Result<()>` (content-type `application/json`).
  - `async fn get_meta(&self, project, id) -> anyhow::Result<Option<Meta>>`.
  - `enum MarkerOutcome { Created, Exists(Uuid) }`; `async fn create_marker(&self, project, key: &str, id: Uuid) -> anyhow::Result<MarkerOutcome>`: `put_object().if_none_match("*")`; on `SdkError::ServiceError` with raw status 412 or 409, `get_object` the marker (retry up to 3 times with 50 ms sleep if `NoSuchKey`) and return `Exists(id)`.
  - `async fn list_metas(&self) -> anyhow::Result<Vec<Meta>>`: `list_objects_v2` paginated with prefix `projects/`, keep keys ending `/meta.json`, `get_meta` each with concurrency 16 (`futures::stream::iter(..).buffer_unordered(16)`).
  - `tests/common/mod.rs`: `pub async fn store() -> S3Store` building a client from env (`AWS_ENDPOINT_URL` etc.) with bucket `format!("muninn-test-{}", Uuid::now_v7())` and `ensure_bucket` called; `pub fn project() -> Uuid` (`Uuid::now_v7()`).

- [ ] **Step 1: Write failing unit tests for keys**

```rust
#[test] fn key_layout() {
    let p = Uuid::nil(); let id = Uuid::max();
    assert_eq!(keys::content(p, id), format!("projects/{p}/datapoints/{id}/content"));
    assert_eq!(keys::meta(p, id), format!("projects/{p}/datapoints/{id}/meta.json"));
    assert_eq!(keys::marker(p, "abc"), format!("projects/{p}/idempotency/ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
}
#[test] fn parse_meta_key_roundtrip() { let (p, id) = (Uuid::now_v7(), Uuid::now_v7()); assert_eq!(keys::parse_meta_key(&keys::meta(p, id)), Some((p, id))); assert_eq!(keys::parse_meta_key("projects/x/datapoints/y/content"), None); }
```

- [ ] **Step 2: Write failing integration tests in `tests/store.rs` (all `#[ignore]`)**

```rust
#[tokio::test] #[ignore] async fn content_and_meta_roundtrip() { /* put_content, get_content == Some(same); put_meta, get_meta == Some(same id); get_content for unknown id == None */ }
#[tokio::test] #[ignore] async fn marker_is_create_only() { /* create_marker -> Created; again with other id -> Exists(first id) */ }
#[tokio::test] #[ignore] async fn list_metas_finds_all_projects() { /* 2 metas in 2 projects; list_metas len 2; an orphan content object without meta is not listed */ }
```

- [ ] **Step 3: `cargo test keys` fails; `cargo test -- --ignored` fails to compile**
- [ ] **Step 4: Implement `src/store.rs` and `tests/common/mod.rs`**
- [ ] **Step 5: `docker compose up -d`; `cargo test` passes; `set -a; . ./.env.example; set +a; cargo test --test store -- --ignored` passes; clippy, fmt clean**
- [ ] **Step 6: Commit** `git commit -am "Add S3 store for content, metadata and idempotency markers"`

---

### Task 7: Recall selection algorithm (pure)

**Files:**
- Create: `src/recall.rs`
- Modify: `src/lib.rs`
- Test: `src/recall.rs` (unit)

**Interfaces:**
- Produces:
  - `Candidate { id: Uuid, score: f32, timestamp: DateTime<Utc>, content_tokens: usize }`
  - `Sort { Relevance, Chronological }` (Deserialize, lowercase)
  - `SelectParams { limit: usize, budget_tokens: usize, query_timestamp: DateTime<Utc>, occurred_from: Option<DateTime<Utc>>, occurred_to: Option<DateTime<Utc>>, sort: Sort }`
  - `Selected { items: Vec<Selection>, used_tokens: usize }`, `Selection { rank: usize, id: Uuid, score: f32 }`
  - `recall::select(candidates: Vec<Candidate>, p: &SelectParams) -> Selected` implementing spec steps 4 to 7: sort by score desc (stable), filter by time, greedy keep-if-fits-else-skip-and-continue, stop at `limit`, rank 1..n in relevance order, then reorder by timestamp asc with id as tiebreak when `Chronological`.
  - `Level { Low, Medium, High, ExtraHigh }` (Deserialize with `rename_all = "kebab-case"`) and `Level::tokens(&self) -> usize`.

- [ ] **Step 1: Write failing tests** (helper `c(score, ts_secs, tokens) -> Candidate`)

```rust
#[test] fn keeps_in_score_order_and_assigns_rank() { /* three cands scores .9 .5 .7 → ids ordered .9 .7 .5, ranks 1 2 3 */ }
#[test] fn budget_skips_big_and_keeps_smaller_later() { /* budget 100; tokens 60, 60, 30 → keeps first (60) and third (30), used 90 */ }
#[test] fn budget_too_small_for_all_gives_empty() { /* budget 10; all tokens 50 → items empty, used 0 */ }
#[test] fn limit_stops_walk() { /* limit 2, five cands → 2 items */ }
#[test] fn excludes_after_query_timestamp() { }
#[test] fn occurred_window_is_inclusive_both_ends() { /* cands at from, to, from-1s, to+1s → two kept */ }
#[test] fn chronological_keeps_rank() { /* scores .9 (newer), .5 (older) → order older first, ranks [2,1] */ }
#[test] fn level_tokens() { assert_eq!(Level::ExtraHigh.tokens(), 32000); assert_eq!(serde_json::from_str::<Level>("\"extra-high\"").unwrap(), Level::ExtraHigh); }
```

- [ ] **Step 2: `cargo test recall` fails**
- [ ] **Step 3: Implement `src/recall.rs`**
- [ ] **Step 4: Tests, clippy, fmt pass**
- [ ] **Step 5: Commit** `git commit -am "Add pure recall selection algorithm"`

---

### Task 8: Ingest endpoint with idempotency

**Files:**
- Create: `src/api/state.rs`, `src/api/ingest.rs`, `src/api/locks.rs`, `tests/ingest.rs`
- Modify: `src/api/mod.rs`, `src/main.rs`, `tests/common/mod.rs`
- Test: `tests/ingest.rs` (ignored, LocalStack + FakeEmbedder)

**Interfaces:**
- Consumes: `S3Store`, `MemoryIndex`/`VectorIndex`, `Embedder`, `NewDatapoint`, `Meta`, `count_tokens`, `build_input`, `ApiError`, `Json`.
- Produces:
  - `AppState` (Clone): `store: S3Store, index: Arc<dyn VectorIndex>, embedder: Arc<dyn Embedder>, ready: Arc<AtomicBool>, locks: KeyLocks`.
  - `KeyLocks::default()`, `async fn lock(&self, key: String) -> tokio::sync::OwnedMutexGuard<()>`: map `String → Arc<tokio::sync::Mutex<()>>`; entry removed when the last guard drops (builder's choice of mechanism; a `HashMap` under `std::sync::Mutex` with removal when `Arc::strong_count == 1` after unlock is acceptable).
  - `api::router(state: AppState) -> Router`: routes `/healthz`, `/api/v1/ingest`, `/api/v1/ingest/batch` (Task 9), `/api/v1/recall` (Task 10), with `tower_http::limit::RequestBodyLimitLayer::new(10 * 1024 * 1024)` and `TraceLayer`.
  - `IngestRequest` (Deserialize, camelCase): `idempotency_key: Option<String>`, `#[serde(flatten)] datapoint: NewDatapoint`.
  - `IngestResponse` (Serialize): `#[serde(flatten)] datapoint: Datapoint, deduplicated: bool`.
  - `ingest::ingest_one(state: &AppState, new: NewDatapoint, key: Option<&str>) -> Result<(StatusCode, IngestResponse), ApiError>` implementing the spec's write order: validate → (with key) lock on `format!("{project}:{key}")`, `create_marker(project, key, Uuid::now_v7())`; `Exists(id)` → `get_meta`; `Some` → `get_content` and return `200, deduplicated: true`; `None` → continue with that id. Then `spawn_blocking` embed `build_input(..)` via `embed_passages`, `count_tokens(content)`, `put_content`, `put_meta`, `index.upsert`, return `201, deduplicated: false`.
  - Handler `POST /api/v1/ingest`: returns `ApiError::Loading` if `!ready`, else calls `ingest_one`.
  - `tests/common/mod.rs` adds `pub async fn app() -> (axum::Router, AppState)` with `FakeEmbedder::new(64)`, `MemoryIndex`, a fresh test bucket, `ready` set true; `pub async fn post(router: &Router, path: &str, body: Value) -> (StatusCode, Value)` via `tower::ServiceExt::oneshot`.

- [ ] **Step 1: Write failing integration tests in `tests/ingest.rs` (all `#[ignore]`)**

```rust
#[tokio::test] #[ignore] async fn ingest_returns_201_with_server_id_and_defaults() { /* status 201; body.id parses as Uuid v7; metadata == {}; timestamp present; deduplicated false; store.get_content(project, id) == Some(content); index.len(project)==1 */ }
#[tokio::test] #[ignore] async fn ingest_with_key_dedupes_and_keeps_first_body() { /* post key K label "first"; post key K label "second" → 200, deduplicated true, label "first", same id; index.len==1 */ }
#[tokio::test] #[ignore] async fn same_key_different_projects_are_distinct() { }
#[tokio::test] #[ignore] async fn ingest_completes_half_written_datapoint() { /* create_marker directly on store for key K with id X and no meta; post with key K → 201, body.id == X */ }
#[tokio::test] #[ignore] async fn validation_errors_are_400() {
    /* missing content → 400 code validation; metadata [1] → 400 code validation; projectId "nope" → 400 code validation; label "   " → 400 */
}
#[tokio::test] #[ignore] async fn loading_returns_503() { /* ready false → 503 code loading */ }
```

- [ ] **Step 2: `cargo test --test ingest -- --ignored` fails to compile**
- [ ] **Step 3: Implement `state.rs`, `locks.rs`, `ingest.rs`, wire `router` in `api/mod.rs`; update `main.rs` to build `S3Store::from_config`, call `ensure_bucket`, build `FastEmbedder::new` (on `spawn_blocking`) and `MemoryIndex`, and set `ready = true` for now (Task 11 replaces this)**
- [ ] **Step 4: `cargo test` and `cargo test --test ingest -- --ignored` pass; clippy, fmt clean**
- [ ] **Step 5: Commit** `git commit -am "Add ingest endpoint with idempotent writes"`

---

### Task 9: Batch ingest

**Files:**
- Modify: `src/api/ingest.rs`, `src/api/mod.rs`, `tests/ingest.rs`
- Test: `tests/ingest.rs`

**Interfaces:**
- Produces:
  - `BatchRequest { idempotency_key: Option<String>, items: Vec<NewDatapoint> }`.
  - `BatchItemResult` (Serialize, untagged or manual): success `{ status: u16, deduplicated: bool, datapoint: Datapoint }`, failure `{ status: u16, error: { code, message } }`.
  - `BatchResponse { results: Vec<BatchItemResult> }`, HTTP 200.
  - Handler `POST /api/v1/ingest/batch`: `Loading` check; `items.len() > 100` → `BatchTooLarge`; validate every item, collect failures into `ValidationDetailed { message: "invalid items", details }` and return before any write; then for each item in order call `ingest_one(state, item, per_item_key.as_deref())` where `per_item_key = key.as_ref().map(|k| format!("{k}#{i}"))`; on `Err` record `{status: 500, error}` for that item and `{status: 0, error: {code: "skipped", ...}}` for every later item, then stop.

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test] #[ignore] async fn batch_ingests_in_order_with_per_item_status() { /* 3 items → 200, results[i].status 201, ids strictly increasing (v7) */ }
#[tokio::test] #[ignore] async fn batch_validates_all_before_writing() { /* item 1 ok, item 2 empty label → 400 validation, details[0].index == 1, index.len()==0 */ }
#[tokio::test] #[ignore] async fn batch_retry_with_key_dedupes_per_item() { /* post twice with key B → second: all results status 200 deduplicated true, same ids; index.len == 3 */ }
#[tokio::test] #[ignore] async fn batch_too_large_is_400() { /* 101 items → 400 batch-too-large */ }
```

- [ ] **Step 2: Tests fail**
- [ ] **Step 3: Implement batch handler**
- [ ] **Step 4: Tests, clippy, fmt pass**
- [ ] **Step 5: Commit** `git commit -am "Add batch ingest with per-item idempotency"`

---

### Task 10: Recall endpoint

**Files:**
- Create: `src/api/recall.rs`, `tests/recall.rs`
- Modify: `src/api/mod.rs`
- Test: `src/api/recall.rs` (unit for budget resolution), `tests/recall.rs` (ignored)

**Interfaces:**
- Consumes: `recall::select`, `Level`, `Sort`, `VectorIndex::search`, `S3Store::get_meta/get_content`, `Embedder::embed_query`.
- Produces:
  - `RecallRequest` (Deserialize, camelCase): `project_id: Uuid, query: String, limit: Option<usize>, level: Option<Level>, max_tokens: Option<usize>, query_timestamp: Option<DateTime<Utc>>, occurred_from: Option<DateTime<Utc>>, occurred_to: Option<DateTime<Utc>>, sort: Option<Sort>`.
  - `fn resolve_budget(level: Option<Level>, max_tokens: Option<usize>) -> Result<usize, ApiError>`: both → `BudgetConflict`; `max_tokens == Some(0)` → `Validation`; none → 8000.
  - `fn validate(req: &RecallRequest) -> Result<(), ApiError>`: `query.trim()` empty → `Validation`; `limit` outside 1..=200 → `Validation`; `occurred_from > occurred_to` → `RecallRangeInvalid`.
  - `RecallItem` (Serialize, camelCase): `rank, score, #[serde(flatten)] datapoint: Datapoint, content_tokens`.
  - `RecallResponse { results: Vec<RecallItem>, used_tokens: usize, budget_tokens: usize }`.
  - Handler: `Loading` check → validate → budget → `spawn_blocking(embed_query)` → `index.search(project, &q, min(limit * 4, 1000))` → `get_meta` for each hit concurrently (`buffer_unordered(16)`), skipping `None` → `Candidate`s → `select` → `get_content` for kept ids concurrently → assemble in selection order.

- [ ] **Step 1: Write failing unit tests for `resolve_budget` and `validate`**

```rust
#[test] fn default_budget_is_8000() { assert_eq!(resolve_budget(None, None).unwrap(), 8000); }
#[test] fn both_is_conflict() { assert!(matches!(resolve_budget(Some(Level::Low), Some(5)), Err(ApiError::BudgetConflict))); }
#[test] fn range_invalid() { /* from > to → RecallRangeInvalid */ }
#[test] fn limit_bounds() { /* 0 and 201 → Validation; 1 and 200 ok */ }
#[test] fn blank_query_rejected() { }
```

- [ ] **Step 2: Write failing integration tests in `tests/recall.rs` (ignored)**

```rust
#[tokio::test] #[ignore] async fn recall_returns_most_similar_first() { /* ingest "rust memory service", "banana bread recipe", "postgres index tuning"; query "memory service in rust" → results[0].label is the rust one, rank 1, score > results[1].score, content present */ }
#[tokio::test] #[ignore] async fn recall_scopes_to_project() { }
#[tokio::test] #[ignore] async fn recall_respects_time_window_and_query_timestamp() { /* three datapoints at T-2d, T-1d, T; occurredFrom T-1d → 2; queryTimestamp T-1d → 2 */ }
#[tokio::test] #[ignore] async fn recall_chronological_keeps_rank() { }
#[tokio::test] #[ignore] async fn recall_budget_omits_whole_documents() { /* one ~200-token doc + one short; maxTokens 50 → only short doc, usedTokens == its contentTokens, budgetTokens 50 */ }
#[tokio::test] #[ignore] async fn recall_errors() { /* level+maxTokens → 400 budget-conflict; from>to → 400 recall-range-invalid; limit 0 → 400 validation */ }
```

- [ ] **Step 3: Tests fail**
- [ ] **Step 4: Implement `src/api/recall.rs` and route**
- [ ] **Step 5: Tests, clippy, fmt pass**
- [ ] **Step 6: Commit** `git commit -am "Add recall endpoint"`

---

### Task 11: Startup rebuild, readiness, model-mismatch re-embed

**Files:**
- Create: `src/boot.rs`, `tests/boot.rs`
- Modify: `src/main.rs`, `src/api/mod.rs` (healthz reads `ready`)
- Test: `tests/boot.rs` (ignored)

**Interfaces:**
- Produces:
  - `boot::rebuild_index(store: &S3Store, index: &dyn VectorIndex, embedder: &dyn Embedder) -> anyhow::Result<usize>`: `list_metas`; for each meta, if `meta.embedding.model != embedder.model_name()` then `get_content`, re-embed via `spawn_blocking` with `build_input`, rewrite `meta.json` with the new `StoredEmbedding` (and recomputed `content_tokens`), then `upsert`; otherwise `upsert` the stored vector. Returns count loaded. Metas whose content is missing are logged and skipped.
  - `main.rs`: bind the listener and start serving with `ready = false`; spawn `rebuild_index` as a task that sets `ready = true` on success and logs the count, or logs the error and exits the process with code 1 on failure.
  - `GET /healthz` returns `{"status":"loading"}` while `!ready`, `{"status":"ok"}` after.

- [ ] **Step 1: Write failing tests in `tests/boot.rs`**

```rust
#[tokio::test] #[ignore] async fn rebuild_loads_existing_metas() { /* ingest 2 via app A; build fresh MemoryIndex; rebuild_index → 2; index.len(project)==2 */ }
#[tokio::test] #[ignore] async fn rebuild_reembeds_on_model_mismatch() { /* put a Meta with embedding.model "old", dim 3, vector [1,0,0] and its content; rebuild with FakeEmbedder(64) → get_meta shows model "fake", dim 64 */ }
#[tokio::test] #[ignore] async fn rebuild_skips_meta_without_content() { /* put_meta only; rebuild returns 0 and Ok */ }
#[tokio::test] #[ignore] async fn healthz_reports_loading_then_ok() { /* ready false → "loading"; set true → "ok" */ }
```

- [ ] **Step 2: Tests fail**
- [ ] **Step 3: Implement `src/boot.rs`, wire `main.rs`, update healthz**
- [ ] **Step 4: Tests, clippy, fmt pass. Manual: `cargo run`, ingest one, stop, `cargo run` again, recall finds it**
- [ ] **Step 5: Commit** `git commit -am "Rebuild index from S3 on startup with readiness gate"`

---

### Task 12: Dockerfile, real-model smoke test, docs sync

**Files:**
- Create: `Dockerfile`, `.dockerignore`, `tests/real_model.rs`
- Modify: `src/main.rs` (download-only mode), `README.md`, `CONTRIBUTING.md`, `ARCHITECTURE.md`

**Interfaces:**
- `main.rs`: if env `DOWNLOAD_ONLY=1`, construct `FastEmbedder` for the configured model and exit 0 without starting the server.
- `Dockerfile`: stage 1 `rust:1-bookworm` builds `--release`; stage 2 `debian:bookworm-slim` with `ca-certificates`, copies the binary, `ENV MODEL_CACHE=/models`, `RUN DOWNLOAD_ONLY=1 /usr/local/bin/muninn`, `EXPOSE 8080`, `CMD ["muninn"]`. `ARG EMBED_MODEL=multilingual-e5-small` forwarded as `ENV`.

- [ ] **Step 1: Write `tests/real_model.rs`**

```rust
#[test] #[ignore] fn real_model_embeds_multilingual() {
    let e = FastEmbedder::new(ModelChoice::MultilingualE5Small, Path::new("./.fastembed_cache")).unwrap();
    assert_eq!(e.dim(), 384);
    let q = e.embed_query("where do we keep the deployment notes").unwrap();
    let v = e.embed_passages(&["Deploymentanteckningarna ligger i Notion under Infra".into(), "Recept på kanelbullar".into()]).unwrap();
    assert!(dot(&q, &v[0]) > dot(&q, &v[1]));
    assert!((q.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-3);
}
```

- [ ] **Step 2: Run `cargo test --test real_model -- --ignored`; first run downloads the model; passes**
- [ ] **Step 3: Add download-only mode to `main.rs`; write `Dockerfile` and `.dockerignore` (`target`, `.fastembed_cache`, `.git`)**
- [ ] **Step 4: `docker build -t muninn .` succeeds; `docker run --rm --network host --env-file .env.example muninn` serves `/healthz`**
- [ ] **Step 5: Reconcile README, CONTRIBUTING and ARCHITECTURE with what was built (env vars, test commands, Docker usage, download-only flag). Remove "not yet implemented" wording from README**
- [ ] **Step 6: Full check: `cargo test`, `cargo test -- --ignored`, clippy, fmt**
- [ ] **Step 7: Commit** `git commit -am "Add Dockerfile with baked model and sync docs"`
