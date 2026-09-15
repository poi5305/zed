use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    zed_web_server::run().await
}
