use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::server::AppState;

#[derive(Serialize)]
pub struct ConfigResponse {
    pub default_bucket: Option<String>,
    pub default_prefix: Option<String>,
}

pub async fn get_config(State(state): State<AppState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        default_bucket: state.default_bucket.clone(),
        default_prefix: state.default_prefix.clone(),
    })
}
