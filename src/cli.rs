//! Command-line surface for rtr.
//!
//! Besides the explicit subcommands, `rtr <tool>` is a convenience alias for
//! `rtr run <tool>`, and `rtr <tool> <profile>` is a one-shot profile override.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rtr",
    version,
    about = "Per-binary MITM proxy that captures and rewrites outbound auth headers"
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
        /// One-shot profile override for this run.
        #[arg(long)]
        profile: Option<String>,
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
    /// Capture a tool's auth header and import it into a config profile.
    Setup {
        /// Tool name as defined in config.toml.
        tool: String,
        /// Profile to update; defaults to <tool>-1.
        profile: Option<String>,
    },
    /// Set the active profile. `switch <tool> <profile>` or `switch <profile>`.
    Switch {
        first: String,
        second: Option<String>,
    },
    /// Show tools, active profiles, CA fingerprint, and trust state.
    Status {
        tool: Option<String>,
    },
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

#[derive(Subcommand, Debug)]
pub enum CaCmd {
    /// Print the path to the CA certificate.
    Path,
    /// Print the CA certificate PEM.
    Show,
}

const SUBCOMMANDS: &[&str] = &[
    "init", "run", "setup", "switch", "status", "trust", "untrust", "ca", "help",
];

/// Rewrite raw args so bare tool invocations route through `run`.
pub fn normalize_args(args: &[String]) -> Vec<String> {
    match args.first() {
        Some(first) if !first.starts_with('-') && !SUBCOMMANDS.contains(&first.as_str()) => {
            let mut out = Vec::with_capacity(args.len() + 1);
            out.push("run".to_string());
            if let Some(profile) = args.get(1).filter(|arg| !arg.starts_with('-')) {
                out.push("--profile".to_string());
                out.push(profile.clone());
                out.push(first.clone());
                out.extend_from_slice(&args[2..]);
            } else {
                out.extend_from_slice(args);
            }
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
        assert_eq!(normalize_args(&v(&["codex"])), v(&["run", "codex"]));
        assert_eq!(
            normalize_args(&v(&["codex", "--model", "o3"])),
            v(&["run", "codex", "--model", "o3"])
        );
        assert_eq!(normalize_args(&v(&["claude"])), v(&["run", "claude"]));
    }

    #[test]
    fn bare_tool_profile_becomes_run_profile_override() {
        assert_eq!(
            normalize_args(&v(&["claude", "claude-2"])),
            v(&["run", "--profile", "claude-2", "claude"])
        );
        assert_eq!(
            normalize_args(&v(&["claude", "claude-2", "--debug"])),
            v(&["run", "--profile", "claude-2", "claude", "--debug"])
        );
    }

    #[test]
    fn known_subcommands_untouched() {
        assert_eq!(normalize_args(&v(&["run", "codex"])), v(&["run", "codex"]));
        assert_eq!(normalize_args(&v(&["switch", "codex-2"])), v(&["switch", "codex-2"]));
        assert_eq!(normalize_args(&v(&["status"])), v(&["status"]));
    }

    #[test]
    fn leading_flag_untouched() {
        assert_eq!(normalize_args(&v(&["--help"])), v(&["--help"]));
        assert_eq!(normalize_args(&v(&["-V"])), v(&["-V"]));
    }

    #[test]
    fn parse_bare_tool_into_run() {
        let cli = parse_from(["codex"]);
        match cli.cmd {
            Cmd::Run {
                tool,
                profile,
                args,
                ..
            } => {
                assert_eq!(tool, "codex");
                assert_eq!(profile, None);
                assert!(args.is_empty());
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_bare_tool_profile_into_run_override() {
        let cli = parse_from(["claude", "claude-2"]);
        match cli.cmd {
            Cmd::Run {
                tool,
                profile,
                args,
                ..
            } => {
                assert_eq!(tool, "claude");
                assert_eq!(profile.as_deref(), Some("claude-2"));
                assert!(args.is_empty());
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_run_with_passthrough_args() {
        let cli = parse_from(["run", "codex", "--", "--login"]);
        match cli.cmd {
            Cmd::Run {
                tool,
                profile,
                args,
                ..
            } => {
                assert_eq!(tool, "codex");
                assert_eq!(profile, None);
                assert_eq!(args, v(&["--login"]));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_explicit_run_keeps_positional_as_child_arg() {
        let cli = parse_from(["run", "claude", "--", "claude-2"]);
        match cli.cmd {
            Cmd::Run {
                tool,
                profile,
                args,
                ..
            } => {
                assert_eq!(tool, "claude");
                assert_eq!(profile, None);
                assert_eq!(args, v(&["claude-2"]));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_setup() {
        match parse_from(["setup", "codex"]).cmd {
            Cmd::Setup { tool, profile } => {
                assert_eq!(tool, "codex");
                assert_eq!(profile, None);
            }
            other => panic!("expected Setup, got {other:?}"),
        }
        match parse_from(["setup", "claude", "claude-2"]).cmd {
            Cmd::Setup { tool, profile } => {
                assert_eq!(tool, "claude");
                assert_eq!(profile.as_deref(), Some("claude-2"));
            }
            other => panic!("expected Setup, got {other:?}"),
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
        assert!(matches!(parse_from(["ca", "path"]).cmd, Cmd::Ca { cmd: CaCmd::Path }));
        assert!(matches!(parse_from(["ca", "show"]).cmd, Cmd::Ca { cmd: CaCmd::Show }));
        assert!(matches!(parse_from(["trust", "--system"]).cmd, Cmd::Trust { system: true }));
        assert!(matches!(parse_from(["untrust"]).cmd, Cmd::Untrust { system: false }));
    }
}
