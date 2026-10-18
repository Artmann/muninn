use std::sync::Arc;

use anyhow::Context;

use crate::datapoints::{Meta, StoredEmbedding, content_hash};
use crate::embed::{Embedder, build_input};
use crate::index::VectorIndex;
use crate::store::S3Store;
use crate::tokens::count_tokens;

/// Vectors handed to the index per call during a rebuild.
const BATCH: usize = 500;

/// Load every stored datapoint into the index. Datapoints embedded with a
/// different model are re-embedded and their `meta.json` rewritten.
/// Returns how many datapoints were loaded.
pub async fn rebuild_index(
    store: &S3Store,
    index: &dyn VectorIndex,
    embedder: Arc<dyn Embedder>,
) -> anyhow::Result<usize> {
    let metas = store.list_metas().await?;
    let total = metas.len();
    let mut loaded = 0;
    let mut reembedded = 0;
    let mut batch = Vec::with_capacity(BATCH);
    for mut meta in metas {
        if meta.embedding.model != embedder.model_name() {
            let Some(content) = store.get_content(meta.project_id, meta.id).await? else {
                tracing::warn!(project = %meta.project_id, id = %meta.id, "metadata without content; skipping");
                continue;
            };
            meta = reembed(meta, content, embedder.clone()).await?;
            store.put_meta(&meta).await?;
            reembedded += 1;
        }
        batch.push((meta.project_id, meta.id, meta.embedding.vector));
        if batch.len() == BATCH {
            index.upsert_many(std::mem::take(&mut batch)).await?;
        }
        loaded += 1;
    }
    index.upsert_many(batch).await?;
    tracing::info!(total, loaded, reembedded, "index rebuilt");
    Ok(loaded)
}

async fn reembed(
    mut meta: Meta,
    content: String,
    embedder: Arc<dyn Embedder>,
) -> anyhow::Result<Meta> {
    let input = build_input(&meta.label, &content, &meta.metadata);
    let model = embedder.model_name().to_string();
    let vector = tokio::task::spawn_blocking(move || embedder.embed_passages(&[input]))
        .await
        .context("embedding task panicked")??
        .pop()
        .context("embedder returned no vector")?;
    meta.embedding = StoredEmbedding {
        model,
        dim: vector.len(),
        vector,
    };
    meta.content_tokens = count_tokens(&content);
    meta.content_sha256 = Some(content_hash(&content));
    Ok(meta)
}

/// Rebuild in the background and flip `ready` when done. Exits the process
/// if the rebuild fails, since serving with a partial index would silently
/// miss memories.
pub fn spawn_rebuild(
    store: S3Store,
    index: Arc<dyn VectorIndex>,
    embedder: Arc<dyn Embedder>,
    ready: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match rebuild_index(&store, index.as_ref(), embedder).await {
            Ok(count) => {
                tracing::info!(count, "ready");
                ready.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Err(e) => {
                tracing::error!(error = ?e, "index rebuild failed");
                std::process::exit(1);
            }
        }
    })
}
