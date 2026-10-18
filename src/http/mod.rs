//! What every domain's HTTP handlers share: the router, the JSON extractor
//! and the error envelope. The handlers live with their domain, in its
//! `http` module.

mod error;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequest, State};
use axum::routing::get;
use tower_http::trace::TraceLayer;

use crate::app::Services;
use crate::{datapoints, recall, webhooks};

pub use error::ApiError;

#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct Json<T>(pub T);

/// Largest request body accepted, in bytes.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

async fn healthz(State(services): State<Services>) -> axum::Json<serde_json::Value> {
    let status = if services.is_ready() { "ok" } else { "loading" };
    axum::Json(serde_json::json!({ "status": status }))
}

pub fn router(services: Services) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .merge(datapoints::http::routes())
        .merge(recall::http::routes())
        .merge(webhooks::http::routes())
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(services)
}
