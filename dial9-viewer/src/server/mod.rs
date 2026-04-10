use crate::storage::StorageBackend;
use axum::Router;
use std::path::Path;
use std::sync::Arc;
use tower_http::services::ServeDir;

mod config;
mod search;
mod trace;

#[derive(Clone)]
#[non_exhaustive]
pub struct AppState {
    pub backend: Arc<dyn StorageBackend>,
    pub default_bucket: Option<String>,
    pub default_prefix: Option<String>,
}

impl AppState {
    pub fn new(
        backend: Arc<dyn StorageBackend>,
        default_bucket: Option<String>,
        default_prefix: Option<String>,
    ) -> Self {
        Self {
            backend,
            default_bucket,
            default_prefix,
        }
    }
}

pub fn router(state: AppState, ui_dir: &Path) -> Router {
    Router::new()
        .nest("/api", api_router(state))
        .fallback_service(ServeDir::new(ui_dir))
}

fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/config", axum::routing::get(config::get_config))
        .route("/search", axum::routing::get(search::search))
        .route("/trace", axum::routing::get(trace::get_trace))
        .with_state(state)
}
