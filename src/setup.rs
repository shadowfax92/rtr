use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::paths::Paths;
use crate::runner::{self, ProfileSelection};
use crate::{ca, capture, config, state::State};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOutcome {
    pub tool: String,
    pub profile: String,
    pub capture_path: PathBuf,
}

/// Default profile name used when `rtr setup <tool>` omits one.
pub fn default_profile(tool: &str) -> String {
    format!("{tool}-1")
}

fn validate_tool(tool: &str) -> Result<()> {
    match tool {
        "codex" | "claude" => Ok(()),
        _ => bail!("setup only has built-in defaults for codex and claude"),
    }
}

/// Import a setup run's captured auth header and make the profile active.
pub fn finish_setup_import(
    paths: &Paths,
    tool: &str,
    profile: &str,
    capture_path: PathBuf,
) -> Result<SetupOutcome> {
    validate_tool(tool)?;
    let authorization = capture::last_authorization_header(&capture_path)
        .with_context(|| format!("setup for '{tool}' did not capture an authorization header"))?;
    config::import_authorization_header(&paths.config_file(), tool, profile, &authorization)?;

    let state_path = paths.state_file();
    let mut st = State::load(&state_path)?;
    st.set_active(tool, profile);
    st.save(&state_path)?;

    Ok(SetupOutcome {
        tool: tool.to_string(),
        profile: profile.to_string(),
        capture_path,
    })
}

fn wait_for_enter(tool: &str, profile: &str) -> Result<()> {
    println!("rtr setup {tool} -> {profile}");
    println!("Authenticate inside {tool}, make one request if needed, then exit the CLI.");
    print!("Press Enter to launch {tool} through rtr...");
    io::stdout().flush().context("flushing setup prompt")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("reading setup confirmation")?;
    Ok(())
}

/// Run guided setup for a built-in tool and import the captured auth header.
pub async fn setup_tool(
    paths: &Paths,
    tool: &str,
    profile: Option<String>,
) -> Result<SetupOutcome> {
    validate_tool(tool)?;
    let profile = profile.unwrap_or_else(|| default_profile(tool));
    config::ensure_default_tools(&paths.config_file())?;
    let ca = ca::load_or_generate(&paths.ca_cert(), &paths.ca_key())?;
    println!("CA ready at {}", ca.cert_path.display());

    wait_for_enter(tool, &profile)?;
    let run = runner::run_tool(paths, tool, &[], ProfileSelection::None, false, false).await?;
    finish_setup_import(paths, tool, &profile, run.capture_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CaptureRecord;
    use crate::config::Config;
    use crate::paths::Paths;
    use crate::state::State;

    #[test]
    fn default_profile_uses_tool_dash_one() {
        assert_eq!(default_profile("codex"), "codex-1");
        assert_eq!(default_profile("claude"), "claude-1");
    }

    #[test]
    fn finish_setup_import_updates_config_and_state() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            state_dir: dir.path().join("state"),
        };
        let capture_path = dir.path().join("capture.jsonl");
        let record = CaptureRecord {
            ts: "2026-06-11T10:55:00Z".to_string(),
            method: "POST".to_string(),
            url: "https://api.anthropic.com/v1/messages".to_string(),
            host: "api.anthropic.com".to_string(),
            headers: vec![(
                "authorization".to_string(),
                "Bearer claude-token".to_string(),
            )],
        };
        std::fs::write(&capture_path, serde_json::to_string(&record).unwrap()).unwrap();

        let outcome =
            finish_setup_import(&paths, "claude", "claude-2", capture_path.clone()).unwrap();
        assert_eq!(outcome.tool, "claude");
        assert_eq!(outcome.profile, "claude-2");
        assert_eq!(outcome.capture_path, capture_path);

        let cfg = Config::load(&paths.config_file()).unwrap();
        let auth = cfg
            .tool("claude")
            .unwrap()
            .profiles
            .get("claude-2")
            .unwrap()
            .set
            .get("Authorization")
            .map(String::as_str);
        assert_eq!(auth, Some("Bearer claude-token"));

        let st = State::load(&paths.state_file()).unwrap();
        assert_eq!(
            st.active.get("claude").map(String::as_str),
            Some("claude-2")
        );
    }
}
