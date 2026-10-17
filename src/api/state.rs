use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::api::ApiError;
use crate::api::locks::KeyLocks;
use crate::embed::Embedder;
use crate::index::VectorIndex;
use crate::store::S3Store;

/// Everything a request handler needs.
#[derive(Clone)]
pub struct AppState {
    pub store: S3Store,
    pub index: Arc<dyn VectorIndex>,
    pub embedder: Arc<dyn Embedder>,
    /// False until the index has been rebuilt from storage.
    pub ready: Arc<AtomicBool>,
    pub locks: KeyLocks,
}

impl AppState {
    pub fn ensure_ready(&self) -> Result<(), ApiError> {
        if self.ready.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(ApiError::Loading)
        }
    }
}
