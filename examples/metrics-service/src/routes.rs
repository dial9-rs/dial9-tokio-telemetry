use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/metrics", post(record_metric))
        .route("/metrics/{name}", get(query_metric))
        .with_state(state)
}

#[derive(Deserialize)]
struct MetricPayload {
    name: String,
    value: f64,
}

async fn record_metric(
    State(state): State<AppState>,
    Json(payload): Json<MetricPayload>,
) -> StatusCode {
    state.buffer.record(payload.name, payload.value).await;
    StatusCode::ACCEPTED
}

#[derive(Serialize)]
struct AggregateRow {
    timestamp: u64,
    sum: f64,
    count: u64,
    min: f64,
    max: f64,
}

async fn query_metric(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Vec<AggregateRow>>, StatusCode> {
    state
        .ddb
        .query_metric(&name)
        .await
        .map(|rows| {
            Json(
                rows.into_iter()
                    .map(|(timestamp, sum, count, min, max)| AggregateRow {
                        timestamp,
                        sum,
                        count,
                        min,
                        max,
                    })
                    .collect(),
            )
        })
        .map_err(|e| {
            eprintln!("query error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}
