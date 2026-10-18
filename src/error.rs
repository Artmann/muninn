/// Why one item of a batch is invalid.
#[derive(Debug)]
pub struct ItemError {
    pub index: usize,
    pub message: String,
}

/// Why a use case failed, in domain terms. The API maps these to statuses
/// and error codes.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("{0}")]
    Validation(String),
    #[error("{message}")]
    ValidationDetailed {
        message: String,
        details: Vec<ItemError>,
    },
    #[error("occurredFrom must not be after occurredTo")]
    RecallRangeInvalid,
    #[error("send level or maxTokens, not both")]
    BudgetConflict,
    #[error("a batch holds at most 100 items")]
    BatchTooLarge,
    #[error("idempotencyKey was already used with a different request; use a new key")]
    IdempotencyKeyReused,
    #[error("{0}")]
    NotFound(String),
    #[error("this webhook is paused")]
    WebhookPaused,
    #[error("too many events for this webhook; at most 600 a minute")]
    RateLimited,
    #[error("index is still loading from storage")]
    Loading,
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}
