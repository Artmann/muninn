use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::error::ServiceError;

/// A failed request: a service error, or a request that never reached a
/// service.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    /// A body, path or query string that does not parse.
    #[error("{0}")]
    Malformed(String),
    #[error("request body is larger than 10 MiB")]
    PayloadTooLarge,
    #[error("webhook body is larger than 1 MiB")]
    WebhookBodyTooLarge,
}

impl ApiError {
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::Service(e) => match e {
                ServiceError::Validation(_) | ServiceError::ValidationDetailed { .. } => {
                    "validation"
                }
                ServiceError::RecallRangeInvalid => "recall-range-invalid",
                ServiceError::BudgetConflict => "budget-conflict",
                ServiceError::BatchTooLarge => "batch-too-large",
                ServiceError::IdempotencyKeyReused => "idempotency-key-reused",
                ServiceError::NotFound(_) => "not-found",
                ServiceError::WebhookPaused => "webhook-paused",
                ServiceError::RateLimited => "rate-limited",
                ServiceError::Loading => "loading",
                ServiceError::Internal(_) => "internal",
            },
            ApiError::Malformed(_) => "validation",
            ApiError::PayloadTooLarge => "payload-too-large",
            ApiError::WebhookBodyTooLarge => "webhook-body-too-large",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::Service(e) => match e {
                ServiceError::Validation(_)
                | ServiceError::ValidationDetailed { .. }
                | ServiceError::RecallRangeInvalid
                | ServiceError::BudgetConflict
                | ServiceError::BatchTooLarge => StatusCode::BAD_REQUEST,
                ServiceError::IdempotencyKeyReused => StatusCode::UNPROCESSABLE_ENTITY,
                ServiceError::NotFound(_) => StatusCode::NOT_FOUND,
                ServiceError::WebhookPaused => StatusCode::CONFLICT,
                ServiceError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
                ServiceError::Loading => StatusCode::SERVICE_UNAVAILABLE,
                ServiceError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            },
            ApiError::Malformed(_) => StatusCode::BAD_REQUEST,
            ApiError::PayloadTooLarge | ApiError::WebhookBodyTooLarge => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
        }
    }

    pub fn to_body(&self) -> serde_json::Value {
        let mut error = serde_json::json!({ "code": self.code(), "message": self.to_string() });
        if let ApiError::Service(ServiceError::ValidationDetailed { details, .. }) = self {
            error["details"] = details
                .iter()
                .map(|d| serde_json::json!({ "index": d.index, "message": d.message }))
                .collect();
        }
        serde_json::json!({ "error": error })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Service(ServiceError::Internal(e)) = &self {
            tracing::error!(error = ?e, "internal error");
        }
        (self.status(), axum::Json(self.to_body())).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(r: JsonRejection) -> Self {
        if r.status() == StatusCode::PAYLOAD_TOO_LARGE {
            return ApiError::PayloadTooLarge;
        }
        ApiError::Malformed(r.body_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ItemError;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;

    async fn body_json(e: impl Into<ApiError>) -> (StatusCode, serde_json::Value) {
        let resp = e.into().into_response();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn envelope_shape_and_status() {
        let (status, body) = body_json(ServiceError::RecallRangeInvalid).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "recall-range-invalid");
        assert!(body["error"]["message"].is_string());
    }

    #[tokio::test]
    async fn internal_hides_details() {
        let (status, body) =
            body_json(ServiceError::Internal(anyhow::anyhow!("secret s3 thing"))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["message"], "internal error");
        assert_eq!(body["error"]["code"], "internal");
    }

    #[tokio::test]
    async fn detailed_validation_includes_details() {
        let e = ServiceError::ValidationDetailed {
            message: "bad items".into(),
            details: vec![ItemError {
                index: 2,
                message: "label empty".into(),
            }],
        };
        let (status, body) = body_json(e).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "validation");
        assert_eq!(body["error"]["details"][0]["index"], 2);
        assert_eq!(body["error"]["details"][0]["message"], "label empty");
    }

    #[tokio::test]
    async fn codes_and_statuses() {
        let api = ApiError::from;
        assert_eq!(
            api(ServiceError::Loading).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(api(ServiceError::Loading).code(), "loading");
        assert_eq!(
            api(ServiceError::IdempotencyKeyReused).code(),
            "idempotency-key-reused"
        );
        assert_eq!(
            api(ServiceError::IdempotencyKeyReused).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(api(ServiceError::BudgetConflict).code(), "budget-conflict");
        assert_eq!(api(ServiceError::BatchTooLarge).code(), "batch-too-large");
        assert_eq!(
            api(ServiceError::Validation("x".into())).code(),
            "validation"
        );
        assert_eq!(ApiError::Malformed("x".into()).code(), "validation");
        assert_eq!(
            ApiError::PayloadTooLarge.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}
