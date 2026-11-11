//! rtr — native profile launcher for Claude Code and Codex.

pub mod cli;
mod codex_history;
pub mod config;
pub mod config_command;
pub mod conversation_command;
mod conversation_transfer;
pub mod conversations;
mod file_lock;
mod launch_settings;
mod native_usage;
pub mod output;
pub mod paths;
mod picker;
mod profile_overview;
pub mod profile_paths;
pub mod profiles;
pub mod quota;
pub mod runner;
pub mod selection;
mod session_index;
pub mod session_status;
pub mod state;
pub mod tool_specs;
pub mod usage;
mod weights;

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
            let code = runner::add_subscription_profile(&paths, &tool, profile.name()?).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Rm { tool, profile, yes } => {
            profiles::run_remove_profile(&paths, &tool, profile.name()?, yes)
        }
        Cmd::Config { command, output } => match command {
            None => {
                let stdout = std::io::stdout();
                config_command::write_config_path(
                    &paths,
                    &mut stdout.lock(),
                    output.color.stdout(),
                )?;
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
            let code = runner::fix_subscription_profile(&paths, &tool, profile.name()?).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Enable { tool, profile } => {
            profiles::run_set_profile_enabled(&paths, &tool, profile.name()?, true)
        }
        Cmd::Disable { tool, profile } => {
            profiles::run_set_profile_enabled(&paths, &tool, profile.name()?, false)
        }
        Cmd::Bypass { tool, profile } => {
            profiles::run_set_profile_bypass(&paths, &tool, profile.name()?, true)
        }
        Cmd::Unbypass { tool, profile } => {
            profiles::run_set_profile_bypass(&paths, &tool, profile.name()?, false)
        }
        Cmd::Paths { json, output } => profile_paths::run(&paths, json, output.color.stdout()),
        Cmd::Sessions(args) => {
            let code = conversation_command::run_sessions(&paths, args).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Session(args) => {
            let status = session_status::lookup(
                &paths,
                &args.id,
                args.tool.as_deref(),
                args.profile.as_deref(),
            )?;
            quota::learn(&paths, std::slice::from_ref(&status))?;
            if args.json {
                println!("{}", serde_json::to_string(&status)?);
            } else {
                println!(
                    "{}/{} {} model={} effort={} limit={}",
                    status.tool,
                    status.profile,
                    status.id,
                    status.model.as_deref().unwrap_or("unknown"),
                    status.effort.as_deref().unwrap_or("unknown"),
                    status
                        .limit
                        .as_ref()
                        .map(|l| l.kind.as_str())
                        .unwrap_or("none")
                );
            }
            Ok(())
        }
        Cmd::ClearLimit { tool, profile } => quota::clear(&paths, &tool, profile.name()?),
        Cmd::Fork(args) => {
            let code = conversation_command::run_fork(&paths, args).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Resume(args) => {
            let code =
                conversation_command::run_open(&paths, args, conversations::OpenMode::Resume, None)
                    .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::ConversationPreview { key } => conversation_command::print_preview(&paths, &key),
        Cmd::Ls { all, output } => profile_overview::run(&paths, all, output.color.stdout()),
        Cmd::Show { tool, profile } => profiles::run_show_profile(&paths, &tool, profile.name()?),
        Cmd::Status { tool } => profiles::print_status(&paths, tool.as_deref()),
        Cmd::Weight(args) => weights::run(&paths, &args),
    }
}
