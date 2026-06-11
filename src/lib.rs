//! rtr — per-binary MITM proxy that captures and rewrites outbound auth headers.

pub mod ca;
pub mod capture;
pub mod cli;
pub mod config;
pub mod keychain;
pub mod paths;
pub mod proxy;
pub mod rewrite;
pub mod runner;
pub mod state;

use std::path::PathBuf;

use anyhow::{Context, Result};

use cli::{CaCmd, Cmd};
use config::Config;
use paths::Paths;
use state::State;

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// Parse argv and dispatch the chosen subcommand.
pub async fn run() -> Result<()> {
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
        Cmd::Run {
            tool,
            show_secrets,
            log,
            args,
        } => {
            let code = runner::run_tool(&paths, &tool, &args, show_secrets, log).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
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
        Cmd::Status { tool } => runner::print_status(&paths, tool.as_deref()),
        Cmd::Trust { system } => {
            let ca = ca::load_or_generate(&paths.ca_cert(), &paths.ca_key())?;
            let domain = trust_domain(system);
            let login_kc = keychain::login_keychain(&home_dir()?);
            keychain::install(domain, &login_kc, &ca.cert_path)?;
            println!("Trusted rtr CA in {} keychain.", domain.label());
            Ok(())
        }
        Cmd::Untrust { system } => {
            let ca = ca::load_or_generate(&paths.ca_cert(), &paths.ca_key())?;
            let domain = trust_domain(system);
            keychain::remove(domain, &ca.cert_path)?;
            println!("Removed rtr CA trust from {} keychain.", domain.label());
            Ok(())
        }
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

fn trust_domain(system: bool) -> keychain::Domain {
    if system {
        keychain::Domain::System
    } else {
        keychain::Domain::Login
    }
}
