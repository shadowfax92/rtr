//! Command-line surface for rtr's native Claude and Codex profile launcher.

use crate::output::ColorArgs;
use clap::{Args, Parser, Subcommand};

const TOP_LEVEL_LONG_ABOUT: &str = "\
Run Claude Code or Codex through named native homes. Each profile keeps its own
auth, settings, sessions, and skills. rtr launches the real CLI directly.

Shortest path:
  rtr init
  rtr add claude work
  rtr add codex personal
  rtr claude -p work [claude args...]
  rtr codex -p personal [codex args...]

Omit --profile to rotate through enabled profiles:
  rtr codex
  rtr codex

Put -- before child args that should not be parsed by rtr:
  rtr codex -- --profile native-codex-profile

Pause a profile and bring it back later:
  rtr disable codex personal
  rtr enable codex personal

Set automatic launch shares in percentages:
  rtr weight
  rtr weight codex -p personal 25
  rtr weight codex --reset

Bypass a broken profile home and restore isolation:
  rtr bypass codex personal
  rtr unbypass codex personal

Maintain profiles and config:
  rtr ls
  rtr fix codex personal
  rtr rm codex personal
  rtr config edit

Discover isolated homes for integrations:
  rtr paths -j

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
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Launch Claude Code with a selected subscription profile.
    #[command(long_about = "\
Launch Claude Code in one configured profile.

With -p, rtr uses that profile and leaves rotation unchanged. Without
-p, rtr uses the next enabled Claude profile according to rtr weight shares.
Remaining arguments are passed to Claude Code.")]
    Claude(ToolRunArgs),
    /// Launch Codex with a selected subscription profile.
    #[command(long_about = "\
Launch Codex in one configured profile.

With -p, rtr uses that profile and leaves rotation unchanged. Without
-p, rtr uses the next enabled Codex profile according to rtr weight shares.
Remaining arguments are passed to Codex.")]
    Codex(ToolRunArgs),
    /// Create a Claude/Codex profile and launch the tool to sign in.
    #[command(override_usage = "add <TOOL> [PROFILE] [-p <NAME>]")]
    Add {
        /// Tool to add: claude or codex.
        tool: String,
        /// Profile name to create.
        #[command(flatten)]
        profile: ProfileArgs,
    },
    /// Delete a Claude/Codex profile and its native home.
    #[command(long_about = "\
Delete one configured profile and its native home.

This permanently deletes auth and sessions stored in that profile. rtr prints
the exact home path and asks for confirmation unless --yes is supplied.")]
    #[command(override_usage = "rm <TOOL> [PROFILE] [-p <NAME>]")]
    Rm {
        /// Tool to remove from: claude or codex.
        tool: String,
        /// Profile name to delete.
        #[command(flatten)]
        profile: ProfileArgs,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Print or edit the resolved config.toml path.
    #[command(long_about = "\
Print the resolved config.toml path.

The default output is only the path for script-friendly use. `rtr config edit`
opens an existing config with $VISUAL, falling back to $EDITOR.")]
    Config {
        #[command(flatten)]
        output: ColorArgs,
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Repair an existing profile's authentication in place.
    #[command(long_about = "\
Repair one existing profile and re-authenticate in place.

rtr removes recognized stale credential locks from only that profile home, then
launches the configured tool there. This leaves rotation unchanged.")]
    #[command(override_usage = "fix <TOOL> [PROFILE] [-p <NAME>]")]
    Fix {
        /// Tool to repair: claude or codex.
        tool: String,
        /// Existing profile name to repair.
        #[command(flatten)]
        profile: ProfileArgs,
    },
    /// Re-enable a profile for selection and rotation.
    #[command(long_about = "\
Re-enable one profile.

The profile rejoins automatic rotation and can be forced with --profile again.
Its native home, sign-in, and skills were kept while disabled, so no new
sign-in is needed. Already enabled is a success.")]
    #[command(override_usage = "enable <TOOL> [PROFILE] [-p <NAME>]")]
    Enable {
        /// Tool to enable: claude or codex.
        tool: String,
        /// Profile name to enable.
        #[command(flatten)]
        profile: ProfileArgs,
    },
    /// Disable a profile without deleting its native home.
    #[command(long_about = "\
Disable one profile.

Only the enabled flag in config.toml changes; the profile's native home,
sign-in, and skills stay in place. Disabled profiles are skipped by rotation
and rejected by --profile until re-enabled. Already disabled is a success.")]
    #[command(override_usage = "disable <TOOL> [PROFILE] [-p <NAME>]")]
    Disable {
        /// Tool to disable: claude or codex.
        tool: String,
        /// Profile name to disable.
        #[command(flatten)]
        profile: ProfileArgs,
    },
    /// Run a profile with the tool's default home.
    #[command(long_about = "\
Bypass one profile's isolated native home as <tool> <name>.

Runs keep selecting the profile normally. They launch with the
default Claude or Codex home and no rtr-managed native-home environment
override. Already bypassed is a success. The setting persists until
`rtr unbypass`.")]
    #[command(override_usage = "bypass <TOOL> [PROFILE] [-p <NAME>]")]
    Bypass {
        /// Tool to bypass: claude or codex.
        tool: String,
        /// Profile name to bypass.
        #[command(flatten)]
        profile: ProfileArgs,
    },
    /// Restore a profile's isolated native home.
    #[command(long_about = "\
Stop bypassing one profile as <tool> <name>.

Future runs use the profile's isolated native home again. Selection and
rotation are unchanged. Already unbypassed is a success.")]
    #[command(override_usage = "unbypass <TOOL> [PROFILE] [-p <NAME>]")]
    Unbypass {
        /// Tool to restore: claude or codex.
        tool: String,
        /// Profile name to restore.
        #[command(flatten)]
        profile: ProfileArgs,
    },
    /// List resolved isolated native homes for configured profiles.
    #[command(long_about = "\
List every configured profile's resolved isolated native home.

Disabled, bypassed, and missing homes are included. Use --json for the
machine-readable v1 contract; human output is for inspection only.")]
    Paths {
        #[command(flatten)]
        output: ColorArgs,
        /// Emit the versioned machine-readable contract.
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Search native conversations across every configured profile.
    #[command(long_about = "\
Search Claude Code and Codex conversations across every configured native home.

The interactive picker ANDs case-insensitive literal terms across titles, project
paths, native IDs, first prompts, and human dialogue. Enter forks the selected
conversation; Ctrl-R resumes it in place. Use -l or -j for
non-interactive output. Tab cycles Conversation, Matches, and Details previews.
Alt-H changes directory scope; Alt-T / Alt-A cycle agent / profile. F1 shows help.")]
    Sessions(SessionsArgs),
    /// Inspect one exact session for an unattended watchdog; never opens a picker.
    Session(SessionArgs),
    /// Clear remembered usage exhaustion after buying credits.
    #[command(override_usage = "clear-limit <TOOL> [PROFILE] [-p <NAME>]")]
    ClearLimit {
        tool: String,
        #[command(flatten)]
        profile: ProfileArgs,
    },
    /// Fork an exact native conversation, or choose one interactively.
    #[command(long_about = "\
Fork a native Claude Code or Codex conversation into the next enabled profile,
using the same share allocation as normal launches. -d pins the destination
without advancing rotation; -p filters the source. A different profile
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
    /// Show profile state and recorded launch counts.
    #[command(
        long_about = "Show configured profiles with their enable/bypass state and recorded launch counts.\n\nCounts cover the current local day by default; -a includes all time. Profiles\nwithout usage show zero, and removed profiles with usage appear separately.\nFAILED counts non-zero or unavailable child exits, not authentication health."
    )]
    Ls {
        /// Count launches across all time instead of the current local day.
        #[arg(short = 'a', long)]
        all: bool,
        #[command(flatten)]
        output: ColorArgs,
    },
    /// Show one configured profile.
    #[command(override_usage = "show <TOOL> [PROFILE] [-p <NAME>]")]
    Show {
        /// Tool to inspect: claude or codex.
        tool: String,
        /// Profile name to inspect.
        #[command(flatten)]
        profile: ProfileArgs,
    },
    /// Show configured tools and profiles.
    Status { tool: Option<String> },
    /// Show or change automatic launch shares in percentages.
    #[command(long_about = "\
Show or change automatic launch shares. Profiles share equally by default.

Set a whole percentage from 0 to 100 with -p NAME PERCENT (25 and 25%
are equivalent). Other profiles without an override divide the remainder
equally. Fixed shares stay fixed when another share changes. A zero share
allows explicit --profile launches but excludes automatic selection.

With no arguments, show all tools; with TOOL, show that tool. --reset removes
all percentage overrides for the tool, restoring equal shares. Changes apply
to future automatic launches and forks, not running sessions or quota usage.")]
    Weight(WeightArgs),
}

/// A required management target accepts the short positional spelling while
/// retaining --profile for integrations. Resolve both before any state mutation.
#[derive(Args, Debug)]
#[group(required = true, multiple = true)]
pub struct ProfileArgs {
    /// Profile name (also accepted with -p/--profile).
    #[arg(value_name = "PROFILE")]
    pub positional: Option<String>,
    /// Profile name, as an alternative to the positional argument.
    #[arg(short = 'p', long = "profile", value_name = "NAME")]
    pub flag: Option<String>,
}

impl ProfileArgs {
    pub fn name(&self) -> anyhow::Result<&str> {
        if let (Some(positional), Some(flag)) = (&self.positional, &self.flag) {
            anyhow::ensure!(
                positional == flag,
                "conflicting profile names: positional profile differs from --profile"
            );
        }
        self.positional
            .as_deref()
            .or(self.flag.as_deref())
            .ok_or_else(|| anyhow::anyhow!("a profile is required"))
    }
}

#[derive(Args, Debug)]
pub struct WeightArgs {
    /// Tool to inspect or change: claude or codex.
    pub tool: Option<String>,
    /// Whole percentage, optionally followed by %.
    #[arg(value_parser = parse_percent, requires_all = ["tool", "profile"])]
    pub percent: Option<u8>,
    /// Profile whose automatic share should change.
    #[arg(short, long, requires_all = ["tool", "percent"])]
    pub profile: Option<String>,
    /// Restore equal shares by removing all overrides for this tool.
    #[arg(long, requires = "tool", conflicts_with_all = ["profile", "percent"])]
    pub reset: bool,
    #[command(flatten)]
    pub output: ColorArgs,
}

fn parse_percent(value: &str) -> std::result::Result<u8, String> {
    let digits = value.strip_suffix('%').unwrap_or(value);
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        if let Ok(percent @ 0..=100) = digits.parse::<u8>() {
            return Ok(percent);
        }
    }
    Err("expected a whole percentage from 0 to 100 (for example: 25 or 25%)".into())
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
    /// Include Codex exec runs and subagent threads.
    #[arg(long)]
    pub include_non_conversations: bool,
    /// Restrict results to claude or codex.
    #[arg(short = 't', long, value_parser = ["claude", "codex"])]
    pub tool: Option<String>,
    /// Restrict results to one configured rtr profile name.
    #[arg(short = 'p', long)]
    pub profile: Option<String>,
    /// Restrict results to conversations created in the current directory.
    #[arg(long)]
    pub here: bool,
    /// Seed the interactive search query.
    #[arg(short = 'q', long)]
    pub query: Option<String>,
    /// Print a human-readable catalog instead of opening the picker.
    #[arg(short = 'l', long, conflicts_with = "json")]
    pub list: bool,
    /// Print the versioned machine-readable catalog instead of opening the picker.
    #[arg(short = 'j', long, conflicts_with = "list")]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ConversationForkArgs {
    /// Plan the destination without changing state, copying, or launching.
    #[arg(short = 'n', long, requires = "json")]
    pub dry_run: bool,
    /// Emit the versioned dry-run contract.
    #[arg(short = 'j', long, requires = "dry_run")]
    pub json: bool,
    /// Fail instead of opening the interactive picker.
    #[arg(long)]
    pub no_picker: bool,
    /// Atomically write the prepared destination before starting the agent.
    #[arg(long, conflicts_with = "dry_run")]
    pub report: Option<std::path::PathBuf>,
    #[command(flatten)]
    pub source: ConversationOpenArgs,
    /// Fork into this enabled profile instead of using automatic rotation.
    #[arg(short = 'd', long, visible_alias = "to")]
    pub to_profile: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct SessionArgs {
    pub id: String,
    #[arg(short = 't', long, value_parser = ["claude", "codex"])]
    pub tool: Option<String>,
    #[arg(short = 'p', long)]
    pub profile: Option<String>,
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ConversationOpenArgs {
    /// Include Codex exec runs and subagent threads in the picker.
    #[arg(long)]
    pub include_non_conversations: bool,
    /// Native session ID or exact native session name.
    pub selector: Option<String>,
    /// Restrict lookup to claude or codex.
    #[arg(short = 't', long, value_parser = ["claude", "codex"])]
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
        assert!(matches!(parse_from(["ls"]).cmd, Cmd::Ls { all: false, .. }));
        assert!(matches!(
            parse_from(["paths"]).cmd,
            Cmd::Paths { json: false, .. }
        ));
        assert!(matches!(
            parse_from(["paths", "--json"]).cmd,
            Cmd::Paths { json: true, .. }
        ));
        assert!(matches!(
            parse_from(["ls", "--all"]).cmd,
            Cmd::Ls { all: true, .. }
        ));
        assert!(Cli::try_parse_from(["rtr", "ls", "--today"]).is_err());
        assert!(matches!(
            parse_from(["show", "claude", "--profile", "work"]).cmd,
            Cmd::Show { tool, profile } if tool == "claude" && profile.name().unwrap() == "work"
        ));
        assert!(matches!(
            parse_from(["status", "codex"]).cmd,
            Cmd::Status { tool } if tool.as_deref() == Some("codex")
        ));
        assert!(matches!(
            parse_from(["add", "codex", "--profile", "personal"]).cmd,
            Cmd::Add { tool, profile } if tool == "codex" && profile.name().unwrap() == "personal"
        ));
        assert!(matches!(
            parse_from(["rm", "codex", "--profile", "personal", "--yes"]).cmd,
            Cmd::Rm { tool, profile, yes }
                if tool == "codex" && profile.name().unwrap() == "personal" && yes
        ));
        assert!(matches!(
            parse_from(["config"]).cmd,
            Cmd::Config { command: None, .. }
        ));
        assert!(matches!(
            parse_from(["config", "edit"]).cmd,
            Cmd::Config {
                command: Some(ConfigCommand::Edit),
                ..
            }
        ));
        assert!(matches!(
            parse_from(["fix", "codex", "--profile", "personal"]).cmd,
            Cmd::Fix { tool, profile } if tool == "codex" && profile.name().unwrap() == "personal"
        ));
        assert!(matches!(
            parse_from(["disable", "codex", "--profile", "personal"]).cmd,
            Cmd::Disable { tool, profile } if tool == "codex" && profile.name().unwrap() == "personal"
        ));
        assert!(matches!(
            parse_from(["enable", "claude", "--profile", "work team"]).cmd,
            Cmd::Enable { tool, profile } if tool == "claude" && profile.name().unwrap() == "work team"
        ));
        assert!(matches!(
            parse_from(["enable", "codex", "--profile=-work"]).cmd,
            Cmd::Enable { tool, profile } if tool == "codex" && profile.name().unwrap() == "-work"
        ));
        assert!(matches!(
            parse_from(["bypass", "codex", "--profile", "personal"]).cmd,
            Cmd::Bypass { tool, profile } if tool == "codex" && profile.name().unwrap() == "personal"
        ));
        assert!(matches!(
            parse_from(["unbypass", "claude", "--profile", "work team"]).cmd,
            Cmd::Unbypass { tool, profile } if tool == "claude" && profile.name().unwrap() == "work team"
        ));
        for args in [&["bypass", "codex"][..], &["unbypass", "claude"][..]] {
            assert!(
                Cli::try_parse_from(std::iter::once("rtr").chain(args.iter().copied())).is_err(),
                "missing --profile parsed: {args:?}"
            );
        }
    }

    #[test]
    fn inspection_color_flags_do_not_capture_native_color_arguments() {
        use crate::output::ColorMode;
        assert!(matches!(
            parse_from(["ls", "--all", "--color=always"]).cmd,
            Cmd::Ls {
                all: true,
                output: ColorArgs {
                    color: ColorMode::Always
                }
            }
        ));
        assert!(matches!(
            parse_from(["paths", "--json", "--color", "never"]).cmd,
            Cmd::Paths {
                json: true,
                output: ColorArgs {
                    color: ColorMode::Never
                }
            }
        ));
        assert!(matches!(
            parse_from(["config", "--color=auto"]).cmd,
            Cmd::Config {
                command: None,
                output: ColorArgs {
                    color: ColorMode::Auto
                }
            }
        ));
        let Cmd::Codex(args) = parse_from(["codex", "--color", "never", "--profile", "work"]).cmd
        else {
            panic!("expected Codex");
        };
        assert_eq!(args.profile.as_deref(), Some("work"));
        assert_eq!(args.args, v(&["--color", "never"]));
        assert!(Cli::try_parse_from(["rtr", "stats"]).is_err());
        assert!(Cli::command().find_subcommand("stats").is_none());
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
            "rtr add claude work",
            "rtr add codex personal",
            "rtr claude -p work [claude args...]",
            "rtr codex -p personal [codex args...]",
            "Omit --profile to rotate through enabled profiles:",
            "rtr codex -- --profile native-codex-profile",
            "Pause a profile and bring it back later:",
            "rtr disable codex personal",
            "rtr enable codex personal",
            "Bypass a broken profile home and restore isolation:",
            "rtr bypass codex personal",
            "rtr unbypass codex personal",
            "Maintain profiles and config:",
            "rtr fix codex personal",
            "rtr rm codex personal",
            "rtr config edit",
            "Discover isolated homes for integrations:",
            "rtr paths -j",
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
        assert!(
            add.contains("Profile name (also accepted with -p/--profile)"),
            "{add}"
        );

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
        assert!(
            disable.contains("Profile name (also accepted with -p/--profile)"),
            "{disable}"
        );
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
        assert!(
            enable.contains("Profile name (also accepted with -p/--profile)"),
            "{enable}"
        );
        assert!(enable.contains("rejoins automatic rotation"), "{enable}");
        assert!(enable.contains("Already enabled is a success"), "{enable}");

        let show = help_for(&["show"]);
        assert!(show.contains("Tool to inspect: claude or codex"), "{show}");
        assert!(
            show.contains("Profile name (also accepted with -p/--profile)"),
            "{show}"
        );

        for help in [disable, enable, show] {
            assert!(help.contains("-p, --profile <NAME>"), "{help}");
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
        assert!(
            bypass.contains("Profile name (also accepted with -p/--profile)"),
            "{bypass}"
        );
        assert!(bypass.contains("default Claude or Codex home"), "{bypass}");
        assert!(bypass.contains("Already bypassed is a success"), "{bypass}");

        let unbypass = help_for(&["unbypass"]);
        assert!(
            unbypass.contains("Tool to restore: claude or codex"),
            "{unbypass}"
        );
        assert!(
            unbypass.contains("Profile name (also accepted with -p/--profile)"),
            "{unbypass}"
        );
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
    fn management_profile(cmd: &Cmd) -> &ProfileArgs {
        match cmd {
            Cmd::Add { profile, .. }
            | Cmd::Rm { profile, .. }
            | Cmd::Fix { profile, .. }
            | Cmd::Enable { profile, .. }
            | Cmd::Disable { profile, .. }
            | Cmd::Bypass { profile, .. }
            | Cmd::Unbypass { profile, .. }
            | Cmd::Show { profile, .. }
            | Cmd::ClearLimit { profile, .. } => profile,
            _ => panic!("not a management command"),
        }
    }

    #[test]
    fn all_management_commands_accept_positionals_flags_and_detect_conflicts() {
        for command in [
            "add",
            "rm",
            "fix",
            "enable",
            "disable",
            "bypass",
            "unbypass",
            "show",
            "clear-limit",
        ] {
            for tail in [
                vec!["work"],
                vec!["-p", "work"],
                vec!["--profile", "work"],
                vec!["work", "-p", "work"],
            ] {
                let parsed = parse_from([command, "codex"].into_iter().chain(tail));
                assert_eq!(management_profile(&parsed.cmd).name().unwrap(), "work");
            }
            let parsed = parse_from([command, "codex", "work", "-p", "other"]);
            assert!(management_profile(&parsed.cmd)
                .name()
                .unwrap_err()
                .to_string()
                .contains("conflicting profile names"));
            assert!(Cli::try_parse_from(["rtr", command, "codex"]).is_err());
        }
    }

    #[test]
    fn short_flags_match_existing_long_forms() {
        for (short, long) in [
            (vec!["init", "-f"], vec!["init", "--force"]),
            (
                vec!["rm", "codex", "-p", "a", "-y"],
                vec!["rm", "codex", "--profile", "a", "--yes"],
            ),
            (vec!["ls", "-a"], vec!["ls", "--all"]),
            (vec!["paths", "-j"], vec!["paths", "--json"]),
            (
                vec!["sessions", "-t", "codex", "-p", "a", "-l"],
                vec!["sessions", "--tool", "codex", "--profile", "a", "--list"],
            ),
            (vec!["sessions", "-j"], vec!["sessions", "--json"]),
            (
                vec!["session", "id", "-t", "codex", "-p", "a", "-j"],
                vec![
                    "session",
                    "id",
                    "--tool",
                    "codex",
                    "--profile",
                    "a",
                    "--json",
                ],
            ),
            (
                vec![
                    "fork", "id", "-t", "codex", "-p", "a", "-d", "b", "-n", "-j",
                ],
                vec![
                    "fork",
                    "id",
                    "--tool",
                    "codex",
                    "--profile",
                    "a",
                    "--to-profile",
                    "b",
                    "--dry-run",
                    "--json",
                ],
            ),
            (
                vec!["fork", "id", "--to", "b"],
                vec!["fork", "id", "--to-profile", "b"],
            ),
            (
                vec!["resume", "id", "-t", "codex", "-p", "a"],
                vec!["resume", "id", "--tool", "codex", "--profile", "a"],
            ),
            (vec!["claude", "-p", "a"], vec!["claude", "--profile", "a"]),
            (vec!["codex", "-p", "a"], vec!["codex", "--profile", "a"]),
            (
                vec!["weight", "codex", "-p", "a", "20"],
                vec!["weight", "codex", "--profile", "a", "20"],
            ),
        ] {
            assert_eq!(
                format!("{:?}", parse_from(short).cmd),
                format!("{:?}", parse_from(long).cmd)
            );
        }
    }

    #[test]
    fn native_short_flags_and_wrapper_aliases_stay_after_the_delimiter() {
        let native = [
            "-p",
            "prompt",
            "-c",
            "x=y",
            "-m",
            "model",
            "-r",
            "id",
            "-t",
            "native",
            "-j",
            "-n",
            "-d",
            "dest",
            "--to",
            "other",
            "--profile",
            "native",
        ];
        for tool in ["claude", "codex"] {
            let cmd = parse_from([tool, "-p", "work", "--"].into_iter().chain(native)).cmd;
            let args = match cmd {
                Cmd::Claude(a) | Cmd::Codex(a) => a,
                _ => unreachable!(),
            };
            assert_eq!(args.profile.as_deref(), Some("work"));
            assert_eq!(args.args, v(&native));
        }
        for command in ["fork", "resume"] {
            let cmd = parse_from(
                [command, "id", "-t", "codex", "-p", "work", "--"]
                    .into_iter()
                    .chain(native),
            )
            .cmd;
            let args = match cmd {
                Cmd::Fork(a) => {
                    assert!(a.to_profile.is_none());
                    assert!(!a.dry_run && !a.json);
                    a.source
                }
                Cmd::Resume(a) => a,
                _ => unreachable!(),
            };
            assert_eq!(args.profile.as_deref(), Some("work"));
            assert_eq!(args.tool.as_deref(), Some("codex"));
            assert_eq!(args.args, v(&native));
        }
    }
}
