use dial9_tokio_telemetry::task_dump::{DetectLongWait, LongPollTracker, SentinelStatus};
use tokio::time::Duration;

#[tokio::main]
async fn main() {
    let (tracker, mut handle) = LongPollTracker::new();
    let _tracker_task = tracker.spawn();

    let sleep_future = tokio::time::sleep(Duration::from_secs(20));
    let wrapped = DetectLongWait::new(sleep_future, handle.sentinel_tx.clone());

    let task = tokio::spawn(async move {
        wrapped.await;
        println!("Sleep completed");
    });

    println!("Waiting for long poll detection (7 seconds to ensure bucket rotation)...");
    tokio::time::sleep(Duration::from_secs(7)).await;

    // Check if we got a trace
    if let Ok(event) = handle.rx.try_recv() {
        println!("\n=== LONG POLL DETECTED ===");
        println!(
            "Status: {:?}",
            match &event.status {
                SentinelStatus::Pending(_) => "Pending",
                SentinelStatus::Completed => "Completed",
                SentinelStatus::Cancelled => "Cancelled",
            }
        );
        println!("Send count: {}", event.send_count.0);

        if let SentinelStatus::Pending(traces) = &event.status {
            println!("\nNumber of traces captured: {}", traces.len());
            if let Some((_, trace)) = traces.last() {
                println!(
                    "\nTrace (first 10 lines):\n{}",
                    trace
                        .to_string()
                        .lines()
                        .take(10)
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
    }

    println!("\nNow cancelling task...");
    task.abort();
    println!("Task cancelled - sentinel should be marked as Cancelled");

    std::process::exit(0);
}
