use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Context;
use muninn::api::AppState;
use muninn::api::locks::KeyLocks;
use muninn::boot;
use muninn::config::Config;
use muninn::embed::FastEmbedder;
use muninn::index::MemoryIndex;
use muninn::store::S3Store;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let config = Config::from_env()?;

    let store = S3Store::from_config(&config).await?;
    store.ensure_bucket().await?;

    let (choice, cache) = (config.embed_model, config.model_cache.clone());
    tracing::info!(model = choice.name(), "loading embedding model");
    let embedder = tokio::task::spawn_blocking(move || FastEmbedder::new(choice, &cache))
        .await
        .context("model loading panicked")??;

    let state = AppState {
        store,
        index: Arc::new(MemoryIndex::new()),
        embedder: Arc::new(embedder),
        ready: Arc::new(AtomicBool::new(false)),
        locks: KeyLocks::default(),
    };

    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(bind = %config.bind, "listening; rebuilding index");
    boot::spawn_rebuild(
        state.store.clone(),
        state.index.clone(),
        state.embedder.clone(),
        state.ready.clone(),
    );
    axum::serve(listener, muninn::api::router(state)).await?;
    Ok(())
}
