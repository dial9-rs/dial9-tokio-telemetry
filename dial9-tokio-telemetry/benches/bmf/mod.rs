//! Bencher Metric Format (BMF) helpers.
//! Spec: <https://bencher.dev/docs/reference/bencher-metric-format/>

use serde::Serialize;
use std::collections::BTreeMap;

pub type Report = BTreeMap<String, Metric>;

#[derive(Serialize)]
pub struct Metric {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<Measure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throughput: Option<Measure>,
}

#[derive(Serialize)]
pub struct Measure {
    pub value: f64,
}

impl Metric {
    pub fn latency(value: f64) -> Self {
        Self {
            latency: Some(Measure { value }),
            throughput: None,
        }
    }
    pub fn throughput(value: f64) -> Self {
        Self {
            latency: None,
            throughput: Some(Measure { value }),
        }
    }
}
