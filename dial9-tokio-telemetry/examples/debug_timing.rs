use dial9_tokio_telemetry::task_dump::{DetectLongWait, LongPollTracker, SentinelStatus};
use tokio::time::Duration;

#[tokio::main]
async fn main() {
    let (tracker, mut handle) = LongPollTracker::new();
    tracker.spawn();

    println!("Starting sleep future...");
    let sleep_future = tokio::time::sleep(Duration::from_secs(20));
    let wrapped = DetectLongWait::new(sleep_future, handle.sentinel_tx.clone());

    let task = tokio::spawn(async move {
        wrapped.await;
        println!("Sleep completed");
    });

    println!("Waiting for long poll detection...");

    for i in 1..=10 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!("{}s elapsed...", i);

        if let Ok(event) = handle.rx.try_recv() {
            println!("\n=== LONG POLL DETECTED at {}s ===", i);
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
                        "\nTrace (first 8 lines):\n{}",
                        trace
                            .to_string()
                            .lines()
                            .take(8)
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                }
            }
            break;
        }
    }

    task.abort();
}
