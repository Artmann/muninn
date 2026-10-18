# Muninn

A memory service for AI agents. Agents write datapoints as things happen
and later ask "what do we know about X" to get back the closest matches,
scoped to a project, filtered by time, and fitted into a token budget they
can paste straight into a prompt.

- Semantic recall with a local, multilingual embedding model. No external
  AI API.
- S3 as the only storage backend, with an optional Amazon S3 Vectors
  index for larger data.
- Idempotent writes.
- Everything scoped to a `projectId`.

## Status

Early but working: everything below is implemented and tested. See
[ARCHITECTURE.md](ARCHITECTURE.md) for how it works and
[CONTRIBUTING.md](CONTRIBUTING.md) to run it locally.

## Quick start

```sh
docker compose up -d                     # LocalStack with S3
cp .env.example .env                     # point the AWS SDK at LocalStack
cargo run                                # first run downloads the embedding model
```

The service listens on `http://localhost:8080`.

### Docker

The image bakes the embedding model in at build time, so containers start
without network access to the model hub.

```sh
docker build -t muninn .
docker build -t muninn --build-arg EMBED_MODEL=bge-m3 .   # another model
docker run -p 8080:8080 --env-file .env.example \
  -e AWS_ENDPOINT_URL=http://host.docker.internal:4566 muninn
```

Against real S3, pass your AWS credentials and region instead and leave
`AWS_ENDPOINT_URL` unset.

## Configuration

| Variable             | Default                | Purpose                                            |
|----------------------|------------------------|----------------------------------------------------|
| `BUCKET`      | `muninn`               | S3 bucket; created on startup if missing           |
| `BIND`        | `0.0.0.0:8080`         | listen address                                     |
| `EMBED_MODEL` | `multilingual-e5-small`| embedding model, see below                         |
| `MODEL_CACHE` | `./.fastembed_cache`   | where model files are downloaded (`/models` in Docker) |
| `DOWNLOAD_ONLY` |                      | `1`: download the model and exit without serving   |
| `VECTOR_STORE_TYPE` | `memory`        | where the search index lives: `memory` or `s3` (Amazon S3 Vectors) |
| `VECTOR_BUCKET` |                      | S3 Vectors bucket; required when the type is `s3`  |
| `AWS_ENDPOINT_URL`   |                        | set to `http://localhost:4566` for LocalStack      |
| `AWS_*`              |                        | standard credentials and region                    |

### Amazon S3 Vectors

By default the search index lives in memory and is rebuilt from S3 on
every start. Set `VECTOR_STORE_TYPE=s3` and `VECTOR_BUCKET`
to keep it in Amazon S3 Vectors instead. The vector bucket and an index named after the model (for
example `muninn-multilingual-e5-small`) are created on startup if missing.
Use one vector bucket per environment, like the S3 bucket.

LocalStack does not emulate S3 Vectors, so this needs real AWS: unset
`AWS_ENDPOINT_URL` and use real credentials and region. The credentials
need `s3vectors:GetIndex`, `CreateIndex`, `CreateVectorBucket`,
`PutVectors`, `QueryVectors` and `GetVectors` on the vector bucket. You
can create the bucket yourself instead
(`aws s3vectors create-vector-bucket --vector-bucket-name muninn-dev-vectors`)
and drop `CreateVectorBucket`.

### Embedding models

| `EMBED_MODEL`    | Languages | Max input  | Download | Notes                              |
|-------------------------|-----------|------------|----------|------------------------------------|
| `multilingual-e5-small` | 100       | 512 tokens | ~450 MB  | default; fast                      |
| `multilingual-e5-base`  | 100       | 512 tokens | ~1.1 GB  | better quality                     |
| `bge-m3`                | 100+      | 8192 tokens| ~2.2 GB  | for long documents; slow on CPU    |
| `bge-small-en-v1.5`     | English   | 512 tokens | ~130 MB  | smallest                           |

Content longer than the model's max input is stored and returned whole,
but only the first part of it is used for search. Switching models
re-embeds existing data on the next start.

## API

All requests and responses are JSON. Errors look like
`{ "error": { "code": "...", "message": "..." } }`.

### `POST /api/v1/ingest`

```json
{
  "idempotencyKey": "optional",
  "projectId": "0192...uuid",
  "label": "Decided on S3 Vectors",
  "content": "We chose S3 Vectors over a separate vector DB because ...",
  "timestamp": "2026-10-17T12:00:00Z",
  "metadata": { "source": "slack", "channel": "#infra" },
  "audience": "engineering"
}
```

`projectId`, `label` and `content` are required; everything else is
optional. `timestamp` is when the thing happened and defaults to the time
of ingest. A `timestamp` more than 5 minutes ahead of the server clock is
rejected with `400 validation`, since recall only sees a datapoint once
its timestamp has passed. `metadata` defaults to `{}`.

Returns `201` with the stored datapoint. The server assigns the `id`.

`idempotencyKey` makes retries safe. Sending the same request again with
the same key returns `200` with the original datapoint and
`"deduplicated": true`. Reusing a key for a different request returns
`422 idempotency-key-reused` and stores nothing. Keys are scoped to the
project. An empty `idempotencyKey` is rejected; omit the field instead.

### `POST /api/v1/ingest/batch`

```json
{ "idempotencyKey": "optional", "items": [ { "...": "same as ingest, without idempotencyKey" } ] }
```

Up to 100 items. All items are validated before anything is written; one
invalid item rejects the whole batch with `400 validation` and a `details`
array naming the failing indexes. Returns `200` with a per-item result
list. With a batch key, a retry of the same
batch after a partial failure finishes the unfinished items and does not
duplicate the finished ones. An item that changed since the first attempt
gets `422 idempotency-key-reused` in its result.

### `POST /api/v1/recall`

```json
{
  "projectId": "0192...uuid",
  "query": "why did we pick S3 Vectors",
  "limit": 20,
  "level": "medium",
  "occurredFrom": "2026-01-01T00:00:00Z",
  "sort": "relevance"
}
```

| Field            | Notes                                                                        |
|------------------|------------------------------------------------------------------------------|
| `query`          | required, natural language, any supported language                           |
| `limit`          | 1 to 200, default 100                                                        |
| `level`          | token budget: `low` 8k (default), `medium` 16k, `high` 24k, `extra-high` 32k |
| `maxTokens`      | explicit budget instead of `level`; never both                               |
| `queryTimestamp` | only datapoints at or before this instant are eligible; default now          |
| `occurredFrom`   | inclusive start of the time window                                           |
| `occurredTo`     | inclusive end; start after end is `400 recall-range-invalid`                 |
| `sort`           | `relevance` (default) or `chronological`, oldest first with `rank` kept      |

Returns the selected datapoints with `rank`, `score` and `contentTokens`,
plus `usedTokens` and `budgetTokens`. Documents are never truncated; a
document that does not fit the remaining budget is skipped.

### `GET /healthz`

Always `200`. The body is `{"status": "ok"}` once the search index has
been rebuilt from S3 and `{"status": "loading"}` before that. Ingest and
recall return `503` with code `loading` until then.

## Not yet

- Auth
- Filtering on `audience`
- Update and delete
- Metadata filters on recall

## License

MIT, see [LICENSE](LICENSE).
