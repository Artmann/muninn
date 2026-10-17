pub mod error;

use axum::Router;
use axum::extract::FromRequest;
use axum::routing::get;

pub use error::ApiError;

#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct Json<T>(pub T);

async fn healthz() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "status": "ok" }))
}

pub fn health_router() -> Router<()> {
    Router::new().route("/healthz", get(healthz))
}
