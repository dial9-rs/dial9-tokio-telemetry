use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::server::AppState;
use crate::storage::ObjectInfo;

#[derive(Deserialize)]
pub struct SearchParams {
    /// Search query — used as S3 prefix in MVP
    pub q: Option<String>,
    pub bucket: Option<String>,
}

pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<ObjectInfo>>, (StatusCode, String)> {
    let bucket = params
        .bucket
        .or(state.default_bucket.clone())
        .ok_or((StatusCode::BAD_REQUEST, "bucket is required".to_string()))?;

    let prefix = match (&state.default_prefix, &params.q) {
        (Some(pfx), Some(q)) => format!("{pfx}/{q}"),
        (Some(pfx), None) => pfx.clone(),
        (None, Some(q)) => q.clone(),
        (None, None) => String::new(),
    };

    let objects = state
        .backend
        .list_objects(&bucket, &prefix)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(objects))
}
