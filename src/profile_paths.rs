//! Read-only discovery of rtr-managed native profile homes.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::Config;
use crate::paths::Paths;
use crate::tool_specs;

pub const PROFILE_PATHS_VERSION: u32 = 1;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ProfilePathRecord {
    pub tool: String,
    pub profile: String,
    pub home_env: String,
    pub home: String,
    pub enabled: bool,
    pub bypass: bool,
    pub exists: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ProfilePaths {
    pub version: u32,
    pub profiles: Vec<ProfilePathRecord>,
}

/// Resolve every configured profile home without preparing or inspecting it.
pub fn discover(paths: &Paths) -> Result<ProfilePaths> {
    let config = Config::load(&paths.config_file())?;
    let mut profiles = Vec::new();

    for spec in tool_specs::all() {
        let Some(tool) = config.tools.get(spec.name) else {
            continue;
        };
        for (profile_name, profile) in &tool.profiles {
            let resolved_home = paths.profile_home_dir(spec.name, profile_name);
            let home = resolved_home
                .to_str()
                .with_context(|| {
                    format!(
                        "resolved home for {}/{} is not valid UTF-8",
                        spec.name, profile_name
                    )
                })?
                .to_string();
            let exists = resolved_home.try_exists().with_context(|| {
                format!(
                    "checking resolved home for {}/{} at {}",
                    spec.name,
                    profile_name,
                    resolved_home.display()
                )
            })?;
            profiles.push(ProfilePathRecord {
                tool: spec.name.to_string(),
                profile: profile_name.clone(),
                home_env: spec.native_home_env.to_string(),
                home,
                enabled: profile.enabled,
                bypass: profile.bypass,
                exists,
            });
        }
    }

    Ok(ProfilePaths {
        version: PROFILE_PATHS_VERSION,
        profiles,
    })
}

pub fn render_human(inventory: &ProfilePaths) -> String {
    if inventory.profiles.is_empty() {
        return "No configured profiles.\n".to_string();
    }

    let mut output = String::new();
    for record in &inventory.profiles {
        let _ = writeln!(output, "{}/{}", record.tool, record.profile);
        let _ = writeln!(output, "  home: {}={}", record.home_env, record.home);
        let _ = writeln!(output, "  enabled: {}", record.enabled);
        let _ = writeln!(output, "  bypass: {}", record.bypass);
        let missing = if record.exists { "" } else { " (missing)" };
        let _ = writeln!(output, "  exists: {}{missing}", record.exists);
    }
    output
}

pub fn render_json(inventory: &ProfilePaths) -> Result<String> {
    serde_json::to_string_pretty(inventory).context("serializing profile paths")
}

pub fn run(paths: &Paths, json: bool) -> Result<()> {
    let inventory = discover(paths)?;
    if json {
        println!("{}", render_json(&inventory)?);
    } else {
        print!("{}", render_human(&inventory));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};

    use serde_json::Value;

    use super::*;
    use crate::paths::Paths;

    fn test_paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
        }
    }

    fn write_config(paths: &Paths, text: &str) {
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.config_file(), text).unwrap();
    }

    fn sample_config() -> &'static str {
        r#"
[tools.codex]
command = ["codex"]

[tools.codex.profiles.zeta]
enabled = false

[tools.codex.profiles."Work Team"]
bypass = true

[tools.claude]
command = ["claude"]

[tools.claude.profiles.work]
"#
    }

    #[test]
    fn discovery_includes_all_profiles_in_tool_and_profile_order() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        write_config(&paths, sample_config());
        std::fs::create_dir_all(paths.profile_home_dir("codex", "Work Team")).unwrap();

        let inventory = discover(&paths).unwrap();

        assert_eq!(inventory.version, 1);
        assert_eq!(
            inventory
                .profiles
                .iter()
                .map(|record| (record.tool.as_str(), record.profile.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("claude", "work"),
                ("codex", "Work Team"),
                ("codex", "zeta"),
            ]
        );
        assert_eq!(inventory.profiles[0].home_env, "CLAUDE_CONFIG_DIR");
        assert!(inventory.profiles[0].enabled);
        assert!(!inventory.profiles[0].bypass);
        assert!(!inventory.profiles[0].exists);

        let bypassed = &inventory.profiles[1];
        assert_eq!(bypassed.home_env, "CODEX_HOME");
        assert_eq!(
            bypassed.home,
            paths
                .profile_home_dir("codex", "Work Team")
                .to_str()
                .unwrap()
        );
        assert!(bypassed.enabled);
        assert!(bypassed.bypass);
        assert!(bypassed.exists);

        let disabled = &inventory.profiles[2];
        assert!(!disabled.enabled);
        assert!(!disabled.bypass);
        assert!(!disabled.exists);
    }

    #[test]
    fn discovery_uses_safe_resolved_paths_without_creating_missing_homes() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = r#"
[tools.codex]
command = ["codex"]
[tools.codex.profiles."../work profile"]
"#;
        write_config(&paths, config);
        let original_config = std::fs::read(paths.config_file()).unwrap();
        assert!(!paths.state_dir.exists());

        let inventory = discover(&paths).unwrap();

        assert_eq!(
            inventory.profiles[0].home,
            paths
                .state_dir
                .join("homes/codex/..%2Fwork%20profile")
                .to_str()
                .unwrap()
        );
        assert!(!paths.state_dir.exists());
        assert_eq!(std::fs::read(paths.config_file()).unwrap(), original_config);
    }

    #[test]
    fn json_is_the_exact_v1_envelope_and_exposes_no_home_contents() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        write_config(
            &paths,
            r#"
[tools.codex]
command = ["codex", "--secret-command-arg"]
skills_source = "/private/skills"
[tools.codex.profiles.personal]
"#,
        );
        let home = paths.profile_home_dir("codex", "personal");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("auth.json"), "credential-secret").unwrap();
        std::fs::write(home.join("session.jsonl"), "session-secret").unwrap();

        let output = render_json(&discover(&paths).unwrap()).unwrap();
        let json: Value = serde_json::from_str(&output).unwrap();

        assert_eq!(json["version"], 1);
        assert_eq!(
            json.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["profiles", "version"])
        );
        let record = json["profiles"][0].as_object().unwrap();
        assert_eq!(
            record.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from(["tool", "profile", "home_env", "home", "enabled", "bypass", "exists"])
        );
        assert_eq!(record["tool"], "codex");
        assert_eq!(record["profile"], "personal");
        assert_eq!(record["home_env"], "CODEX_HOME");
        assert_eq!(record["home"], home.to_str().unwrap());
        assert_eq!(record["enabled"], true);
        assert_eq!(record["bypass"], false);
        assert_eq!(record["exists"], true);
        assert!(!output.contains("credential-secret"));
        assert!(!output.contains("session-secret"));
        assert!(!output.contains("--secret-command-arg"));
        assert!(!output.contains("/private/skills"));
    }

    #[test]
    fn human_output_identifies_assignment_flags_and_missing_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        write_config(
            &paths,
            r#"
[tools.claude]
command = ["claude"]
[tools.claude.profiles.work]
enabled = false
bypass = true
"#,
        );

        let output = render_human(&discover(&paths).unwrap());

        assert!(output.contains("claude/work"), "{output}");
        assert!(
            output.contains(&format!(
                "CLAUDE_CONFIG_DIR={}",
                paths.profile_home_dir("claude", "work").display()
            )),
            "{output}"
        );
        assert!(output.contains("enabled: false"), "{output}");
        assert!(output.contains("bypass: true"), "{output}");
        assert!(output.contains("exists: false (missing)"), "{output}");
    }

    #[test]
    fn non_utf8_resolved_home_returns_contextual_error() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff])),
        };
        write_config(
            &paths,
            r#"
[tools.codex]
command = ["codex"]
[tools.codex.profiles.personal]
"#,
        );

        let error = discover(&paths).unwrap_err().to_string();

        assert!(error.contains("codex/personal"), "{error}");
        assert!(error.contains("UTF-8"), "{error}");
    }
}
