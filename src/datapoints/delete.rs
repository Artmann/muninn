use futures::{StreamExt, TryStreamExt};
use uuid::Uuid;

use super::{DatapointService, datapoint_lock, detached};
use crate::datapoints::source_uuid;
use crate::error::ServiceError;

/// Most ids accepted in one delete call.
pub const MAX_DELETE: usize = 1000;

/// Datapoints to delete, by the caller's `sourceId` or by server id.
#[derive(Debug)]
pub struct DeleteCommand {
    pub project_id: Uuid,
    pub source_ids: Vec<String>,
    pub ids: Vec<Uuid>,
}

#[derive(Debug)]
pub struct Deleted {
    pub id: Uuid,
    pub source_id: Option<String>,
    /// False when nothing was stored under the id.
    pub deleted: bool,
}

impl DatapointService {
    /// Delete datapoints by `sourceId` or server `id`. Results come back in
    /// command order, `source_ids` first.
    pub async fn delete(&self, cmd: DeleteCommand) -> Result<Vec<Deleted>, ServiceError> {
        self.infra.ensure_ready()?;
        let total = cmd.source_ids.len() + cmd.ids.len();
        if !(1..=MAX_DELETE).contains(&total) {
            return Err(ServiceError::Validation(format!(
                "send between 1 and {MAX_DELETE} sourceIds and ids in total, got {total}"
            )));
        }
        if cmd.source_ids.iter().any(|s| s.trim().is_empty()) {
            return Err(ServiceError::Validation(
                "sourceIds must not be empty".into(),
            ));
        }
        let project = cmd.project_id;
        let targets = cmd
            .source_ids
            .into_iter()
            .map(|s| (source_uuid(project, &s), Some(s)))
            .chain(cmd.ids.into_iter().map(|id| (id, None)));
        let results = futures::stream::iter(targets)
            .map(|(id, source_id)| async move {
                let this = self.clone();
                let deleted = detached(async move { this.delete_one(project, id).await }).await?;
                anyhow::Ok(Deleted {
                    id,
                    source_id,
                    deleted,
                })
            })
            .buffered(16)
            .try_collect()
            .await?;
        Ok(results)
    }

    /// Remove from the index first, so a crash part way leaves a stored
    /// datapoint that the next start puts back in the index, never a vector
    /// pointing at nothing.
    async fn delete_one(&self, project: Uuid, id: Uuid) -> anyhow::Result<bool> {
        let _guard = self.locks.lock(datapoint_lock(project, id)).await;
        if !self.infra.store.has_meta(project, id).await? {
            return Ok(false);
        }
        self.infra.index.remove(project, id).await?;
        self.infra.store.delete_datapoint(project, id).await?;
        Ok(true)
    }
}
