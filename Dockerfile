# syntax=docker/dockerfile:1
FROM rust:1-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cache mounts keep the registry and compiled dependencies between builds,
# so a source change only recompiles muninn itself.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
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
