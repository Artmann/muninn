# syntax=docker/dockerfile:1
FROM rust:1-trixie AS chef
RUN cargo install cargo-chef --locked --version ^0.1
WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# Dependencies are built in their own layer from the recipe, which only
# changes with Cargo.toml and Cargo.lock. A source change reuses that layer,
# locally and from the CI layer cache, and only recompiles muninn itself.
FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked \
    && cp target/release/muninn /usr/local/bin/muninn

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3t64 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home muninn \
    && mkdir /models && chown muninn /models
COPY --from=build /usr/local/bin/muninn /usr/local/bin/muninn
USER muninn
WORKDIR /models

ARG EMBED_MODEL=multilingual-e5-small
ENV EMBED_MODEL=${EMBED_MODEL} \
    MODEL_CACHE=/models
# Bake the model into the image so containers start without network
# access to the model hub.
RUN DOWNLOAD_ONLY=1 /usr/local/bin/muninn

EXPOSE 8080
CMD ["muninn"]
