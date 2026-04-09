use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use flate2::read::GzDecoder;
use serde::Deserialize;
use std::io::Read;

use crate::server::AppState;

const MAX_RESPONSE_BYTES: usize = 50 * 1024 * 1024; // 50 MB

#[derive(Deserialize)]
pub struct TraceParams {
    /// Comma-separated S3 keys
    pub keys: String,
    pub bucket: Option<String>,
}

pub async fn get_trace(
    State(state): State<AppState>,
    Query(params): Query<TraceParams>,
) -> Result<Response, (StatusCode, String)> {
    let bucket = params
        .bucket
        .or(state.default_bucket.clone())
        .ok_or((StatusCode::BAD_REQUEST, "bucket is required".to_string()))?;

    let keys: Vec<&str> = params.keys.split(',').filter(|k| !k.is_empty()).collect();
    if keys.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "keys is required".to_string()));
    }

    let mut combined = Vec::new();

    for key in &keys {
        let data = state
            .backend
            .get_object(&bucket, key)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let raw = maybe_gunzip(&data);

        if combined.len() + raw.len() > MAX_RESPONSE_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("combined trace exceeds {MAX_RESPONSE_BYTES} bytes"),
            ));
        }

        combined.extend_from_slice(&raw);
    }

    Ok(Response::builder()
        .header("content-type", "application/octet-stream")
        .header("content-disposition", "attachment; filename=\"trace.bin\"")
        .body(Body::from(combined))
        .unwrap()
        .into_response())
}

fn maybe_gunzip(data: &[u8]) -> Vec<u8> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        let mut decoder = GzDecoder::new(data);
        let mut decompressed = Vec::new();
        if decoder.read_to_end(&mut decompressed).is_ok() {
            return decompressed;
        }
        tracing::warn!("gzip header detected but decompression failed, returning raw bytes");
    }
    data.to_vec()
}
