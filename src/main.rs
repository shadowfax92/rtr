use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    rtr::run().await
}
