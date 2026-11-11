use anyhow::Result;

#[tokio::main]
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
