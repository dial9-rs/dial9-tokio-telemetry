use dial9_tokio_telemetry::telemetry::{SimpleBinaryWriter, TracedRuntime};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn cpu_work(iterations: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..iterations {
        result = result.wrapping_add(i.wrapping_mul(i));
    }
    result
}

async fn echo_server(listener: TcpListener) {
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) => return,
                    Ok(n) => {
                        cpu_work(5000).await;
                        let _ = socket.write_all(&buf[..n]).await;
                    }
                    Err(_) => return,
                }
            }
        });
    }
}

async fn chatty_client(port: u16, id: usize) {
    tokio::time::sleep(Duration::from_millis(200)).await;
    loop {
        if let Ok(mut stream) = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await
        {
            for i in 0u64.. {
                let msg = format!("client {} msg {}", id, i);
                if stream.write_all(msg.as_bytes()).await.is_err() {
                    break;
                }
                let mut buf = [0u8; 1024];
                if stream.read(&mut buf).await.is_err() {
                    break;
                }
                let delay = match id % 3 {
                    0 => 10,
                    1 => 50,
                    _ => 200,
                };
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn background_cpu_bursts() {
    loop {
        for _ in 0..20 {
            tokio::spawn(async { cpu_work(100_000).await });
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn periodic_yielder() {
    loop {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let duration_secs = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30u64);

    let writer = Box::new(SimpleBinaryWriter::new("long_trace.bin").unwrap());
    let (runtime, _guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    println!("Running workload for {}s...", duration_secs);

    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(echo_server(listener));
        for i in 0..8 {
            tokio::spawn(chatty_client(port, i));
        }
        tokio::spawn(background_cpu_bursts());
        tokio::spawn(periodic_yielder());

        tokio::time::sleep(Duration::from_secs(duration_secs)).await;
        println!("Done.");
    });

    drop(_guard);

    let size = std::fs::metadata("long_trace.bin")
        .map(|m| m.len())
        .unwrap_or(0);
    let events = size.saturating_sub(12) / 33;
    println!(
        "Trace written to long_trace.bin ({} bytes, {} events)",
        size, events
    );
}
