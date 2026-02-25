mod validation;

use dial9_tokio_telemetry::telemetry::{
    SimpleBinaryWriter, TraceReader, TracedRuntime, analyze_trace,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const NUM_CLIENTS: usize = 20;

async fn echo_server(listener: TcpListener, running: Arc<AtomicBool>) {
    while running.load(Ordering::Relaxed) {
        let (mut sock, _) = match listener.accept().await {
            Ok(c) => c,
            Err(_) => break,
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            loop {
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if sock.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        });
    }
}

async fn run_client(port: u16, running: Arc<AtomicBool>) -> usize {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let msg = b"hello";
    let mut buf = [0u8; 256];
    let mut count = 0;

    while running.load(Ordering::Relaxed) {
        if stream.write_all(msg).await.is_err() {
            break;
        }
        if stream.read(&mut buf).await.is_err() {
            break;
        }
        count += 1;
    }
    count
}

#[test]
fn overhead_bench_validates() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let num_workers = 4;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let writer = SimpleBinaryWriter::new(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, Box::new(writer))
        .unwrap();

    let running = Arc::new(AtomicBool::new(true));

    let tokio_metrics = runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_running = running.clone();
        tokio::spawn(echo_server(listener, server_running));

        // Spawn clients
        let mut handles = Vec::new();
        for _ in 0..NUM_CLIENTS {
            let r = running.clone();
            handles.push(tokio::spawn(run_client(port, r)));
        }

        // this is enough to get ~5k plls and ~800 parks/unparks
        tokio::time::sleep(Duration::from_millis(100)).await;
        running.store(false, Ordering::Relaxed);

        // Wait for clients
        let mut total_requests = 0;
        for h in handles {
            total_requests += h.await.unwrap();
        }

        let metrics = tokio::runtime::Handle::current().metrics();
        (metrics, total_requests)
    });

    drop(runtime);
    drop(guard);

    // Read trace
    let mut reader = TraceReader::new(trace_path.to_str().unwrap()).unwrap();
    reader.read_header().unwrap();
    let events = reader.read_all().unwrap();
    let analysis = analyze_trace(&events);

    let (metrics, total_requests) = tokio_metrics;

    eprintln!("Total requests processed: {}", total_requests);
    eprintln!("Total tasks spawned: {}", metrics.spawned_tasks_count());

    validation::validate_trace_matches_metrics(&analysis, &events, &metrics);
}
