use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // Tracing is initialised per-command inside `rtr::run` — the `run` command
    // routes proxy logs to a file to keep the child's terminal clean.
    rtr::run().await
}
