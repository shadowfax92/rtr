//! rtr — native profile launcher for Claude Code and Codex.

pub mod cli;
pub mod config;
pub mod config_command;
mod file_lock;
pub mod paths;
pub mod profiles;
pub mod runner;
pub mod selection;
pub mod state;
pub mod tool_specs;
pub mod usage;

use std::path::PathBuf;

use anyhow::{Context, Result};

use cli::{Cmd, ConfigCommand};
use paths::Paths;

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// Parse argv and dispatch the chosen command.
pub async fn run() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let parsed = cli::parse_from(raw);
    let paths = Paths::from_env()?;

    match parsed.cmd {
        Cmd::Init { force } => {
            let cfg_path = paths.config_file();
            config::write_starter_config(&cfg_path, force)?;
            println!("Wrote starter config to {}", cfg_path.display());
            Ok(())
        }
        Cmd::Claude(args) => {
            let code = runner::run_subscription_tool(
                &paths,
                "claude",
                args.profile.as_deref(),
                &args.args,
            )
            .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Codex(args) => {
            let code =
                runner::run_subscription_tool(&paths, "codex", args.profile.as_deref(), &args.args)
                    .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Add { tool, profile } => {
            let code = runner::add_subscription_profile(&paths, &tool, &profile).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Rm { tool, profile, yes } => {
            profiles::run_remove_profile(&paths, &tool, &profile, yes)
        }
        Cmd::Config { command } => match command {
            None => {
                let stdout = std::io::stdout();
                config_command::write_config_path(&paths, &mut stdout.lock())?;
                Ok(())
            }
            Some(ConfigCommand::Edit) => {
                let code = config_command::edit_config(&paths)?;
                if code != 0 {
                    std::process::exit(code);
                }
                Ok(())
            }
        },
        Cmd::Fix { tool, profile } => {
            let code = runner::fix_subscription_profile(&paths, &tool, &profile).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Enable { target } => profiles::run_set_profile_enabled(&paths, &target, true),
        Cmd::Disable { target } => profiles::run_set_profile_enabled(&paths, &target, false),
        Cmd::Bypass { tool, profile } => {
            profiles::run_set_profile_bypass(&paths, &tool, &profile, true)
        }
        Cmd::Unbypass { tool, profile } => {
            profiles::run_set_profile_bypass(&paths, &tool, &profile, false)
        }
        Cmd::Ls => profiles::run_list_profiles(&paths),
        Cmd::Show { target } => profiles::run_show_profile(&paths, &target),
        Cmd::Stats { today } => usage::print_stats(&paths, today),
        Cmd::Status { tool } => profiles::print_status(&paths, tool.as_deref()),
    }
}
