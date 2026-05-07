#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dial9_viewer::cli::run().await
}
