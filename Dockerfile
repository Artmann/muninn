FROM rust:1-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3t64 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/muninn /usr/local/bin/muninn

ARG MUNINN_EMBED_MODEL=multilingual-e5-small
ENV MUNINN_EMBED_MODEL=${MUNINN_EMBED_MODEL} \
    MUNINN_MODEL_CACHE=/models
RUN MUNINN_DOWNLOAD_ONLY=1 /usr/local/bin/muninn

EXPOSE 8080
CMD ["muninn"]
