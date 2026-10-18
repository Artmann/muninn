use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Context;
use muninn::api::AppState;
use muninn::api::locks::KeyLocks;
use muninn::boot;
use muninn::config::Config;
use muninn::embed::Embedder;
use muninn::embed::FastEmbedder;
use muninn::index::{MemoryIndex, S3VectorsIndex, VectorIndex};
use muninn::store::S3Store;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    // Variables already in the environment win over the file.
    match dotenvy::dotenv() {
        Ok(path) => tracing::info!(path = %path.display(), "loaded environment file"),
        Err(e) if e.not_found() => {}
        Err(e) => return Err(e).context("reading .env"),
    }
    let config = Config::from_env()?;

    let (choice, cache) = (config.embed_model, config.model_cache.clone());
    tracing::info!(model = choice.name(), "loading embedding model");
    let embedder = tokio::task::spawn_blocking(move || FastEmbedder::new(choice, &cache))
        .await
        .context("model loading panicked")??;

    // Used by the Docker build to bake the model into the image.
    if std::env::var("MUNINN_DOWNLOAD_ONLY").as_deref() == Ok("1") {
        tracing::info!(cache = %config.model_cache.display(), "model downloaded");
        return Ok(());
    }

    let sdk = S3Store::load_sdk().await?;
    let store = S3Store::from_sdk(&sdk, &config);
    store.ensure_bucket().await?;

    let index: Arc<dyn VectorIndex> = match &config.vector_bucket {
        Some(bucket) => {
            let idx = S3VectorsIndex::connect(
                &sdk,
                bucket.clone(),
                embedder.model_name(),
                embedder.dim(),
            )
            .await?;
            tracing::info!(bucket, index = idx.index(), "using S3 Vectors index");
            Arc::new(idx)
        }
        None => Arc::new(MemoryIndex::new()),
    };

    let state = AppState {
        store,
        index,
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
