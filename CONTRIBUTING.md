# Contributing

## Prerequisites

- Rust stable (edition 2024)
- Docker, for LocalStack. The compose file pins `localstack/localstack:4.14`
  because releases from 2026.03 on refuse to start without a LocalStack
  auth token.
- About 500 MB of disk for the default embedding model

## Local setup

```sh
docker compose up -d                 # LocalStack with S3 on localhost:4566
cp .env.example .env                 # AWS_* vars pointing at LocalStack
cargo run
```

`.env.example` holds the LocalStack endpoint and dummy credentials. The
service loads `.env` from the working directory at startup; variables
already set in the environment take precedence. Tests do not read `.env`,
so source it for integration runs as shown below.

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
cargo test                                # unit tests, no S3 or model needed
set -a; . ./.env.example; set +a
cargo test -- --ignored                   # integration tests against LocalStack
cargo test --test ingest -- --ignored     # one suite: store, ingest, recall, boot, real_model
cargo test --test e2e -- --ignored        # end to end: the real binary over HTTP
```

Unit tests cover the recall selection algorithm, embedding input
construction, and the in-memory index, using a fake embedder. Integration
tests need LocalStack running; each test gets its own fresh bucket. They
exercise the S3 store, ingest and dedupe, recall, and index rebuild.
`real_model` loads the real `multilingual-e5-small` model (downloading it
on first run) and checks a Swedish passage matches an English query.

The `e2e` suite starts the real `muninn` binary on a random port, with the
real model and a fresh bucket, and talks to it over HTTP. It checks that
realistic English and Swedish queries find the right memory in their top
three, and covers restart, time filters, budget, sort and batch retries.
Server logs go to `target/e2e-logs/`. `e2e_model_switch` is kept separate
because it downloads a second model (`bge-small-en-v1.5`, about 130 MB);
run it with `cargo test --test e2e_model_switch -- --ignored`.

If `cargo run` is holding `target/debug/muninn.exe` on Windows, the build
can't replace it; stop the server or use `CARGO_TARGET_DIR=target/e2e`.

## Docker

```sh
docker build -t muninn .
docker run --rm -p 8080:8080 --env-file .env.example   -e AWS_ENDPOINT_URL=http://host.docker.internal:4566 muninn
```

The build runs the binary once with `MUNINN_DOWNLOAD_ONLY=1` to bake the
model into `/models`. Both stages use Debian trixie: the prebuilt ONNX
Runtime that `ort` links needs glibc 2.38 or newer, which bookworm lacks.

## Code layout

```
src/main.rs          wiring, download-only mode, server
src/config.rs        environment variables
src/boot.rs          index rebuild from S3 at startup
src/api/             handlers, request/response types, error mapping, key locks
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

- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must pass.
- Errors returned to clients use the `{ "error": { "code", "message" } }`
  envelope; add new codes to the table in ARCHITECTURE.md.
- Commits: short imperative subject line. No generated co-author trailers.

## Adding an embedding model

Add a variant to the model enum in `src/embed.rs` mapping the config name
to the fastembed model, its dimension, and its query/passage prefix rule.
Document it in the README table. Stored data records the model name, so
existing datapoints are re-embedded on the next start after a switch.
