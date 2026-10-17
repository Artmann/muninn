pub mod error;
pub mod ingest;
pub mod locks;
pub mod state;

use axum::Router;
use axum::extract::FromRequest;
use axum::routing::{get, post};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

pub use error::ApiError;
pub use state::AppState;

#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct Json<T>(pub T);

/// Largest request body accepted, in bytes.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

async fn healthz() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "status": "ok" }))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/ingest", post(ingest::handler))
        .route("/api/v1/ingest/batch", post(ingest::batch_handler))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
