//! Datapoints: the memories a project stores. Ingest stores or replaces
//! them, delete removes them, and listing shows what is stored.

pub mod http;

mod delete;
mod ingest;
mod list;
mod model;

pub use delete::{DeleteCommand, Deleted, MAX_DELETE};
pub use ingest::{BatchItem, IngestOutcome, Ingested, MAX_BATCH};
pub use list::{DEFAULT_PAGE_LIMIT, ListQuery, Listed, MAX_PAGE_LIMIT, MAX_SCAN, Page};
pub use model::*;

use uuid::Uuid;

use crate::app::Infra;
use crate::locks::KeyLocks;

/// Stores, replaces, deletes and lists a project's datapoints.
#[derive(Clone)]
pub struct DatapointService {
    infra: Infra,
    locks: KeyLocks,
}

impl DatapointService {
    pub(crate) fn new(infra: Infra, locks: KeyLocks) -> DatapointService {
        DatapointService { infra, locks }
    }
}

/// The in-process lock serialising writes and deletes of one datapoint.
fn datapoint_lock(project: Uuid, id: Uuid) -> String {
    format!("{project}:datapoint:{id}")
}
