use std::time::Duration;

use clap::Parser;
use reqwest::Client;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(about = "Load-test client for the metrics service")]
struct Args {
    #[arg(long, help = "Base URL of the metrics server")]
    server_url: String,

    #[arg(
        long,
        default_value = "55",
        help = "How long to run the load profile (seconds)"
    )]
    run_duration: u64,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let shutdown = CancellationToken::new();

    // Internal timer: cancel the load loop after run_duration seconds.
    let timer_shutdown = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(args.run_duration)).await;
        timer_shutdown.cancel();
    });

    println!(
        "Client starting load profile against {}...",
        args.server_url
    );
    metrics_service::client::run(&args.server_url, shutdown).await;

    // Load profile finished – tell the server to shut down gracefully.
    println!("Client load complete, sending /terminate to server...");
    let http = Client::new();
    match http
        .post(format!("{}/terminate", args.server_url))
        .send()
        .await
    {
        Ok(_) => println!("Server acknowledged termination."),
        Err(e) => eprintln!("Warning: could not reach /terminate: {e}"),
    }
}
