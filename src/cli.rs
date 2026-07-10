//! Command-line surface for rtr's native Claude and Codex profile launcher.

use clap::{Args, Parser, Subcommand};

const TOP_LEVEL_LONG_ABOUT: &str = "\
Run Claude Code or Codex through named native homes. Each profile keeps its own
auth, settings, sessions, and skills. rtr launches the real CLI directly.

Shortest path:
  rtr init
  rtr add claude --profile work
  rtr add codex --profile personal
  rtr claude --profile work [claude args...]
  rtr codex --profile personal [codex args...]

Omit --profile to rotate through enabled profiles:
  rtr codex
  rtr codex

Put -- before child args that should not be parsed by rtr:
  rtr codex -- --profile native-codex-profile";

#[derive(Parser, Debug)]
#[command(
    name = "rtr",
    version,
    about = "Native profile launcher for Claude Code and Codex",
    long_about = TOP_LEVEL_LONG_ABOUT
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Scaffold a starter config.toml.
    Init {
        /// Replace an existing config.toml.
        #[arg(long)]
        force: bool,
    },
    /// Launch Claude Code with a selected subscription profile.
    #[command(long_about = "\
Launch Claude Code in one configured profile.

With --profile, rtr uses that profile and leaves rotation unchanged. Without
--profile, rtr uses the next enabled Claude profile. Remaining arguments are
passed to Claude Code.")]
    Claude(ToolRunArgs),
    /// Launch Codex with a selected subscription profile.
    #[command(long_about = "\
Launch Codex in one configured profile.

With --profile, rtr uses that profile and leaves rotation unchanged. Without
--profile, rtr uses the next enabled Codex profile. Remaining arguments are
passed to Codex.")]
    Codex(ToolRunArgs),
    /// Create a Claude/Codex profile and launch the tool to sign in.
    Add {
        /// Tool to add: claude or codex.
        tool: String,
        /// Profile name to create.
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
pub struct ToolRunArgs {
    /// Configured rtr profile to use instead of automatic rotation.
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
    use clap::CommandFactory;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn help_for(args: &[&str]) -> String {
        let mut cmd = Cli::command();
        if let Some((name, _subcommand)) = args.split_first() {
            let subcommand = cmd
                .find_subcommand_mut(name)
                .unwrap_or_else(|| panic!("missing subcommand {name}"));
            return subcommand.render_long_help().to_string();
        }
        cmd.render_long_help().to_string()
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
    fn top_level_help_teaches_setup_and_run_flow() {
        let help = help_for(&[]);
        for expected in [
            "Shortest path:",
            "rtr init",
            "rtr add claude --profile work",
            "rtr add codex --profile personal",
            "rtr claude --profile work [claude args...]",
            "rtr codex --profile personal [codex args...]",
            "Omit --profile to rotate through enabled profiles:",
            "rtr codex -- --profile native-codex-profile",
        ] {
            assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
        }
    }

    #[test]
    fn run_and_add_help_describe_profile_and_passthrough() {
        let claude = help_for(&["claude"]);
        assert!(
            claude.contains("Configured rtr profile to use instead of automatic rotation"),
            "{claude}"
        );
        assert!(
            claude.contains("Arguments passed through to the selected tool"),
            "{claude}"
        );
        assert!(claude.contains("leaves rotation unchanged"), "{claude}");

        let add = help_for(&["add"]);
        assert!(add.contains("Tool to add: claude or codex"), "{add}");
        assert!(add.contains("Profile name to create"), "{add}");
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
