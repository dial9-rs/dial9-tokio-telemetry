use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
};
use dial9_tokio_telemetry::telemetry::{Dial9EntrySink, Span};
use metrique::timers::Timer;
use metrique::unit::Millisecond;
use metrique::unit_of_work::metrics;
use metrique::writer::value::ToString as MetriqueToString;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

use crate::AppState;

#[metrics(rename_all = "PascalCase")]
struct RequestEntry {
    #[metrics(timestamp)]
    timestamp: SystemTime,
    operation: &'static str,
    #[metrics(format = MetriqueToString)]
    status: u16,
    #[metrics(unit = Millisecond)]
    duration: Span<Timer>,
}

static SINK: Dial9EntrySink<metrique::RootMetric<RequestEntry>> =
    Dial9EntrySink::new("RequestEntry");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/metrics", post(record_metric))
        .route("/metrics/{name}", get(query_metric))
        .route("/terminate", post(terminate))
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
    let _entry = RequestEntry {
        timestamp: SystemTime::now(),
        operation: "RecordMetric",
        status: 202,
        duration: Timer::start_now().into(),
    }
    .append_on_drop(&SINK);
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
    let _entry = RequestEntry {
        timestamp: SystemTime::now(),
        operation: "QueryMetric",
        status: 200,
        duration: Timer::start_now().into(),
    }
    .append_on_drop(&SINK);
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

async fn terminate(State(state): State<AppState>) -> StatusCode {
    println!("Received /terminate – initiating graceful shutdown.");
    state.shutdown.cancel();
    StatusCode::OK
}
