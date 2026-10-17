# Architecture

How Muninn works, in enough detail to reason about behaviour without
reading the code. The full design with rationale is in
[docs/superpowers/specs/2026-10-17-memory-service-design.md](docs/superpowers/specs/2026-10-17-memory-service-design.md).

## The big picture

```
 agent ──POST /ingest──▶ ┌────────────────────────────┐
                         │  axum HTTP service          │
                         │                             │
                         │  embedder (fastembed, CPU)  │──▶ vector
                         │  token counter (cl100k)     │──▶ contentTokens
                         │                             │
                         │  VectorIndex (in-memory)    │◀── rebuilt from S3 on start
                         │                             │
                         └──────────┬──────────────────┘
                                    │
                                    ▼
                          S3 bucket (LocalStack locally)
                          projects/{projectId}/datapoints/{id}/content
                          projects/{projectId}/datapoints/{id}/meta.json
                          projects/{projectId}/idempotency/{sha256(key)}
```

There is no database. S3 holds everything. The in-memory index is a cache
of the vectors in S3 and is rebuilt on every start.

## Storage

Each datapoint is two S3 objects, plus a third if the caller gave an
idempotency key.

- **`content`**: the document exactly as sent, as plain text. Kept
  separate so recall only downloads the documents it will return.
- **`meta.json`**: label, timestamp, metadata, audience, the embedding
  vector with the model name and dimension, and the content token count.
  Storing the vector and token count means a restart never has to run the
  model or tokenizer over old data.
- **idempotency marker**: a tiny JSON object `{"id": "..."}` mapping a
  caller's key to the datapoint it produced. The key is SHA-256 hashed so
  any string works as an S3 object name. Markers are per project.

`meta.json` is the source of truth. A datapoint exists when its
`meta.json` exists. Startup lists all `meta.json` objects and ignores
everything else.

## Ingest

1. Validate the request.
2. If there is an idempotency key: mint an id and create the marker with a
   create-only put. If the marker already existed, read its id. If that id
   already has a `meta.json`, return it as deduplicated. If not, a previous
   attempt crashed; continue below reusing that id.
3. Build the embedding input: label, content, then one `key: value` line
   per metadata entry. Prefix per model (E5 wants `passage: `). Embed on a
   blocking thread.
4. Count content tokens with `cl100k_base`.
5. Write `content`, then `meta.json`, then insert the vector into the
   index.

Writing the marker first pins the id, so a retry after a crash at any step
finishes the same datapoint instead of creating a second one. A per-key
lock inside the process serialises concurrent duplicates.

Batch ingest validates every item before writing anything, then runs each
item as a single ingest with key `"{batchKey}#{index}"`. A retry completes
unfinished items and dedupes finished ones.

## Recall

1. Embed the query with the model's query prefix.
2. Ask the index for the top `limit * 4` (capped at 1000) ids by cosine
   score for this project.
3. Load `meta.json` for each hit.
4. Drop anything after `queryTimestamp` or outside
   `[occurredFrom, occurredTo]`.
5. Walk the rest in score order. Keep a document if there is room under
   `limit` and its `contentTokens` fits in the remaining budget. If it
   does not fit, skip it and keep walking; a smaller later one may fit.
6. Number the kept set `rank` 1..n in relevance order.
7. If `sort` is `chronological`, reorder by timestamp ascending, keeping
   each item's `rank`.
8. Download `content` for the kept set in parallel and respond.

The budget is `level` (8k/16k/24k/32k) or `maxTokens`, never both.
Documents are whole or omitted, never cut.

## Embedding

`fastembed` runs ONNX models on the CPU. The model is a config option;
the default is `multilingual-e5-small` (384 dimensions, 512-token input,
100 languages). `bge-m3` is available for long documents (8192 tokens) at
a much higher CPU cost.

Input beyond the model's limit is silently truncated by the model, so a
very long document is searchable by its beginning only. The whole document
is still stored and returned. Chunking is a planned later change.

Every `meta.json` records which model produced its vector. On startup,
datapoints embedded by a different model than the configured one are
re-embedded and rewritten.

Model files are downloaded once into `MUNINN_MODEL_CACHE`. The Dockerfile
downloads them at build time so containers start offline.

## Vector index

```rust
trait VectorIndex {
    async fn upsert(&self, project: Uuid, id: Uuid, vector: Vec<f32>) -> Result<()>;
    async fn search(&self, project: Uuid, query: Vec<f32>, k: usize) -> Result<Vec<(Uuid, f32)>>;
}
```

v1 is a `HashMap<projectId, Vec<(id, vector)>>` behind an `RwLock` with
brute-force dot products. It is good to low tens of thousands of datapoints
per project. Time filtering happens in recall, not in the index, which is
why recall over-fetches.

Amazon S3 Vectors is the intended production implementation. LocalStack
does not emulate it, so it is not in v1. When it lands, `timestamp` will
be stored as filterable metadata so time filtering can move into the
index.

## Startup

1. Load config, create the S3 client (path-style when an endpoint override
   is set).
2. Load the embedding model or fail.
3. Create the bucket if missing.
4. Scan `meta.json` objects into the index. `/healthz` reports `loading`
   and ingest/recall return `503` until this finishes.
5. Serve.

## Error envelope

| HTTP | code                   | when                                          |
|------|------------------------|-----------------------------------------------|
| 400  | `validation`           | missing/invalid field, bad uuid, bad timestamp |
| 400  | `recall-range-invalid` | `occurredFrom > occurredTo`                   |
| 400  | `budget-conflict`      | both `level` and `maxTokens`                  |
| 400  | `batch-too-large`      | more than 100 items                           |
| 503  | `loading`              | index not yet rebuilt from S3                 |
| 500  | `internal`             | S3 or model failure; details in logs only     |

## Known limits

- One process owns the index. Two instances would each need to rebuild
  from S3 and would not see each other's writes until restart.
- Idempotency across two processes on an S3 that ignores conditional puts
  is best effort.
- Long documents are searchable by their first 512 tokens unless `bge-m3`
  is configured.
