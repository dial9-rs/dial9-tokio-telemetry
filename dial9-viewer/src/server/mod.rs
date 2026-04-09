use crate::storage::StorageBackend;
use axum::Router;
use std::path::Path;
use std::sync::Arc;
use tower_http::services::ServeDir;

mod search;
mod trace;

#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn StorageBackend>,
    pub default_bucket: Option<String>,
    pub default_prefix: Option<String>,
}

pub fn router(state: AppState, ui_dir: &Path) -> Router {
    Router::new()
        .nest("/api", api_router(state))
        .fallback_service(ServeDir::new(ui_dir))
}

fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/search", axum::routing::get(search::search))
        .route("/trace", axum::routing::get(trace::get_trace))
        .with_state(state)
}
