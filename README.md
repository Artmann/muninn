# Muninn

A memory service for AI agents. Agents write datapoints as things happen
and later ask "what do we know about X" to get back the closest matches,
scoped to a project, filtered by time, and fitted into a token budget they
can paste straight into a prompt.

- Semantic recall with a local, multilingual embedding model. No external
  AI API.
- S3 as the only storage backend.
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
docker build -t muninn --build-arg MUNINN_EMBED_MODEL=bge-m3 .   # another model
docker run -p 8080:8080 --env-file .env.example   -e AWS_ENDPOINT_URL=http://host.docker.internal:4566 muninn
```

Against real S3, pass your AWS credentials and region instead and leave
`AWS_ENDPOINT_URL` unset.

## Configuration

| Variable             | Default                | Purpose                                            |
|----------------------|------------------------|----------------------------------------------------|
| `MUNINN_BUCKET`      | `muninn`               | S3 bucket; created on startup if missing           |
| `MUNINN_BIND`        | `0.0.0.0:8080`         | listen address                                     |
| `MUNINN_EMBED_MODEL` | `multilingual-e5-small`| embedding model, see below                         |
| `MUNINN_MODEL_CACHE` | `./.fastembed_cache`   | where model files are downloaded (`/models` in Docker) |
| `MUNINN_DOWNLOAD_ONLY` |                      | `1`: download the model and exit without serving   |
| `AWS_ENDPOINT_URL`   |                        | set to `http://localhost:4566` for LocalStack      |
| `AWS_*`              |                        | standard credentials and region                    |

### Embedding models

| `MUNINN_EMBED_MODEL`    | Languages | Max input  | Download | Notes                              |
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

`projectId`, `label` and `content` are required. `timestamp` defaults to
now, `metadata` to `{}`. Returns `201` with the stored datapoint, or `200`
with `"deduplicated": true` if the idempotency key was seen before in this
project. The server assigns the `id`. An empty `idempotencyKey` is rejected; omit the
field instead.

### `POST /api/v1/ingest/batch`

```json
{ "idempotencyKey": "optional", "items": [ { "...": "same as ingest, without idempotencyKey" } ] }
```

Up to 100 items. All items are validated before anything is written.
Returns `200` with a per-item result list. With a batch key, a retry after
a partial failure finishes the unfinished items and does not duplicate the
finished ones.

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
- Amazon S3 Vectors backend

## License

MIT, see [LICENSE](LICENSE).
