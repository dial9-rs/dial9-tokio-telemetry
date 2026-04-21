use std::time::Duration;

use dial9_tokio_telemetry::config::Dial9Config;
use dial9_tokio_telemetry::telemetry::TelemetryHandle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn my_config() -> Dial9Config {
    Dial9Config::builder()
        .base_path("realistic_trace.bin")
        .max_file_size(64 * 1024 * 1024)
        .max_total_size(256 * 1024 * 1024)
        .with_tokio(|t| {
            t.worker_threads(4);
        })
        .with_runtime(|r| r.with_task_tracking(true))
        .build()
        .expect("config build failed")
}

async fn cpu_bound_work(n: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..n {
        result = result.wrapping_add(i.wrapping_mul(i));
    }
    result
}

async fn network_server(listener: TcpListener) {
    let handle = TelemetryHandle::current();
    loop {
        if let Ok((mut socket, _)) = listener.accept().await {
            handle.spawn(async move {
                let mut buf = [0u8; 1024];
                if let Ok(n) = socket.read(&mut buf).await {
                    let result = cpu_bound_work(10000).await;
                    let response = format!("Processed {} bytes, result: {}\n", n, result);
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });
        }
    }
}

async fn network_client(port: u16, id: usize) {
    tokio::time::sleep(Duration::from_millis(100)).await;

    for i in 0..5000 {
        if let Ok(mut stream) = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await
        {
            let msg = format!("Client {} request {}", id, i);
            let _ = stream.write_all(msg.as_bytes()).await;

            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

async fn mixed_workload(port: u16) {
    let handle = TelemetryHandle::current();

    let clients: Vec<_> = (0..5)
        .map(|i| handle.spawn(network_client(port, i)))
        .collect();

    let cpu_tasks: Vec<_> = (0..3)
        .map(|_| {
            handle.spawn(async {
                for _ in 0..10 {
                    cpu_bound_work(50000).await;
                    tokio::task::yield_now().await;
                }
            })
        })
        .collect();

    for client in clients {
        let _ = client.await;
    }
    for task in cpu_tasks {
        let _ = task.await;
    }
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    println!("Running realistic workload...");

    let handle = TelemetryHandle::current();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    handle.spawn(network_server(listener));

    tokio::time::timeout(Duration::from_secs(5), mixed_workload(port))
        .await
        .ok();

    println!("Trace written to realistic_trace.*.bin");
}
