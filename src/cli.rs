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
  rtr codex -- --profile native-codex-profile

Pause a profile and bring it back later:
  rtr disable codex --profile personal
  rtr enable codex --profile personal

Bypass a broken profile home and restore isolation:
  rtr bypass codex --profile personal
  rtr unbypass codex --profile personal

Maintain profiles and config:
  rtr fix codex --profile personal
  rtr rm codex --profile personal
  rtr config edit

Discover isolated homes for integrations:
  rtr paths --json

Resume work from the current directory:
  rtr here";

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
    /// Delete a Claude/Codex profile and its native home.
    #[command(long_about = "\
Delete one configured profile and its native home.

This permanently deletes auth and sessions stored in that profile. rtr prints
the exact home path and asks for confirmation unless --yes is supplied.")]
    Rm {
        /// Tool to remove from: claude or codex.
        tool: String,
        /// Profile name to delete.
        #[arg(long)]
        profile: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Print or edit the resolved config.toml path.
    #[command(long_about = "\
Print the resolved config.toml path.

The default output is only the path for script-friendly use. `rtr config edit`
opens an existing config with $VISUAL, falling back to $EDITOR.")]
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Repair an existing profile's authentication in place.
    #[command(long_about = "\
Repair one existing profile and re-authenticate in place.

rtr removes recognized stale credential locks from only that profile home, then
launches the configured tool there. This leaves rotation unchanged.")]
    Fix {
        /// Tool to repair: claude or codex.
        tool: String,
        /// Existing profile name to repair.
        #[arg(long)]
        profile: String,
    },
    /// Re-enable a profile for selection and rotation.
    #[command(long_about = "\
Re-enable one profile.

The profile rejoins automatic rotation and can be forced with --profile again.
Its native home, sign-in, and skills were kept while disabled, so no new
sign-in is needed. Already enabled is a success.")]
    Enable {
        /// Tool to enable: claude or codex.
        tool: String,
        /// Profile name to enable.
        #[arg(long)]
        profile: String,
    },
    /// Disable a profile without deleting its native home.
    #[command(long_about = "\
Disable one profile.

Only the enabled flag in config.toml changes; the profile's native home,
sign-in, and skills stay in place. Disabled profiles are skipped by rotation
and rejected by --profile until re-enabled. Already disabled is a success.")]
    Disable {
        /// Tool to disable: claude or codex.
        tool: String,
        /// Profile name to disable.
        #[arg(long)]
        profile: String,
    },
    /// Run a profile with the tool's default home.
    #[command(long_about = "\
Bypass one profile's isolated native home as <tool> --profile <name>.

Runs keep selecting the profile normally. They launch with the
default Claude or Codex home and no rtr-managed native-home environment
override. Already bypassed is a success. The setting persists until
`rtr unbypass`.")]
    Bypass {
        /// Tool to bypass: claude or codex.
        tool: String,
        /// Profile name to bypass.
        #[arg(long)]
        profile: String,
    },
    /// Restore a profile's isolated native home.
    #[command(long_about = "\
Stop bypassing one profile as <tool> --profile <name>.

Future runs use the profile's isolated native home again. Selection and
rotation are unchanged. Already unbypassed is a success.")]
    Unbypass {
        /// Tool to restore: claude or codex.
        tool: String,
        /// Profile name to restore.
        #[arg(long)]
        profile: String,
    },
    /// List resolved isolated native homes for configured profiles.
    #[command(long_about = "\
List every configured profile's resolved isolated native home.

Disabled, bypassed, and missing homes are included. Use --json for the
machine-readable v1 contract; human output is for inspection only.")]
    Paths {
        /// Emit the versioned machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// List recent resumable sessions for the current directory.
    #[command(long_about = "\
List the five most recently updated Claude Code and Codex sessions whose native
session history records this exact current directory. Results include the agent,
profile, relative update time, session ID, and a profile-bound resume command.")]
    Here,
    /// List configured Claude/Codex profiles.
    Ls,
    /// Show one configured profile.
    Show {
        /// Tool to inspect: claude or codex.
        tool: String,
        /// Profile name to inspect.
        #[arg(long)]
        profile: String,
    },
    /// Show usage distribution and failure rates.
    Stats {
        #[arg(long)]
        today: bool,
    },
    /// Show configured tools and profiles.
    Status { tool: Option<String> },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Open config.toml with $VISUAL or $EDITOR.
    Edit,
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
            parse_from(["paths"]).cmd,
            Cmd::Paths { json: false }
        ));
        assert!(matches!(
            parse_from(["paths", "--json"]).cmd,
            Cmd::Paths { json: true }
        ));
        assert!(matches!(parse_from(["here"]).cmd, Cmd::Here));
        assert!(matches!(
            parse_from(["stats", "--today"]).cmd,
            Cmd::Stats { today: true }
        ));
        assert!(matches!(
            parse_from(["show", "claude", "--profile", "work"]).cmd,
            Cmd::Show { tool, profile } if tool == "claude" && profile == "work"
        ));
        assert!(matches!(
            parse_from(["status", "codex"]).cmd,
            Cmd::Status { tool } if tool.as_deref() == Some("codex")
        ));
        assert!(matches!(
            parse_from(["add", "codex", "--profile", "personal"]).cmd,
            Cmd::Add { tool, profile } if tool == "codex" && profile == "personal"
        ));
        assert!(matches!(
            parse_from(["rm", "codex", "--profile", "personal", "--yes"]).cmd,
            Cmd::Rm { tool, profile, yes }
                if tool == "codex" && profile == "personal" && yes
        ));
        assert!(matches!(
            parse_from(["config"]).cmd,
            Cmd::Config { command: None }
        ));
        assert!(matches!(
            parse_from(["config", "edit"]).cmd,
            Cmd::Config {
                command: Some(ConfigCommand::Edit)
            }
        ));
        assert!(matches!(
            parse_from(["fix", "codex", "--profile", "personal"]).cmd,
            Cmd::Fix { tool, profile } if tool == "codex" && profile == "personal"
        ));
        assert!(matches!(
            parse_from(["disable", "codex", "--profile", "personal"]).cmd,
            Cmd::Disable { tool, profile } if tool == "codex" && profile == "personal"
        ));
        assert!(matches!(
            parse_from(["enable", "claude", "--profile", "work team"]).cmd,
            Cmd::Enable { tool, profile } if tool == "claude" && profile == "work team"
        ));
        assert!(matches!(
            parse_from(["enable", "codex", "--profile=-work"]).cmd,
            Cmd::Enable { tool, profile } if tool == "codex" && profile == "-work"
        ));
        assert!(matches!(
            parse_from(["bypass", "codex", "--profile", "personal"]).cmd,
            Cmd::Bypass { tool, profile } if tool == "codex" && profile == "personal"
        ));
        assert!(matches!(
            parse_from(["unbypass", "claude", "--profile", "work team"]).cmd,
            Cmd::Unbypass { tool, profile } if tool == "claude" && profile == "work team"
        ));
        for args in [&["bypass", "codex"][..], &["unbypass", "claude"][..]] {
            assert!(
                Cli::try_parse_from(std::iter::once("rtr").chain(args.iter().copied())).is_err(),
                "missing --profile parsed: {args:?}"
            );
        }
    }

    #[test]
    fn slash_profile_command_arguments_are_rejected() {
        for args in [
            ["enable", "codex/personal"],
            ["disable", "codex/personal"],
            ["show", "codex/personal"],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("rtr").chain(args)).is_err(),
                "slash-form command parsed: {args:?}"
            );
        }
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
            "Pause a profile and bring it back later:",
            "rtr disable codex --profile personal",
            "rtr enable codex --profile personal",
            "Bypass a broken profile home and restore isolation:",
            "rtr bypass codex --profile personal",
            "rtr unbypass codex --profile personal",
            "Maintain profiles and config:",
            "rtr fix codex --profile personal",
            "rtr rm codex --profile personal",
            "rtr config edit",
            "Discover isolated homes for integrations:",
            "rtr paths --json",
            "Resume work from the current directory:",
            "rtr here",
        ] {
            assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
        }
    }

    #[test]
    fn command_help_describes_profile_and_maintenance_behavior() {
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

        let remove = help_for(&["rm"]);
        assert!(
            remove.contains("permanently deletes auth and sessions"),
            "{remove}"
        );
        assert!(remove.contains("Skip the confirmation prompt"), "{remove}");

        let config = help_for(&["config"]);
        assert!(config.contains("resolved config.toml path"), "{config}");
        assert!(config.contains("VISUAL"), "{config}");
        assert!(config.contains("EDITOR"), "{config}");

        let fix = help_for(&["fix"]);
        assert!(fix.contains("re-authenticate in place"), "{fix}");
        assert!(fix.contains("leaves rotation unchanged"), "{fix}");

        let paths = help_for(&["paths"]);
        assert!(paths.contains("isolated native home"), "{paths}");
        assert!(paths.contains("machine-readable v1 contract"), "{paths}");
        assert!(paths.contains("--json"), "{paths}");

        let here = help_for(&["here"]);
        assert!(here.contains("five most recently updated"), "{here}");
        assert!(here.contains("exact current directory"), "{here}");
        assert!(here.contains("profile-bound resume command"), "{here}");
    }

    #[test]
    fn profile_command_help_describes_the_flag_style() {
        let disable = help_for(&["disable"]);
        assert!(
            disable.contains("Tool to disable: claude or codex"),
            "{disable}"
        );
        assert!(disable.contains("Profile name to disable"), "{disable}");
        assert!(disable.contains("native home"), "{disable}");
        assert!(
            disable.contains("Already disabled is a success"),
            "{disable}"
        );

        let enable = help_for(&["enable"]);
        assert!(
            enable.contains("Tool to enable: claude or codex"),
            "{enable}"
        );
        assert!(enable.contains("Profile name to enable"), "{enable}");
        assert!(enable.contains("rejoins automatic rotation"), "{enable}");
        assert!(enable.contains("Already enabled is a success"), "{enable}");

        let show = help_for(&["show"]);
        assert!(show.contains("Tool to inspect: claude or codex"), "{show}");
        assert!(show.contains("Profile name to inspect"), "{show}");

        for help in [disable, enable, show] {
            assert!(help.contains("--profile <PROFILE>"), "{help}");
            assert!(!help.contains("<tool>/<profile>"), "{help}");
        }
    }

    #[test]
    fn bypass_help_describes_default_home_and_persisted_undo() {
        let bypass = help_for(&["bypass"]);
        assert!(
            bypass.contains("Tool to bypass: claude or codex"),
            "{bypass}"
        );
        assert!(bypass.contains("Profile name to bypass"), "{bypass}");
        assert!(bypass.contains("default Claude or Codex home"), "{bypass}");
        assert!(bypass.contains("Already bypassed is a success"), "{bypass}");

        let unbypass = help_for(&["unbypass"]);
        assert!(
            unbypass.contains("Tool to restore: claude or codex"),
            "{unbypass}"
        );
        assert!(unbypass.contains("Profile name to restore"), "{unbypass}");
        assert!(
            unbypass.contains("isolated native home again"),
            "{unbypass}"
        );
        assert!(
            unbypass.contains("Already unbypassed is a success"),
            "{unbypass}"
        );
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
