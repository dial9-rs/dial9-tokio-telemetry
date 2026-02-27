use std::collections::HashMap;
use tokio::sync::Mutex;

use crate::ddb::DdbClient;

#[derive(Default)]
struct Aggregate {
    sum: f64,
    count: u64,
    min: f64,
    max: f64,
}

impl Aggregate {
    fn record(&mut self, value: f64) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.sum += value;
        self.count += 1;
    }
}

pub struct MetricsBuffer {
    inner: Mutex<HashMap<String, Aggregate>>,
}

impl Default for MetricsBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsBuffer {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub async fn record(&self, name: String, value: f64) {
        self.inner
            .lock()
            .await
            .entry(name)
            .or_default()
            .record(value);
    }

    pub async fn flush_to_ddb(&self, ddb: &DdbClient) {
        let snapshot: HashMap<String, (f64, u64, f64, f64)> = {
            let mut guard = self.inner.lock().await;
            guard
                .drain()
                .map(|(k, v)| (k, (v.sum, v.count, v.min, v.max)))
                .collect()
        };

        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        for (name, (sum, count, min, max)) in snapshot {
            if let Err(e) = ddb.put_aggregate(&name, ts, sum, count, min, max).await {
                eprintln!("flush error for {name}: {e}");
            }
        }
    }
}
