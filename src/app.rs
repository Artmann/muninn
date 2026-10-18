//! Wires each domain's service to the shared infrastructure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::datapoints::DatapointService;
use crate::embed::Embedder;
use crate::error::ServiceError;
use crate::index::VectorIndex;
use crate::locks::KeyLocks;
use crate::recall::RecallService;
use crate::store::S3Store;
use crate::webhooks::WebhookService;

/// The infrastructure the services run on.
#[derive(Clone)]
pub struct Infra {
    pub store: S3Store,
    pub index: Arc<dyn VectorIndex>,
    pub embedder: Arc<dyn Embedder>,
    /// False until the index has been rebuilt from storage.
    pub ready: Arc<AtomicBool>,
}

impl Infra {
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    pub(crate) fn ensure_ready(&self) -> Result<(), ServiceError> {
        if self.is_ready() {
            Ok(())
        } else {
            Err(ServiceError::Loading)
        }
    }
}

/// Every service, wired to one set of infrastructure.
#[derive(Clone)]
pub struct Services {
    pub datapoints: DatapointService,
    pub recall: RecallService,
    pub webhooks: WebhookService,
    infra: Infra,
}

impl Services {
    pub fn new(infra: Infra) -> Services {
        let locks = KeyLocks::default();
        let datapoints = DatapointService::new(infra.clone(), locks.clone());
        Services {
            recall: RecallService::new(infra.clone()),
            webhooks: WebhookService::new(infra.clone(), locks, datapoints.clone()),
            datapoints,
            infra,
        }
    }

    /// Whether the index has been rebuilt from storage.
    pub fn is_ready(&self) -> bool {
        self.infra.is_ready()
    }
}
