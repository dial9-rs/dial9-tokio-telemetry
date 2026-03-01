use dial9_tokio_telemetry::task_dump::{DetectLongWait, LongPollTracker, SentinelStatus};
use tokio::time::Duration;

#[tokio::main]
async fn main() {
    let (tracker, mut handle) = LongPollTracker::new();
    let _tracker_task = tracker.spawn();

    println!("Starting short sleep (2s) that completes before threshold...");
    let sleep_future = tokio::time::sleep(Duration::from_secs(2));
    let wrapped = DetectLongWait::new(sleep_future, handle.sentinel_tx.clone());

    wrapped.await;
    println!("Sleep completed successfully");

    println!("Waiting 5 more seconds to see if any trace is sent...");
    tokio::time::sleep(Duration::from_secs(5)).await;

    if let Ok(event) = handle.rx.try_recv() {
        println!("\n=== UNEXPECTED: TRACE RECEIVED ===");
        println!(
            "Status: {:?}",
            match &event.status {
                SentinelStatus::Pending(_) => "Pending",
                SentinelStatus::Completed => "Completed",
                SentinelStatus::Cancelled => "Cancelled",
            }
        );
        println!("Send count: {}", event.send_count.0);
    } else {
        println!("\n✓ SUCCESS: No trace sent for task that completed before threshold");
    }

    std::process::exit(0);
}
