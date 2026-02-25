use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

const METRICS: &[&str] = &["cpu", "memory", "latency", "error_rate", "queue_depth"];
const MAX_WORKERS: usize = 40;
const THUNDERING_HERD: usize = 200;
const BASELINE: usize = 4;

/// Load profile (elapsed_secs, target_concurrency):
/// 0-10:  ramp 4 -> 40
/// 10-20: hold at 40
/// 20-30: ramp 40 -> 4
/// 30-40: hold at 4 (baseline)
/// 40-45: thundering herd (200)
/// 45+:   back to baseline (4)
fn target_concurrency(elapsed: f64) -> usize {
    if elapsed < 10.0 {
        let t = elapsed / 10.0;
        (BASELINE as f64 + t * (MAX_WORKERS - BASELINE) as f64) as usize
    } else if elapsed < 20.0 {
        MAX_WORKERS
    } else if elapsed < 30.0 {
        let t = (elapsed - 20.0) / 10.0;
        (MAX_WORKERS as f64 - t * (MAX_WORKERS - BASELINE) as f64) as usize
    } else if elapsed < 40.0 {
        BASELINE
    } else if elapsed < 45.0 {
        THUNDERING_HERD
    } else {
        BASELINE
    }
}

pub async fn run(base_url: &str, shutdown: CancellationToken) {
    let client = Arc::new(Client::new());
    // semaphore controls how many workers run concurrently
    let sem = Arc::new(Semaphore::new(0));
    let start = Instant::now();

    // spawn a large pool of workers that each wait for a permit
    for i in 0..THUNDERING_HERD {
        let client = client.clone();
        let sem = sem.clone();
        let base_url = base_url.to_string();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut tick: u64 = i as u64;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    permit = sem.acquire() => {
                        let _permit = permit.unwrap();
                        do_work(&client, &base_url, i, tick).await;
                        tick += 1;
                    }
                }
            }
        });
    }

    // coordinator: adjusts semaphore permits to match target concurrency
    let mut current = 0usize;
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let target = target_concurrency(start.elapsed().as_secs_f64());
        match target.cmp(&current) {
            std::cmp::Ordering::Greater => {
                sem.add_permits(target - current);
                println!("concurrency -> {target}");
            }
            std::cmp::Ordering::Less => {
                // acquire and forget permits to reduce concurrency
                let to_remove = current - target;
                let sem2 = sem.clone();
                tokio::spawn(async move {
                    for _ in 0..to_remove {
                        sem2.acquire().await.unwrap().forget();
                    }
                });
                println!("concurrency -> {target}");
            }
            std::cmp::Ordering::Equal => {}
        }
        current = target;
        sleep(Duration::from_millis(500)).await;
    }
}

async fn do_work(client: &Client, base_url: &str, worker: usize, tick: u64) {
    let metric = METRICS[tick as usize % METRICS.len()];
    let value = (tick as f64 * 1.3 + worker as f64 * 7.7).sin().abs() * 100.0;

    let _ = client
        .post(format!("{base_url}/metrics"))
        .json(&json!({"name": metric, "value": value}))
        .send()
        .await;

    if tick.is_multiple_of(10) {
        let _ = client
            .get(format!("{base_url}/metrics/{metric}"))
            .send()
            .await;
    }
}
