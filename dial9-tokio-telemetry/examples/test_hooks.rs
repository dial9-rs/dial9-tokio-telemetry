use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn main() {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).on_thread_start(move || {
        c.fetch_add(1, Ordering::SeqCst);
        eprintln!("Thread started!");
    });

    let runtime = builder.build().unwrap();

    runtime.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });

    println!("Thread start count: {}", counter.load(Ordering::SeqCst));
}
