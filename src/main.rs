use anyhow::Result;

// The launcher awaits one native child and signals; it has no parallel async
// tasks. Avoid a CPU-sized thread pool for every short watchdog lookup.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    match rtr::run().await {
        Ok(()) => Ok(()),
        Err(error) => {
            let code = if error
                .downcast_ref::<rtr::quota::NoUsableProfiles>()
                .is_some()
            {
                75
            } else {
                1
            };
            eprintln!("rtr: {}", format!("{error:#}").replace(['\n', '\r'], " "));
            std::process::exit(code)
        }
    }
}
