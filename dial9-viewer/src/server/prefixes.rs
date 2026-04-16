use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::server::AppState;

#[derive(Deserialize)]
pub struct PrefixParams {
    pub bucket: Option<String>,
    pub prefix: Option<String>,
}

pub async fn list_prefixes(
    State(state): State<AppState>,
    Query(params): Query<PrefixParams>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    let bucket = params
        .bucket
        .or(state.default_bucket.clone())
        .ok_or((StatusCode::BAD_REQUEST, "bucket is required".to_string()))?;

    let prefix = params.prefix.unwrap_or_default();

    let prefixes = state
        .backend
        .list_prefixes(&bucket, &prefix)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(prefixes))
}
