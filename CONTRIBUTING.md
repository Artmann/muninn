# Contributing

## Prerequisites

- Rust stable (edition 2024)
- Docker, for LocalStack. The compose file pins `localstack/localstack:4.14`
  because releases from 2026.03 on refuse to start without a LocalStack
  auth token.
- About 500 MB of disk for the default embedding model

## Local setup

```sh
docker compose up -d        # LocalStack with S3 on localhost:4566
cp .env.example .env        # if present; otherwise export the vars below
cargo run
```

Environment for local dev:

```sh
AWS_ENDPOINT_URL=http://localhost:4566
AWS_ACCESS_KEY_ID=test
AWS_SECRET_ACCESS_KEY=test
AWS_REGION=us-east-1
```

The bucket is created on startup if it does not exist. The embedding model
is downloaded into `./.fastembed_cache` on first run and is gitignored.

Smoke test:

```sh
curl -s localhost:8080/healthz
curl -s -X POST localhost:8080/api/v1/ingest -H 'content-type: application/json' \
  -d '{"projectId":"019a0000-0000-7000-8000-000000000000","label":"hello","content":"first memory"}'
curl -s -X POST localhost:8080/api/v1/recall -H 'content-type: application/json' \
  -d '{"projectId":"019a0000-0000-7000-8000-000000000000","query":"hello"}'
```

## Tests

```sh
cargo test                       # unit tests, no S3 or model needed
set -a; . ./.env.example; set +a
cargo test -- --ignored          # integration tests against LocalStack
```

Unit tests cover the recall selection algorithm, embedding input
construction, and the in-memory index, using a fake embedder. Integration
tests need LocalStack running and exercise ingest, dedupe, and index
rebuild after restart.

## Code layout

```
src/main.rs          config, wiring, server
src/api/             handlers, request/response types, error mapping
src/domain.rs        Datapoint, validation
src/embed.rs         Embedder trait + fastembed impl
src/store.rs         S3 document store (content, meta, idempotency)
src/index/mod.rs     VectorIndex trait
src/index/memory.rs  in-memory impl
src/recall.rs        the recall algorithm, pure over loaded metas
src/tokens.rs        token counting
```

Keep the recall algorithm in `recall.rs` pure: it takes already-loaded
metadata and scores and returns the selection. That is what makes it
unit-testable without S3.

## Design docs

Design decisions live in `docs/superpowers/specs/`. The current design is
[2026-10-17-memory-service-design.md](docs/superpowers/specs/2026-10-17-memory-service-design.md).
If a change alters the HTTP contract or storage layout, update the design
doc and [ARCHITECTURE.md](ARCHITECTURE.md) in the same PR.

## Conventions

- `cargo fmt` and `cargo clippy -- -D warnings` must pass.
- Errors returned to clients use the `{ "error": { "code", "message" } }`
  envelope; add new codes to the table in ARCHITECTURE.md.
- Commits: short imperative subject line. No generated co-author trailers.

## Adding an embedding model

Add a variant to the model enum in `src/embed.rs` mapping the config name
to the fastembed model, its dimension, and its query/passage prefix rule.
Document it in the README table. Stored data records the model name, so
existing datapoints are re-embedded on the next start after a switch.
