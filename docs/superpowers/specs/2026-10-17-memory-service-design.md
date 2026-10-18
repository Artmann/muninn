# Muninn memory service: ingest and recall

Date: 2026-10-17

## Intent

Muninn is a memory service for AI agents. An agent mid-conversation asks
"what do we know about X" and gets back the datapoints closest in meaning
to X, scoped to its project and optionally to a time window, fitted into a
token budget it can paste straight into a prompt. Datapoints are written by
the same or other agents as things happen.

## Goals

- Rust HTTP API with three endpoints: `POST /api/v1/ingest`,
  `POST /api/v1/ingest/batch`, `POST /api/v1/recall`.
- Documents, metadata and embeddings live in S3. LocalStack for local dev.
- Embeddings are produced by a local model inside the service, no external
  API.
- Ingest is idempotent on a caller-supplied key.
- Recall is semantic search with a relevance-ordered, budget-fitted result.
- Vector search is behind a trait. The default implementation is in-memory
  and is rebuilt from S3 on startup. Amazon S3 Vectors is the second,
  chosen by setting `MUNINN_VECTOR_BUCKET`; nothing in the API depends on
  which one runs.

## Non-goals (for now)

- Authentication or multi-tenant isolation beyond `projectId` scoping.
- Filtering on `audience`. It is stored and returned, nothing more.
- Update or delete of datapoints.
- Metadata filters on recall.
- Pagination beyond `limit`.
- Plan ceilings on the evidence budget (the source spec mentions them; we
  have no plans).
- Horizontal scaling. One process owns the in-memory index.

## Anti-goals

- A recall that silently truncates a document to fit the budget. Documents
  are whole or omitted.
- A design where swapping to S3 Vectors changes the HTTP contract.
- Depending on LocalStack for anything it does not emulate (it does not
  emulate S3 Vectors).

## Datapoint

| Field       | Type                 | Set by  | Notes                                         |
|-------------|----------------------|---------|-----------------------------------------------|
| id          | uuid v7              | server  |                                               |
| projectId   | uuid v7              | caller  | all storage and queries are scoped by it      |
| label       | string               | caller  | title                                         |
| content     | string               | caller  | the document body                             |
| timestamp   | ISO 8601 (RFC 3339)  | caller  | when the thing occurred; defaults to now; at most 5 min in the future |
| metadata    | JSON object          | caller  | defaults to `{}`                              |
| audience    | string               | caller  | optional; stored and echoed, never filtered   |

Required on ingest: `projectId`, `label`, `content`. Everything else is
optional.

`projectId` is validated as any RFC 4122 UUID, not v7 specifically. The
table says v7 because that is what callers are expected to mint, but
rejecting other versions would break standard UUID generation for no
benefit. (Author's call.)

This document is the HTTP contract. The recall request shape was modelled
on an external API's docs, but there are no existing clients or SDKs to
conform to; where this document and that one differ, this one wins.

## Storage

Everything lives in one S3 bucket. The bucket name comes from config
(`MUNINN_BUCKET`, default `muninn`). There is no database. S3 is the only
place data is kept, and the in-memory search index is just a cache of what
is in S3.

### What gets stored for one datapoint

Each datapoint becomes two objects in the bucket, plus an optional third
one if the caller gave an idempotency key. All of them sit under a folder
for the project, then a folder for the datapoint id:

```
projects/{projectId}/datapoints/{id}/content
projects/{projectId}/datapoints/{id}/meta.json
projects/{projectId}/idempotency/{sha256(key)}
```

**`content`** is the document exactly as the caller sent it, stored as
plain text. It is kept separate from everything else so that recall can
fetch only the documents it is about to return, and so that a large
document never has to be parsed just to read its label or timestamp.

**`meta.json`** is everything else we know about the datapoint:

```json
{
  "id": "...",
  "projectId": "...",
  "label": "...",
  "timestamp": "...",
  "metadata": {},
  "audience": null,
  "embedding": { "model": "bge-small-en-v1.5", "dim": 384, "vector": [ ... ] },
  "contentTokens": 123
}
```

Two of these fields are computed by the service rather than sent by the
caller:

- `embedding` is the vector the local model produced from the label,
  content and metadata. It is stored so that when the service restarts it
  can rebuild its search index by reading these files, without running the
  model again over every document. The model name and dimension are stored
  next to it so a future model change can be detected: if the service is
  started with a different model than the one in the file, it re-embeds
  that datapoint on load and rewrites the file.
- `contentTokens` is how many tokens the content is. Recall needs this
  number to fit documents into the budget, and counting is cheap to do once
  at write time and expensive to do on every query.

**The idempotency marker** is a tiny file that maps a caller's
`idempotencyKey` to the datapoint id that key produced. The key is hashed
with SHA-256 before it becomes part of the object name, because callers
can send any string and S3 object names have length and character limits.
Markers live under the project, so the same key in two different projects
is two different datapoints.

### Why `meta.json` is the source of truth

When the service starts, it lists every `meta.json` in the bucket and loads
them into the in-memory index. It does not look at `content` objects or
markers to decide what exists. This means a datapoint only "exists" once
its `meta.json` has been written, which is what makes the write order
below safe.

### How a write happens

When the caller sends an idempotency key:

1. The service mints a new id and tries to create the marker file for that
   key with a "only create if it does not already exist" put
   (`If-None-Match: *`). If the marker was already there, the service reads
   the id out of it and skips to step 5.
2. Write `content`.
3. Write `meta.json`.
4. Add the vector to the in-memory index.
5. Deduplicate path. If `meta.json` exists for the id in the marker, the
   datapoint is complete; return it with `deduplicated: true`. If it does
   not exist, a previous attempt crashed partway through, so the service
   continues from step 2 using the id from the marker, and returns `201` as
   if it were a fresh write.

Without an idempotency key, only steps 2 to 4 happen.

The point of writing the marker first is that the id is pinned before any
data is written. Whatever step a crash happens at, retrying with the same
key finishes the same datapoint rather than starting a second one. A
half-written datapoint is invisible to recall and to startup because it
has no `meta.json` yet.

The service also takes a lock on `(projectId, key)` for the duration of an
ingest, so two identical requests arriving at the same time in one process
are handled one after the other. That is the whole v1 guarantee: a key is
written once per process and across restarts. Two separate processes
racing on the same key against an S3 that ignores conditional puts could
both write. (Author's call.)

## Embedding

Crate: `fastembed`. It runs ONNX models on the CPU inside the service, no
external API. Vectors are L2-normalised before storage so cosine
similarity is a dot product.

### Model is a config option

The model is chosen by `MUNINN_EMBED_MODEL`. The service must handle
multiple languages, so the default is multilingual. Supported values in
v1, all from fastembed's built-in list:

| Value                   | Dim  | Max input | Size    | When to use                                  |
|-------------------------|------|-----------|---------|----------------------------------------------|
| `multilingual-e5-small` | 384  | 512 tok   | ~450 MB | default; fast, 100 languages, fine for short datapoints |
| `multilingual-e5-base`  | 768  | 512 tok   | ~1.1 GB | better quality, same limits                  |
| `bge-m3`                | 1024 | 8192 tok  | ~2.2 GB | long documents; 100+ languages; slow on CPU  |
| `bge-small-en-v1.5`     | 384  | 512 tok   | ~130 MB | English only, smallest                       |

Other fastembed models may be added to the enum by the builder; the names
above are the public config values and must stay stable.

The model's name and dimension are stored in every `meta.json`. On
startup, any datapoint whose stored model differs from the configured one
is re-embedded and rewritten, so switching models is a config change plus a
slow first start, not a migration. The in-memory index holds vectors of
one dimension per process; the S3 Vectors implementation uses one index
per model.

### Query and passage prefixes

E5 models are trained with `"query: "` in front of search queries and
`"passage: "` in front of documents, and quality drops without them. bge
models use an instruction prefix on queries only. The embedder
implementation owns this: it knows which model it is running and adds the
right prefix to documents at ingest and to the query at recall. Callers
never see prefixes.

### Long content

Input longer than the model's limit is truncated by the model; only the
first part of the document is embedded, though the whole document is
stored and returned. This is accepted for v1 because datapoints written by
agents are mostly short, and `bge-m3` is available for deployments that
need long documents. Chunking into several vectors per datapoint is a
later change (see Open for later). (Author's call.)

### Model files

Model files are downloaded on first run into a cache directory from config
(`MUNINN_MODEL_CACHE`, default `./.fastembed_cache`). The repo ships a
`Dockerfile` that downloads the configured model at image build time into
that directory, so a container start never needs network access to Hugging
Face. If the model is missing and cannot be downloaded, the service fails
at startup with a clear message rather than serving requests. (Author's
call.)

### Embedding input text

In this order, joined with `\n`, then prefixed as above:

```
{label}
{content}
{key}: {value}        one line per top-level metadata entry, keys sorted,
                      value rendered as compact JSON if not a string
```

The recall query is embedded as the bare query string plus the model's
query prefix.

## Vector index

```rust
trait VectorIndex {
    async fn upsert(&self, project: Uuid, id: Uuid, vector: Vec<f32>) -> Result<()>;
    async fn upsert_many(&self, items: Vec<(Uuid, Uuid, Vec<f32>)>) -> Result<()>; // default: loop
    async fn search(&self, project: Uuid, query: &[f32], k: usize) -> Result<Vec<(Uuid, f32)>>;
}
```

`search` returns ids with cosine scores, highest first, at most `k`. Time
filtering is not the index's job in v1; recall over-fetches and filters.

### In-memory implementation

`HashMap<Uuid /*project*/, Vec<(Uuid, Vec<f32>)>>` behind an `RwLock`.
Brute-force dot product. On startup the service lists
`projects/*/datapoints/*/meta.json`, loads each, and fills the map. The
service does not accept ingest or recall requests until the load is
complete; the health endpoint reports `loading` until then. Fine up to low
tens of thousands of datapoints per project. Beyond that is what S3
Vectors is for.

### S3 Vectors implementation

Used when `MUNINN_VECTOR_BUCKET` is set. S3 still holds all documents;
the vector bucket holds only the search index.

- Index name: `muninn-` plus the model name, lowercased, with anything
  other than letters, digits and hyphens replaced by `-` (for example
  `muninn-multilingual-e5-small`). One index per model, so a model switch
  starts a fresh index and leaves the old one unused.
- On startup the vector bucket and index are created if missing (float32,
  cosine, the model's dimension). An existing index with a different
  dimension is a startup error.
- Each vector is keyed by datapoint id with filterable metadata
  `projectId`. Search filters on `projectId`, pages through results until
  it has `k`, and reports `1 - distance` as the score.
- The startup scan still runs and re-puts every vector in batches of 500.
  Puts are idempotent, so this repairs any vector a crash left out and
  keeps S3 the single source of truth.
- LocalStack does not emulate S3 Vectors. Its tests run against real AWS
  and are skipped unless `MUNINN_TEST_VECTOR_BUCKET` is set.

## Ingest

`POST /api/v1/ingest`

```json
{
  "idempotencyKey": "optional string",
  "projectId": "uuid", "label": "...", "content": "...",
  "timestamp": "2026-10-17T12:00:00Z", "metadata": {}, "audience": "..."
}
```

Response `201` with the stored datapoint (all fields, no embedding) plus
`"deduplicated": false`. If the idempotency marker already exists, respond
`200` with the existing datapoint and `"deduplicated": true`. The
idempotency key is scoped to the project; the same key in two projects is
two datapoints.

The marker records a fingerprint of the request alongside the id: a
SHA-256 over the datapoint fields with encoding differences removed
(omitted `metadata` equals `{}`, object keys sorted, timestamps compared as
instants; an omitted `timestamp` is distinct from an explicit one). A
repeat with the same key and the same request returns the original. A
different request reusing the key is rejected with
`422 idempotency-key-reused` and nothing is written, whether or not the
first attempt finished. This follows the IETF Idempotency-Key draft and
Stripe: silently returning the first datapoint for a different body would
hide client bugs such as colliding keys. The body is validated before the
key is checked, so an invalid body is always `400`. Markers written before
fingerprints existed carry only the id and are honoured without a check.

An empty or whitespace-only `idempotencyKey` is rejected with
`400 validation`; omit the field instead.

`POST /api/v1/ingest/batch`

```json
{ "idempotencyKey": "optional", "items": [ { ...ingest body without idempotencyKey... } ] }
```

Each item carries its own `projectId`; they may differ within a batch. Max
100 items per batch.

All items are validated before anything is written. Any validation failure
rejects the whole batch with `400 validation` and a `details` array naming
the failing item indexes, so nothing is stored. After validation the only
failures left are infrastructure ones.

Idempotency is per item, derived from the batch key: item `i` is ingested
exactly as a single ingest with key `"{batchKey}#{i}"`. A retry after a
partial failure therefore completes the items that did not finish and
dedupes the ones that did, with no duplicates. Without a batch key there is
no per-item key either.

Response, HTTP `200`:
`{ "results": [ {"status": 201, "deduplicated": false, "datapoint": ...},
{"status": 200, "deduplicated": true, "datapoint": ...}, {"status": 500,
"error": {...}}, ... ] }`. Items are processed in order; after an
infrastructure failure on item N, items after N are not attempted and are
reported with status `0` and error code `skipped`. (Author's call.)

Embedding is CPU-bound; it runs on a blocking thread (`spawn_blocking`).
Batch embeds all items in one model call.

## Recall

`POST /api/v1/recall`

| Field          | Type      | Default     | Rules                                                     |
|----------------|-----------|-------------|-----------------------------------------------------------|
| projectId      | uuid      | required    |                                                           |
| query          | string    | required    | non-empty                                                 |
| limit          | int       | 100         | 1..=200                                                   |
| level          | string    | `low`       | `low` 8000, `medium` 16000, `high` 24000, `extra-high` 32000 tokens |
| maxTokens      | int       |             | >= 1; mutually exclusive with `level` (400 if both)       |
| queryTimestamp | ISO 8601  | now         | only datapoints with `timestamp <= queryTimestamp` are eligible |
| occurredFrom   | ISO 8601  |             | inclusive                                                 |
| occurredTo     | ISO 8601  |             | inclusive; `from > to` is 400 `recall-range-invalid`      |
| sort           | string    | `relevance` | or `chronological`                                        |

Algorithm:

1. Embed `query`.
2. `index.search(project, qvec, k)` with `k = limit * 4` capped at 1000,
   so time filtering has slack. Builder may tune.
3. Load `meta.json` for each hit (a small in-process cache keyed by id is
   fine; invalidation is not needed because datapoints are immutable).
4. Drop hits outside `[occurredFrom, occurredTo]` or after
   `queryTimestamp`.
5. Walk remaining hits in score order. Keep a hit if
   `kept.len() < limit` and `usedTokens + hit.contentTokens <= budget`.
   Otherwise skip it and keep walking; a smaller later document may still
   fit. Stop when `kept.len() == limit`.
6. Assign `rank` 1..n in relevance order over the kept set.
7. If `sort == chronological`, reorder kept by `timestamp` ascending, ties
   by id (uuid v7 is time-ordered). `rank` is kept on each item.
8. Load `content` for the kept set from S3, in parallel.

Response:

```json
{
  "results": [
    {
      "rank": 1, "score": 0.83,
      "id": "...", "projectId": "...", "label": "...", "content": "...",
      "timestamp": "...", "metadata": {}, "audience": null,
      "contentTokens": 123
    }
  ],
  "usedTokens": 4567,
  "budgetTokens": 8000
}
```

Token counting uses the `tiktoken-rs` crate with `cl100k_base`, over
`content` only. Counts are computed once at ingest and stored.

## Errors

All errors are JSON: `{ "error": { "code": "...", "message": "..." } }`.

| HTTP | code                    | when                                               |
|------|-------------------------|----------------------------------------------------|
| 400  | `validation`            | missing/invalid field, bad uuid, bad timestamp, timestamp > 5 min in the future |
| 400  | `recall-range-invalid`  | `occurredFrom > occurredTo`                        |
| 400  | `budget-conflict`       | both `level` and `maxTokens`                       |
| 400  | `batch-too-large`       | more than 100 items                                |
| 413  | `payload-too-large`     | request body over 10 MiB                           |
| 422  | `idempotency-key-reused`| key already used with a different request          |
| 503  | `loading`               | index not yet rebuilt from S3                      |
| 500  | `internal`              | S3 or model failure; message is generic, details logged |

## Service shape

- `axum` + `tokio`, `serde`, `aws-sdk-s3` + `aws-config`, `fastembed`,
  `tiktoken-rs`, `uuid` (v7), `time` or `chrono` for RFC 3339, `tracing`.
- Config from env: `MUNINN_BUCKET`, `MUNINN_BIND` (default `0.0.0.0:8080`),
  `MUNINN_EMBED_MODEL` (default `multilingual-e5-small`), `MUNINN_MODEL_CACHE`,
  plus standard `AWS_*` vars and `AWS_ENDPOINT_URL`
  for LocalStack. The S3 client uses path-style addressing when an endpoint
  override is set.
- `GET /healthz` always returns `200` with `{"status": "ok" | "loading"}`.
- Startup creates the bucket if it does not exist (LocalStack convenience;
  harmless in prod if permissions allow, logged and ignored if not).
- `docker-compose.yml` at repo root runs LocalStack with the S3 service.
- Licensed under MIT (`LICENSE` at repo root).

Suggested module split, builder may change:

```
src/main.rs          config, wiring, server
src/api/             handlers, request/response types, error mapping
src/domain.rs        Datapoint, validation
src/embed.rs         Embedder trait + fastembed impl
src/store.rs         S3 document store (content, meta, idempotency)
src/index/mod.rs     VectorIndex trait
src/index/memory.rs  in-memory impl
src/index/s3vectors.rs  Amazon S3 Vectors impl
src/recall.rs        the recall algorithm, pure over loaded metas (unit-testable)
src/tokens.rs        token counting
```

## Testing

- Unit tests for the recall selection algorithm (budget fitting, limit,
  time filters, chronological sort keeping rank) using hand-built metas
  and scores, no S3 or model.
- Unit tests for embedding input text construction and metadata flattening.
- Unit tests for the in-memory index (per-project isolation, ordering).
- Integration tests against LocalStack behind a feature flag or env guard:
  ingest, duplicate ingest returns same id, restart rebuilds the index and
  recall still works.
- The embedder is a trait so tests can use a fake that returns fixed
  vectors.

## Left to the builder

- Exact crate versions and whether to use `time` or `chrono`.
- The over-fetch multiplier in recall step 2.
- Startup scan concurrency.
- Request body size limit (settled: 10 MiB).

## Open for later

- `audience` semantics.
- Carrying `timestamp` as filterable S3 Vectors metadata so time
  filtering can move into the index. The startup re-put means adding it
  needs no migration.
- Update and delete.
- Chunking long content into several vectors per datapoint, with recall
  merging hits back to one document.
