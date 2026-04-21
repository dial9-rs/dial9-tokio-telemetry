use crate::storage::StorageBackend;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;
use std::path::PathBuf;
use std::sync::Arc;

mod config;
mod prefixes;
mod search;
mod trace;

#[derive(Embed)]
#[folder = "ui/"]
struct UiAssets;

#[derive(Clone)]
#[non_exhaustive]
pub struct AppState {
    pub backend: Arc<dyn StorageBackend>,
    pub default_bucket: Option<String>,
    pub default_prefix: Option<String>,
    /// When set, serve UI files from disk instead of embedded assets.
    pub dev_ui_dir: Option<PathBuf>,
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
            dev_ui_dir: None,
        }
    }

    pub fn with_dev_ui_dir(mut self, dir: PathBuf) -> Self {
        self.dev_ui_dir = Some(dir);
        self
    }
}

pub fn router(state: AppState) -> Router {
    if let Some(dir) = state.dev_ui_dir.clone() {
        tracing::info!(path = %dir.display(), "serving UI from disk (dev mode)");
        Router::new()
            .nest("/api", api_router(state))
            .fallback_service(tower_http::services::ServeDir::new(dir))
    } else {
        Router::new()
            .nest("/api", api_router(state))
            .fallback(serve_embedded)
    }
}

async fn serve_embedded(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match UiAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_str(mime.as_ref()).unwrap(),
                )
                .body(Body::from(file.data.to_vec()))
                .unwrap()
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/config", axum::routing::get(config::get_config))
        .route("/prefixes", axum::routing::get(prefixes::list_prefixes))
        .route("/search", axum::routing::get(search::search))
        .route("/trace", axum::routing::get(trace::get_trace))
        .with_state(state)
}
