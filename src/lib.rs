//! rtr — per-binary profile launcher for Claude Code and Codex.

pub mod ca;
pub mod cli;
pub mod config;
mod file_lock;
pub mod import;
pub mod keychain;
pub mod paths;
pub mod proxy;
pub mod rewrite;
pub mod runner;
pub mod selection;
pub mod state;
pub mod tool_specs;
pub mod usage;

use std::path::PathBuf;

use anyhow::{Context, Result};

use cli::{CaCmd, Cmd};
use config::Config;
use import::ConflictPolicy;
use paths::Paths;
use state::State;

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn tracing_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_env("RTR_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

pub fn init_stderr_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_filter())
        .with_writer(std::io::stderr)
        .without_time()
        .try_init();
}

/// Route tracing (including hudsucker's own spans/errors) to a file so the
/// child process keeps a clean terminal. Best-effort: if the file can't be
/// opened we simply drop proxy logs.
pub fn init_file_tracing(path: &std::path::Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("rtr: could not open {} for logs: {e}", path.display());
            return;
        }
    };
    // `Arc<File>` writes through one shared fd: no per-event dup(2), and no
    // panic path in the logging hot loop.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_filter())
        .with_ansi(false)
        .without_time()
        .with_writer(std::sync::Arc::new(file))
        .try_init();
}

/// Parse argv and dispatch the chosen subcommand.
pub async fn run() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let parsed = cli::parse_from(raw);
    let paths = Paths::from_env()?;

    if !matches!(parsed.cmd, Cmd::Run { .. } | Cmd::Claude(_) | Cmd::Codex(_)) {
        init_stderr_tracing();
    }

    match parsed.cmd {
        Cmd::Init { force } => {
            let cfg_path = paths.config_file();
            config::write_starter_config(&cfg_path, force)?;
            println!("Wrote starter config to {}", cfg_path.display());
            let ca = ca::load_or_generate(&paths.ca_cert(), &paths.ca_key())?;
            println!("CA ready at {}", ca.cert_path.display());
            println!("  fingerprint (SHA-256): {}", ca.fingerprint()?);
            println!("Next: run `rtr trust`, then configure profiles in config.toml.");
            Ok(())
        }
        Cmd::Run { tool, log, args } => {
            let code = runner::run_tool(&paths, &tool, &args, log).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Claude(args) => {
            let code = runner::run_subscription_tool(
                &paths,
                "claude",
                args.profile.as_deref(),
                &args.args,
                args.log,
            )
            .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Codex(args) => {
            let code = runner::run_subscription_tool(
                &paths,
                "codex",
                args.profile.as_deref(),
                &args.args,
                args.log,
            )
            .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Import {
            tool,
            profile,
            from_capture,
            force,
            no_overwrite,
            show_secrets,
        } => {
            let policy = if force {
                ConflictPolicy::Force
            } else if no_overwrite {
                ConflictPolicy::Reject
            } else {
                ConflictPolicy::Prompt
            };
            import::run_import_profile(&paths, &tool, &profile, &from_capture, policy, show_secrets)
        }
        Cmd::Ls => import::run_list_profiles(&paths),
        Cmd::Show {
            target,
            show_secrets,
        } => import::run_show_profile(&paths, &target, show_secrets),
        Cmd::Stats { today } => usage::print_stats(&paths, today),
        Cmd::Switch { first, second } => {
            let cfg = Config::load(&paths.config_file())?;
            let (tool, profile) = cfg.resolve_switch(&first, second.as_deref())?;
            let state_path = paths.state_file();
            State::update_locked(&state_path, |st| {
                st.set_active(&tool, &profile);
                Ok(())
            })?;
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
