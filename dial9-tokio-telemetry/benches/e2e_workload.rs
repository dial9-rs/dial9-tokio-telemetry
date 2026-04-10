//! Runs a fixed-size mixed CPU/IO workload — modelled on the
// realistic_workload example;

mod bmf;

#[cfg(target_os = "linux")]
use dial9_tokio_telemetry::telemetry::CpuProfilingConfig;
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const NUM_CLIENTS: usize = 4;
const REQUESTS_PER_CLIENT: usize = 1_000;
const NUM_CPU_TASKS: usize = 3;
const CPU_TASK_ITERATIONS: usize = 20;
const CPU_ITERS_PER_REQUEST: u64 = 10_000;
const CPU_ITERS_PER_BURST: u64 = 50_000;
const TOTAL_REQUESTS: usize = NUM_CLIENTS * REQUESTS_PER_CLIENT;

fn cpu_work(iterations: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..iterations {
        result = result.wrapping_add(i.wrapping_mul(i));
    }
    result
}

async fn workload_server(listener: TcpListener) {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let Ok(n) = sock.read(&mut buf).await else {
                return;
            };
            if n == 0 {
                return;
            }
            let checksum = cpu_work(CPU_ITERS_PER_REQUEST);
            let _ = sock.write_all(&checksum.to_le_bytes()).await;
        });
    }
}

async fn workload_client(port: u16) {
    for _ in 0..REQUESTS_PER_CLIENT {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream.write_all(b"request").await.expect("write");
        let mut buf = [0u8; 8];
        stream.read_exact(&mut buf).await.expect("read");
    }
}

async fn cpu_task() {
    for _ in 0..CPU_TASK_ITERATIONS {
        cpu_work(CPU_ITERS_PER_BURST);
        tokio::task::yield_now().await;
    }
}

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = RotatingWriter::single_file("/tmp/e2e_workload_trace.bin").unwrap();
    let tb = TracedRuntime::builder().with_task_tracking(true);
    #[cfg(target_os = "linux")]
    let tb = tb.with_cpu_profiling(CpuProfilingConfig::default());
    let (runtime, _guard) = tb.build_and_start(builder, writer).unwrap();

    let start = Instant::now();
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(workload_server(listener));

        let clients: Vec<_> = (0..NUM_CLIENTS)
            .map(|_| tokio::spawn(workload_client(port)))
            .collect();
        let cpu_tasks: Vec<_> = (0..NUM_CPU_TASKS)
            .map(|_| tokio::spawn(cpu_task()))
            .collect();

        for c in clients {
            c.await.expect("client");
        }
        for t in cpu_tasks {
            t.await.expect("cpu task");
        }
        server.abort();
    });
    let wall = start.elapsed();

    drop(_guard);

    let rps = TOTAL_REQUESTS as f64 / wall.as_secs_f64();
    let mut report = bmf::Report::new();
    report.insert(
        "e2e::wall_time_ns".to_string(),
        bmf::Metric::latency(wall.as_nanos() as f64),
    );
    report.insert(
        "e2e::throughput_rps".to_string(),
        bmf::Metric::throughput(rps),
    );
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
