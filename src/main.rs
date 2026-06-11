mod ca;
mod capture;
mod cli;
mod config;
mod paths;
mod rewrite;
mod state;

use anyhow::Result;

use cli::{CaCmd, Cmd};
use config::Config;
use paths::Paths;
use state::State;

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
            let cfg_path = paths.config_file();
            config::write_starter_config(&cfg_path, force)?;
            println!("Wrote starter config to {}", cfg_path.display());
            let ca = ca::load_or_generate(&paths.ca_cert(), &paths.ca_key())?;
            println!("CA ready at {}", ca.cert_path.display());
            println!("  fingerprint (SHA-256): {}", ca.fingerprint()?);
            println!("Next: run `rtr trust` once, then `rtr codex`.");
            Ok(())
        }
        Cmd::Run { tool, .. } => anyhow::bail!("run not implemented yet (tool={tool})"),
        Cmd::Switch { first, second } => {
            let cfg = Config::load(&paths.config_file())?;
            let (tool, profile) = cfg.resolve_switch(&first, second.as_deref())?;
            let state_path = paths.state_file();
            let mut st = State::load(&state_path)?;
            st.set_active(&tool, &profile);
            st.save(&state_path)?;
            println!("Switched {tool} -> {profile}");
            Ok(())
        }
        Cmd::Status { tool } => anyhow::bail!("status not implemented yet ({tool:?})"),
        Cmd::Trust { system } => anyhow::bail!("trust not implemented yet (system={system})"),
        Cmd::Untrust { system } => anyhow::bail!("untrust not implemented yet (system={system})"),
        Cmd::Ca { cmd } => {
            let ca = ca::load_or_generate(&paths.ca_cert(), &paths.ca_key())?;
            match cmd {
                CaCmd::Path => println!("{}", ca.cert_path.display()),
                CaCmd::Show => print!("{}", ca.cert_pem),
            }
            Ok(())
        }
    }
}
