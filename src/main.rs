mod cli;
mod paths;

use anyhow::Result;

use cli::{CaCmd, Cmd};
use paths::Paths;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RTR_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let parsed = cli::parse_from(raw);
    let paths = Paths::from_env()?;

    match parsed.cmd {
        Cmd::Init { force } => {
            anyhow::bail!("init not implemented yet (force={force}, {:?})", paths.config_file())
        }
        Cmd::Run { tool, .. } => anyhow::bail!("run not implemented yet (tool={tool})"),
        Cmd::Switch { first, second } => {
            anyhow::bail!("switch not implemented yet ({first} {second:?})")
        }
        Cmd::Status { tool } => anyhow::bail!("status not implemented yet ({tool:?})"),
        Cmd::Trust { system } => anyhow::bail!("trust not implemented yet (system={system})"),
        Cmd::Untrust { system } => anyhow::bail!("untrust not implemented yet (system={system})"),
        Cmd::Ca { cmd } => match cmd {
            CaCmd::Path => anyhow::bail!("ca path not implemented yet"),
            CaCmd::Show => anyhow::bail!("ca show not implemented yet"),
        },
    }
}
