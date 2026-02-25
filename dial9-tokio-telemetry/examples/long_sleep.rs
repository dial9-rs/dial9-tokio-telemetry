use dial9_tokio_telemetry::{DetectLongWait, LongPollTracker, SentinelStatus};
use tokio::time::Duration;

#[tokio::main]
async fn main() {
    let (tracker, mut handle) = LongPollTracker::new();
    let _tracker_task = tracker.spawn();

    let sleep_future = tokio::time::sleep(Duration::from_secs(10));
    let wrapped = DetectLongWait::new(sleep_future, handle.sentinel_tx.clone());

    tokio::spawn(async move {
        wrapped.await;
        println!("Sleep completed");
    });

    println!("Waiting for long poll detection (6 seconds)...");

    if let Some(event) = handle.rx.recv().await {
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
            if let Some((time, trace)) = traces.last() {
                println!("Latest trace captured at: {:?}", time);
                println!("\nTrace:\n{}", trace);
            }
        }
    }

    std::process::exit(0);
}
