//! Command-line surface for rtr's native Claude and Codex profile launcher.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rtr",
    version,
    about = "Native profile launcher for Claude Code and Codex"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Scaffold a starter config.toml.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Launch Claude Code with a selected subscription profile.
    Claude(SubscriptionRunArgs),
    /// Launch Codex with a selected subscription profile.
    Codex(SubscriptionRunArgs),
    /// Create a Claude/Codex profile and launch the tool to sign in.
    Add {
        tool: String,
        #[arg(long)]
        profile: String,
    },
    /// List configured Claude/Codex profiles.
    Ls,
    /// Show one profile as `<tool>/<profile>`.
    Show { target: String },
    /// Show usage distribution and failure rates.
    Stats {
        #[arg(long)]
        today: bool,
    },
    /// Show configured tools and profiles.
    Status { tool: Option<String> },
}

#[derive(Args, Debug, Clone)]
pub struct SubscriptionRunArgs {
    #[arg(short = 'p', long)]
    pub profile: Option<String>,
    /// Arguments passed through to the selected tool.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Parse raw arguments without the program name.
pub fn parse_from<I, S>(raw: I) -> Cli
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    Cli::parse_from(std::iter::once("rtr".to_string()).chain(raw.into_iter().map(Into::into)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_subscription_runtime_commands() {
        match parse_from([
            "claude",
            "--profile",
            "work",
            "--effort",
            "xhigh",
            "--model",
            "claude-fable-5",
            "--debug",
        ])
        .cmd
        {
            Cmd::Claude(args) => {
                assert_eq!(args.profile.as_deref(), Some("work"));
                assert_eq!(
                    args.args,
                    v(&["--effort", "xhigh", "--model", "claude-fable-5", "--debug"])
                );
            }
            other => panic!("expected Claude, got {other:?}"),
        }

        match parse_from([
            "codex",
            "--dangerously-bypass-approvals-and-sandbox",
            "-m",
            "gpt-5.5",
            "-c",
            "model_reasoning_effort=xhigh",
        ])
        .cmd
        {
            Cmd::Codex(args) => {
                assert_eq!(args.profile.as_deref(), None);
                assert_eq!(
                    args.args,
                    v(&[
                        "--dangerously-bypass-approvals-and-sandbox",
                        "-m",
                        "gpt-5.5",
                        "-c",
                        "model_reasoning_effort=xhigh"
                    ])
                );
            }
            other => panic!("expected Codex, got {other:?}"),
        }

        match parse_from(["codex", "--", "--profile", "native"]).cmd {
            Cmd::Codex(args) => {
                assert_eq!(args.profile.as_deref(), None);
                assert_eq!(args.args, v(&["--profile", "native"]));
            }
            other => panic!("expected Codex, got {other:?}"),
        }
    }

    #[test]
    fn parse_profile_management_commands() {
        assert!(matches!(parse_from(["ls"]).cmd, Cmd::Ls));
        assert!(matches!(
            parse_from(["stats", "--today"]).cmd,
            Cmd::Stats { today: true }
        ));
        assert!(matches!(
            parse_from(["show", "claude/work"]).cmd,
            Cmd::Show { target } if target == "claude/work"
        ));
        assert!(matches!(
            parse_from(["status", "codex"]).cmd,
            Cmd::Status { tool } if tool.as_deref() == Some("codex")
        ));
        assert!(matches!(
            parse_from(["add", "codex", "--profile", "personal"]).cmd,
            Cmd::Add { tool, profile } if tool == "codex" && profile == "personal"
        ));
    }

    #[test]
    fn removed_proxy_commands_are_rejected() {
        for args in [
            vec!["run", "codex"],
            vec!["capture", "codex", "--profile", "work"],
            vec!["import", "codex", "--profile", "work"],
            vec!["trust"],
            vec!["untrust"],
            vec!["ca", "path"],
            vec!["switch", "codex", "work"],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("rtr").chain(args.iter().copied())).is_err(),
                "removed command parsed: {args:?}"
            );
        }
    }
}
