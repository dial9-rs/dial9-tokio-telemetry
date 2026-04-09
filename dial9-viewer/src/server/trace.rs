use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use flate2::read::GzDecoder;
use serde::Deserialize;
use std::io::Read;

use crate::server::AppState;

const MAX_RESPONSE_BYTES: usize = 50 * 1024 * 1024; // 50 MB
const MAX_KEYS: usize = 100;

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
    if keys.len() > MAX_KEYS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("too many keys (max {MAX_KEYS})"),
        ));
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
        let mut buf = [0u8; 8192];
        loop {
            match decoder.read(&mut buf) {
                Ok(0) => return decompressed,
                Ok(n) => {
                    decompressed.extend_from_slice(&buf[..n]);
                    if decompressed.len() > MAX_RESPONSE_BYTES {
                        tracing::warn!(
                            size = decompressed.len(),
                            "decompressed data exceeds limit, truncating"
                        );
                        return decompressed;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "gzip header detected but decompression failed, returning raw bytes"
                    );
                    return data.to_vec();
                }
            }
        }
    }
    data.to_vec()
}
