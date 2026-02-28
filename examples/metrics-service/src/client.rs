use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::json;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

const METRICS: &[&str] = &["cpu", "memory", "latency", "error_rate", "queue_depth"];
const MAX_WORKERS: usize = 40;
const THUNDERING_HERD: usize = 200;
const BASELINE: usize = 4;

/// Stats for a single operation (GET or POST) within one tracking scope.
struct OperationStats {
    success: u64,
    failure: u64,
    samples: Vec<Duration>,
}

impl OperationStats {
    fn new() -> Self {
        Self {
            success: 0,
            failure: 0,
            samples: Vec::new(),
        }
    }

    fn record(&mut self, latency: Duration, ok: bool) {
        self.samples.push(latency);
        if ok {
            self.success += 1;
        } else {
            self.failure += 1;
        }
    }

    fn total(&self) -> u64 {
        self.success + self.failure
    }

    fn percentile(&mut self, p: f64) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        self.samples.sort();
        let idx = percentile_idx(self.samples.len(), p);
        Some(self.samples[idx])
    }

    fn report_json(&mut self) -> serde_json::Value {
        json!({
            "success": self.success,
            "failure": self.failure,
            "total": self.total(),
            "count": self.samples.len(),
            "p50_ms": self.percentile(0.50).map(|d| d.as_secs_f64() * 1000.0),
            "p90_ms": self.percentile(0.90).map(|d| d.as_secs_f64() * 1000.0),
            "p99_ms": self.percentile(0.99).map(|d| d.as_secs_f64() * 1000.0),
            "p999_ms": self.percentile(0.999).map(|d| d.as_secs_f64() * 1000.0),
        })
    }

    fn print_line(&mut self, op: &str) {
        let n = self.samples.len();
        if n == 0 {
            println!("  {op:<8} (no data)");
            return;
        }
        self.samples.sort();
        let p50 = self.samples[percentile_idx(n, 0.50)];
        let p90 = self.samples[percentile_idx(n, 0.90)];
        let p99 = self.samples[percentile_idx(n, 0.99)];
        let p999 = self.samples[percentile_idx(n, 0.999)];
        println!(
            "  {op:<8} ok={:<6} err={:<6} n={n:<6} \
             p50={:.2}ms p90={:.2}ms p99={:.2}ms p999={:.2}ms",
            self.success,
            self.failure,
            p50.as_secs_f64() * 1000.0,
            p90.as_secs_f64() * 1000.0,
            p99.as_secs_f64() * 1000.0,
            p999.as_secs_f64() * 1000.0,
        );
    }
}

fn percentile_idx(n: usize, p: f64) -> usize {
    ((n as f64 * p).ceil() as usize).saturating_sub(1)
}

/// Stats keyed by operation name ("GET", "POST").
#[derive(Default)]
struct StatsMap {
    ops: HashMap<String, OperationStats>,
}

impl StatsMap {
    fn record(&mut self, operation: &str, latency: Duration, ok: bool) {
        self.ops
            .entry(operation.to_string())
            .or_insert_with(OperationStats::new)
            .record(latency, ok);
    }

    fn print_report(&mut self, header: &str) {
        println!("{header}");
        let mut keys: Vec<_> = self.ops.keys().cloned().collect();
        keys.sort();
        for key in keys {
            self.ops.get_mut(&key).unwrap().print_line(&key);
        }
    }

    fn report_final_json(&mut self) -> String {
        let mut map = serde_json::Map::new();
        for (op, stats) in self.ops.iter_mut() {
            map.insert(op.clone(), stats.report_json());
        }
        serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap()
    }

    fn clear(&mut self) {
        self.ops.clear();
    }
}

/// Holds both the cumulative (full-run) and window (current concurrency level)
/// stats so workers can record into both with a single lock acquisition.
struct AllStats {
    cumulative: StatsMap,
    window: StatsMap,
}

impl AllStats {
    fn new() -> Self {
        Self {
            cumulative: StatsMap::default(),
            window: StatsMap::default(),
        }
    }

    fn record(&mut self, operation: &str, latency: Duration, ok: bool) {
        self.cumulative.record(operation, latency, ok);
        self.window.record(operation, latency, ok);
    }
}

// ramp up for 3 seconds -> crushing load -> baseline
fn target_concurrency(elapsed: f64) -> usize {
    if elapsed < 3.0 {
        let t = elapsed / 10.0;
        (BASELINE as f64 + t * (MAX_WORKERS - BASELINE) as f64) as usize
    } else if elapsed < 10.0 {
        THUNDERING_HERD
    } else {
        BASELINE
    }
}

/// Outcome of a single HTTP call.
struct WorkResult {
    operation: &'static str,
    latency: Duration,
    ok: bool,
}

pub async fn run(base_url: &str, shutdown: CancellationToken) {
    let client = Arc::new(Client::new());
    let sem = Arc::new(Semaphore::new(0));
    let stats = Arc::new(Mutex::new(AllStats::new()));
    let start = Instant::now();

    // spawn a large pool of workers that each wait for a permit
    for i in 0..THUNDERING_HERD {
        let client = client.clone();
        let sem = sem.clone();
        let stats = stats.clone();
        let base_url = base_url.to_string();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut tick: u64 = i as u64;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    permit = sem.acquire() => {
                        let _permit = permit.unwrap();
                        let result = do_work(&client, &base_url, i, tick).await;
                        stats.lock().await.record(
                            result.operation,
                            result.latency,
                            result.ok,
                        );
                        tick += 1;
                    }
                }
            }
        });
    }

    // coordinator: adjusts semaphore permits to match target concurrency
    // and snapshots stats on every concurrency change
    let mut current = 0usize;
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let target = target_concurrency(elapsed);
        if target != current {
            match target.cmp(&current) {
                std::cmp::Ordering::Greater => {
                    sem.add_permits(target - current);
                }
                std::cmp::Ordering::Less => {
                    let to_remove = current - target;
                    let sem2 = sem.clone();
                    tokio::spawn(async move {
                        for _ in 0..to_remove {
                            sem2.acquire().await.unwrap().forget();
                        }
                    });
                }
                std::cmp::Ordering::Equal => unreachable!(),
            }

            let mut s = stats.lock().await;
            println!("\nconcurrency {current} -> {target} (elapsed {elapsed:.1}s)");
            if current > 0 {
                s.window
                    .print_report(&format!("--- window stats (concurrency {current}) ---"));
            }
            s.cumulative.print_report("--- cumulative ---");
            s.window.clear();
            current = target;
        }
        sleep(Duration::from_millis(500)).await;
    }

    // dump final cumulative stats as JSON (full-run percentiles per operation)
    let mut s = stats.lock().await;
    if current > 0 {
        s.window.print_report(&format!(
            "--- final window stats (concurrency {current}) ---"
        ));
    }
    println!("{}", s.cumulative.report_final_json());
}

async fn do_work(client: &Client, base_url: &str, worker: usize, tick: u64) -> WorkResult {
    let start = Instant::now();
    let metric = METRICS[tick as usize % METRICS.len()];
    let value = (tick as f64 * 1.3 + worker as f64 * 7.7).sin().abs() * 100.0;

    let (operation, ok) = if tick.is_multiple_of(10) {
        let op = "GET";
        let resp = client
            .get(format!("{base_url}/metrics/{metric}"))
            .send()
            .await;
        let ok = resp.is_ok_and(|r| r.status().is_success());
        (op, ok)
    } else {
        let op = "POST";
        let resp = client
            .post(format!("{base_url}/metrics"))
            .json(&json!({"name": metric, "value": value}))
            .send()
            .await;
        let ok = resp.is_ok_and(|r| r.status().is_success());
        (op, ok)
    };

    WorkResult {
        operation,
        latency: start.elapsed(),
        ok,
    }
}
