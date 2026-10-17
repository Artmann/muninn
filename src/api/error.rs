use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ItemError {
    pub index: usize,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
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
    #[error("index is still loading from storage")]
    Loading,
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl ApiError {
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::Validation(_) | ApiError::ValidationDetailed { .. } => "validation",
            ApiError::RecallRangeInvalid => "recall-range-invalid",
            ApiError::BudgetConflict => "budget-conflict",
            ApiError::BatchTooLarge => "batch-too-large",
            ApiError::Loading => "loading",
            ApiError::Internal(_) => "internal",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::Validation(_)
            | ApiError::ValidationDetailed { .. }
            | ApiError::RecallRangeInvalid
            | ApiError::BudgetConflict
            | ApiError::BatchTooLarge => StatusCode::BAD_REQUEST,
            ApiError::Loading => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn to_body(&self) -> serde_json::Value {
        let mut error = serde_json::json!({ "code": self.code(), "message": self.to_string() });
        if let ApiError::ValidationDetailed { details, .. } = self {
            error["details"] = serde_json::to_value(details).unwrap_or_default();
        }
        serde_json::json!({ "error": error })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(e) = &self {
            tracing::error!(error = ?e, "internal error");
        }
        (self.status(), axum::Json(self.to_body())).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(r: JsonRejection) -> Self {
        ApiError::Validation(r.body_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;

    async fn body_json(e: ApiError) -> (StatusCode, serde_json::Value) {
        let resp = e.into_response();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn envelope_shape_and_status() {
        let (status, body) = body_json(ApiError::RecallRangeInvalid).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "recall-range-invalid");
        assert!(body["error"]["message"].is_string());
    }

    #[tokio::test]
    async fn internal_hides_details() {
        let (status, body) =
            body_json(ApiError::Internal(anyhow::anyhow!("secret s3 thing"))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["message"], "internal error");
        assert_eq!(body["error"]["code"], "internal");
    }

    #[tokio::test]
    async fn detailed_validation_includes_details() {
        let e = ApiError::ValidationDetailed {
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
    }

    #[tokio::test]
    async fn codes_and_statuses() {
        assert_eq!(ApiError::Loading.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(ApiError::Loading.code(), "loading");
        assert_eq!(ApiError::BudgetConflict.code(), "budget-conflict");
        assert_eq!(ApiError::BatchTooLarge.code(), "batch-too-large");
        assert_eq!(ApiError::Validation("x".into()).code(), "validation");
    }
}
