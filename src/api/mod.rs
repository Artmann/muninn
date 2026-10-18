pub mod datapoints;
pub mod delete;
pub mod error;
pub mod ingest;
pub mod locks;
pub mod rate;
pub mod recall;
pub mod state;
pub mod webhooks;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequest, State};
use axum::routing::{get, post};

use crate::webhook::MAX_BODY_BYTES as MAX_WEBHOOK_BODY_BYTES;
use tower_http::trace::TraceLayer;

pub use error::ApiError;
pub use state::AppState;

#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct Json<T>(pub T);

/// Largest request body accepted, in bytes.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

async fn healthz(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    let status = if state.ensure_ready().is_ok() {
        "ok"
    } else {
        "loading"
    };
    axum::Json(serde_json::json!({ "status": status }))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/ingest", post(ingest::handler))
        .route("/api/v1/ingest/batch", post(ingest::batch_handler))
        .route("/api/v1/recall", post(recall::handler))
        .route("/api/v1/datapoints", get(datapoints::list))
        .route("/api/v1/delete", post(delete::handler))
        .route(
            "/api/v1/webhooks",
            post(webhooks::create).get(webhooks::list),
        )
        .route(
            "/api/v1/webhooks/{id}",
            get(webhooks::get)
                .put(webhooks::update)
                .delete(webhooks::delete),
        )
        .route(
            "/api/v1/webhooks/{id}/{token}",
            post(webhooks::receive).layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES)),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
