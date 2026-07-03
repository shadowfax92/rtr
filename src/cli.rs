//! Command-line surface for rtr.
//!
//! Besides the explicit subcommands, `rtr <tool> [args]` is a convenience alias
//! for `rtr run <tool> [args]`. That is handled by [`normalize_args`], which
//! prepends `run` when the first token is neither a known subcommand nor a flag.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rtr",
    version,
    about = "Per-binary profile launcher and MITM capture for Claude Code and Codex"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Scaffold a starter config.toml and generate the CA if missing.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Launch a configured tool with its traffic routed through the MITM proxy.
    Run {
        /// Tool name as defined in config.toml.
        tool: String,
        /// Reveal secret header values in terminal output.
        #[arg(long)]
        show_secrets: bool,
        /// Pipe and tee the tool's stdout/stderr to a per-run output.log (may
        /// degrade full-screen TUIs). Off by default: the child owns the
        /// terminal and request captures still land in capture.jsonl.
        #[arg(long)]
        log: bool,
        /// Arguments passed through to the tool (everything after the tool name).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Launch Claude Code with a selected subscription profile.
    Claude(SubscriptionRunArgs),
    /// Launch Codex with a selected subscription profile.
    Codex(SubscriptionRunArgs),
    /// Create/use a first-class profile, then launch it for login/capture.
    Capture {
        tool: String,
        #[arg(long)]
        profile: String,
    },
    /// Import captured headers for legacy/custom rewrite profiles.
    Import {
        tool: String,
        #[arg(long)]
        profile: String,
        #[arg(long = "from-capture")]
        from_capture: PathBuf,
        #[arg(long, conflicts_with = "no_overwrite")]
        force: bool,
        #[arg(long = "no-overwrite")]
        no_overwrite: bool,
        #[arg(long)]
        show_secrets: bool,
    },
    /// List configured Claude/Codex profiles.
    Ls,
    /// Show one profile as `<tool>/<profile>`.
    Show {
        target: String,
        #[arg(long)]
        show_secrets: bool,
    },
    /// Show usage distribution and failure rates.
    Stats {
        #[arg(long)]
        today: bool,
    },
    /// Set the active profile. `switch <tool> <profile>` or `switch <profile>`.
    Switch {
        first: String,
        second: Option<String>,
    },
    /// Show tools, active profiles, CA fingerprint, and trust state.
    Status { tool: Option<String> },
    /// Install the rtr CA into a macOS keychain as a trusted root.
    Trust {
        #[arg(long)]
        system: bool,
    },
    /// Remove the rtr CA from a macOS keychain.
    Untrust {
        #[arg(long)]
        system: bool,
    },
    /// CA management.
    Ca {
        #[command(subcommand)]
        cmd: CaCmd,
    },
}

#[derive(Args, Debug, Clone)]
pub struct SubscriptionRunArgs {
    #[arg(short = 'p', long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub show_secrets: bool,
    #[arg(long)]
    pub log: bool,
    /// Arguments passed through to the selected tool.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum CaCmd {
    /// Print the path to the CA certificate.
    Path,
    /// Print the CA certificate PEM.
    Show,
}

const SUBCOMMANDS: &[&str] = &[
    "init", "run", "claude", "codex", "capture", "import", "ls", "show", "stats", "switch",
    "status", "trust", "untrust", "ca", "help",
];

/// Rewrite raw args (without the program name) so `rtr <tool> ...` becomes
/// `rtr run <tool> ...`. Leaves explicit subcommands and top-level flags
/// (`-h`, `--version`, …) untouched.
pub fn normalize_args(args: &[String]) -> Vec<String> {
    match args.first() {
        Some(first) if !first.starts_with('-') && !SUBCOMMANDS.contains(&first.as_str()) => {
            let mut out = Vec::with_capacity(args.len() + 1);
            out.push("run".to_string());
            out.extend_from_slice(args);
            out
        }
        _ => args.to_vec(),
    }
}

/// Parse from raw args (without program name), applying the bare-tool alias.
pub fn parse_from<I, S>(raw: I) -> Cli
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = raw.into_iter().map(Into::into).collect();
    let normalized = normalize_args(&args);
    Cli::parse_from(std::iter::once("rtr".to_string()).chain(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_tool_becomes_run() {
        assert_eq!(normalize_args(&v(&["curl"])), v(&["run", "curl"]));
        assert_eq!(
            normalize_args(&v(&["curl", "--model", "o3"])),
            v(&["run", "curl", "--model", "o3"])
        );
    }

    #[test]
    fn known_subcommands_untouched() {
        assert_eq!(normalize_args(&v(&["run", "codex"])), v(&["run", "codex"]));
        assert_eq!(
            normalize_args(&v(&["switch", "codex-2"])),
            v(&["switch", "codex-2"])
        );
        assert_eq!(normalize_args(&v(&["status"])), v(&["status"]));
    }

    #[test]
    fn leading_flag_untouched() {
        assert_eq!(normalize_args(&v(&["--help"])), v(&["--help"]));
        assert_eq!(normalize_args(&v(&["-V"])), v(&["-V"]));
    }

    #[test]
    fn parse_bare_tool_into_run() {
        let cli = parse_from(["curl"]);
        match cli.cmd {
            Cmd::Run { tool, args, .. } => {
                assert_eq!(tool, "curl");
                assert!(args.is_empty());
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_subscription_runtime_commands() {
        match parse_from([
            "claude",
            "--profile",
            "work",
            "--show-secrets",
            "--log",
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
                assert!(args.show_secrets);
                assert!(args.log);
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
        match parse_from(["codex", "-p", "personal"]).cmd {
            Cmd::Codex(args) => {
                assert_eq!(args.profile.as_deref(), Some("personal"));
                assert!(args.args.is_empty());
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
    fn parse_capture_import_show_and_stats() {
        match parse_from(["capture", "claude", "--profile", "work"]).cmd {
            Cmd::Capture { tool, profile } => {
                assert_eq!(tool, "claude");
                assert_eq!(profile, "work");
            }
            other => panic!("expected Capture, got {other:?}"),
        }
        match parse_from([
            "import",
            "codex",
            "--profile",
            "personal",
            "--from-capture",
            "/tmp/capture.jsonl",
            "--force",
            "--show-secrets",
        ])
        .cmd
        {
            Cmd::Import {
                tool,
                profile,
                from_capture,
                force,
                show_secrets,
                ..
            } => {
                assert_eq!(tool, "codex");
                assert_eq!(profile, "personal");
                assert_eq!(from_capture, PathBuf::from("/tmp/capture.jsonl"));
                assert!(force);
                assert!(show_secrets);
            }
            other => panic!("expected Import, got {other:?}"),
        }
        assert!(matches!(parse_from(["ls"]).cmd, Cmd::Ls));
        assert!(matches!(
            parse_from(["stats", "--today"]).cmd,
            Cmd::Stats { today: true }
        ));
        match parse_from(["show", "claude/work", "--show-secrets"]).cmd {
            Cmd::Show {
                target,
                show_secrets,
            } => {
                assert_eq!(target, "claude/work");
                assert!(show_secrets);
            }
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn parse_run_with_passthrough_args() {
        let cli = parse_from(["run", "codex", "--", "--login"]);
        match cli.cmd {
            Cmd::Run { tool, args, .. } => {
                assert_eq!(tool, "codex");
                assert_eq!(args, v(&["--login"]));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_switch_one_and_two_args() {
        match parse_from(["switch", "codex-2"]).cmd {
            Cmd::Switch { first, second } => {
                assert_eq!(first, "codex-2");
                assert_eq!(second, None);
            }
            other => panic!("expected Switch, got {other:?}"),
        }
        match parse_from(["switch", "codex", "codex-2"]).cmd {
            Cmd::Switch { first, second } => {
                assert_eq!(first, "codex");
                assert_eq!(second, Some("codex-2".to_string()));
            }
            other => panic!("expected Switch, got {other:?}"),
        }
    }

    #[test]
    fn parse_ca_and_trust() {
        assert!(matches!(
            parse_from(["ca", "path"]).cmd,
            Cmd::Ca { cmd: CaCmd::Path }
        ));
        assert!(matches!(
            parse_from(["ca", "show"]).cmd,
            Cmd::Ca { cmd: CaCmd::Show }
        ));
        assert!(matches!(
            parse_from(["trust", "--system"]).cmd,
            Cmd::Trust { system: true }
        ));
        assert!(matches!(
            parse_from(["untrust"]).cmd,
            Cmd::Untrust { system: false }
        ));
    }
}
