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

Browse conversations from the current directory:
  rtr sessions --here

Search every native conversation (Enter forks; Ctrl-R resumes):
  rtr sessions
  rtr resume <session-id-or-name>
  rtr fork <session-id-or-name>";

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
    /// Search native conversations across every configured profile.
    #[command(long_about = "\
Search Claude Code and Codex conversations across every configured native home.

The interactive picker searches the complete user/assistant dialogue as well as
titles, prompts, paths, tools, profiles, and native IDs. Enter forks the selected
conversation; Ctrl-R resumes it in place. Use --list or --json for
non-interactive output. Tab cycles Conversation, Matches, and Details previews.
Alt-H changes directory scope; Alt-T / Alt-A cycle agent / profile. F1 shows help.")]
    Sessions(SessionsArgs),
    /// Fork an exact native conversation, or choose one interactively.
    #[command(long_about = "\
Fork a native Claude Code or Codex conversation into the next enabled profile,
using the same round-robin as normal launches. --to-profile pins the destination
without advancing rotation; --profile filters the source. A different profile
receives an independent copy of the conversation in its isolated home.

SESSION may be a native ID or exact native name. When omitted or ambiguous,
rtr opens the conversation picker. Arguments after -- are passed to the native tool.")]
    Fork(ConversationForkArgs),
    /// Resume an exact native conversation, or choose one interactively.
    #[command(long_about = "\
Resume a native Claude Code or Codex conversation in the isolated profile that
owns it. SESSION may be a native ID or exact native name. When omitted or
ambiguous, rtr opens the conversation picker. Arguments after -- are passed to
the native tool. Enter resumes; Ctrl-F forks explicitly.")]
    Resume(ConversationOpenArgs),
    /// Render one bounded transcript preview by its encoded conversation key.
    #[command(name = "conversation-preview", hide = true)]
    ConversationPreview { key: String },
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

#[derive(Args, Debug, Clone)]
pub struct SessionsArgs {
    /// Restrict results to claude or codex.
    #[arg(long, value_parser = ["claude", "codex"])]
    pub tool: Option<String>,
    /// Restrict results to one configured rtr profile name.
    #[arg(short = 'p', long)]
    pub profile: Option<String>,
    /// Restrict results to conversations created in the current directory.
    #[arg(long)]
    pub here: bool,
    /// Seed the interactive fuzzy-search query.
    #[arg(short = 'q', long)]
    pub query: Option<String>,
    /// Print a human-readable catalog instead of opening the picker.
    #[arg(long, conflicts_with = "json")]
    pub list: bool,
    /// Print the versioned machine-readable catalog instead of opening the picker.
    #[arg(long, conflicts_with = "list")]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ConversationForkArgs {
    #[command(flatten)]
    pub source: ConversationOpenArgs,
    /// Fork into this enabled profile instead of using automatic rotation.
    #[arg(long)]
    pub to_profile: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ConversationOpenArgs {
    /// Native session ID or exact native session name.
    pub selector: Option<String>,
    /// Restrict lookup to claude or codex.
    #[arg(long, value_parser = ["claude", "codex"])]
    pub tool: Option<String>,
    /// Restrict lookup to one configured rtr profile name.
    #[arg(short = 'p', long)]
    pub profile: Option<String>,
    /// Restrict lookup to conversations created in the current directory.
    #[arg(long)]
    pub here: bool,
    /// Arguments passed through after the native resume/fork invocation.
    #[arg(last = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Parse raw arguments without the program name.
pub fn parse_from<I, S>(raw: I) -> Cli
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let raw = raw.into_iter().map(Into::into).collect();
    let raw = normalize_tool_run_args_for_clap(raw);
    Cli::parse_from(std::iter::once("rtr".to_string()).chain(raw))
}

/// Keep rtr's profile selector parseable when shell aliases prepend native
/// tool arguments to an invocation, without weakening the `--` boundary.
fn normalize_tool_run_args_for_clap(raw: Vec<String>) -> Vec<String> {
    if !matches!(raw.first().map(String::as_str), Some("claude" | "codex")) {
        return raw;
    }

    // `trailing_var_arg` must accept arbitrary native flags, but after the first
    // such flag Clap intentionally stops recognizing rtr options. Treat -p /
    // --profile anywhere before `--` as wrapper-owned and move it ahead of that
    // boundary; arguments after `--` remain wholly owned by the child CLI.
    let mut profile_args = Vec::new();
    let mut child_args = Vec::new();
    let mut saw_delimiter = false;
    let mut index = 1;
    while index < raw.len() {
        let argument = &raw[index];
        if argument == "--" {
            saw_delimiter = true;
            child_args.extend_from_slice(&raw[index + 1..]);
            break;
        }

        if matches!(argument.as_str(), "-p" | "--profile") {
            profile_args.push(argument.clone());
            index += 1;
            if raw.get(index).is_some_and(|value| value != "--") {
                profile_args.push(raw[index].clone());
                index += 1;
            }
            continue;
        }

        if argument.starts_with("--profile=") || (argument.starts_with("-p") && argument.len() > 2)
        {
            profile_args.push(argument.clone());
            index += 1;
            continue;
        }

        child_args.push(argument.clone());
        index += 1;
    }

    if profile_args.is_empty() && !saw_delimiter {
        return raw;
    }

    let mut reordered = Vec::with_capacity(raw.len());
    reordered.push(raw[0].clone());
    reordered.extend(profile_args);
    if saw_delimiter {
        // Put the escape before every child argument so Clap consumes it even
        // when the original invocation had already started the trailing vararg.
        reordered.push("--".to_string());
    }
    reordered.extend(child_args);
    reordered
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
    fn parse_profile_appended_after_alias_arguments() {
        match parse_from([
            "claude",
            "--effort",
            "max",
            "--model",
            "claude-opus-5",
            "--dangerously-skip-permissions",
            "-p",
            "eng",
        ])
        .cmd
        {
            Cmd::Claude(args) => {
                assert_eq!(args.profile.as_deref(), Some("eng"));
                assert_eq!(
                    args.args,
                    v(&[
                        "--effort",
                        "max",
                        "--model",
                        "claude-opus-5",
                        "--dangerously-skip-permissions",
                    ])
                );
            }
            other => panic!("expected Claude, got {other:?}"),
        }

        match parse_from(["codex", "--model", "gpt-5.6-sol", "--profile=eng"]).cmd {
            Cmd::Codex(args) => {
                assert_eq!(args.profile.as_deref(), Some("eng"));
                assert_eq!(args.args, v(&["--model", "gpt-5.6-sol"]));
            }
            other => panic!("expected Codex, got {other:?}"),
        }

        match parse_from(["claude", "--effort", "max", "--", "--profile", "native"]).cmd {
            Cmd::Claude(args) => {
                assert_eq!(args.profile.as_deref(), None);
                assert_eq!(args.args, v(&["--effort", "max", "--profile", "native"]));
            }
            other => panic!("expected Claude, got {other:?}"),
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
    fn parse_conversation_picker_and_direct_open_commands() {
        match parse_from([
            "sessions",
            "--tool",
            "codex",
            "--profile",
            "eng",
            "--here",
            "--query",
            "release work",
            "--json",
        ])
        .cmd
        {
            Cmd::Sessions(args) => {
                assert_eq!(args.tool.as_deref(), Some("codex"));
                assert_eq!(args.profile.as_deref(), Some("eng"));
                assert!(args.here);
                assert_eq!(args.query.as_deref(), Some("release work"));
                assert!(args.json);
                assert!(!args.list);
            }
            other => panic!("expected sessions, got {other:?}"),
        }

        match parse_from([
            "fork",
            "native-id",
            "--tool",
            "claude",
            "--profile",
            "work",
            "--",
            "--model",
            "opus",
        ])
        .cmd
        {
            Cmd::Fork(args) => {
                assert_eq!(args.source.selector.as_deref(), Some("native-id"));
                assert_eq!(args.source.tool.as_deref(), Some("claude"));
                assert_eq!(args.source.profile.as_deref(), Some("work"));
                assert_eq!(args.source.args, v(&["--model", "opus"]));
                assert!(args.to_profile.is_none());
            }
            other => panic!("expected fork, got {other:?}"),
        }

        assert!(matches!(
            parse_from(["resume", "named thread", "--here"]).cmd,
            Cmd::Resume(ConversationOpenArgs { here: true, .. })
        ));
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
    fn destination_override_is_fork_only_and_does_not_replace_source_filter() {
        match parse_from([
            "fork",
            "native-id",
            "--profile",
            "source",
            "--to-profile",
            "destination",
        ])
        .cmd
        {
            Cmd::Fork(args) => {
                assert_eq!(args.source.profile.as_deref(), Some("source"));
                assert_eq!(args.to_profile.as_deref(), Some("destination"));
            }
            other => panic!("expected fork, got {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["rtr", "resume", "native-id", "--to-profile", "destination"])
                .is_err()
        );
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
            "Browse conversations from the current directory:",
            "rtr sessions --here",
            "Search every native conversation (Enter forks; Ctrl-R resumes):",
            "rtr sessions",
            "rtr resume <session-id-or-name>",
            "rtr fork <session-id-or-name>",
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

        let sessions = help_for(&["sessions"]);
        assert!(
            sessions.contains("every configured native home"),
            "{sessions}"
        );
        assert!(sessions.contains("Enter forks"), "{sessions}");
        assert!(sessions.contains("Ctrl-R resumes"), "{sessions}");
        assert!(sessions.contains("--list"), "{sessions}");
        assert!(sessions.contains("--json"), "{sessions}");

        let fork = help_for(&["fork"]);
        assert!(fork.contains("next enabled profile"), "{fork}");
        assert!(fork.contains("--to-profile"), "{fork}");
        assert!(fork.contains("native ID or exact native name"), "{fork}");
        assert!(fork.contains("Arguments after --"), "{fork}");
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
