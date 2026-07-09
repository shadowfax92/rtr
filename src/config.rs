//! Hand-editable launcher configuration for tools and native profiles.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub tools: BTreeMap<String, Tool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_source: Option<PathBuf>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).context("parsing config.toml")
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serializing config")
    }

    pub fn tool(&self, name: &str) -> Result<&Tool> {
        self.tools
            .get(name)
            .with_context(|| format!("no tool named '{name}' in config.toml"))
    }

    pub fn tool_mut(&mut self, name: &str) -> Result<&mut Tool> {
        self.tools
            .get_mut(name)
            .with_context(|| format!("no tool named '{name}' in config.toml"))
    }

    pub fn ensure_profile_entry(&mut self, tool_name: &str, profile_name: &str) -> Result<bool> {
        let tool = self.tool_mut(tool_name)?;
        if tool.profiles.contains_key(profile_name) {
            return Ok(false);
        }
        tool.profiles
            .insert(profile_name.to_string(), Profile::default());
        Ok(true)
    }
}

pub const STARTER_CONFIG: &str = r#"# rtr configuration

[tools.claude]
command = ["claude"]

[tools.codex]
command = ["codex"]
"#;

/// Write launcher configuration atomically with owner-only permissions.
pub fn write_config_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::create_private_dir_all(parent)?;
    }
    crate::file_lock::write_private_atomic(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))?;
    Ok(())
}

/// Add a missing profile while preserving hand-written config comments.
pub fn ensure_profile_entry_in_file(
    path: &Path,
    cfg: &mut Config,
    tool_name: &str,
    profile_name: &str,
) -> Result<bool> {
    if !cfg.ensure_profile_entry(tool_name, profile_name)? {
        return Ok(false);
    }

    let table = format!(
        "\n[tools.{}.profiles.{}]\nenabled = true\n",
        toml_key_segment(tool_name),
        toml_key_segment(profile_name)
    );
    let current =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if Config::parse(&format!("{current}{table}")).is_ok() {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("opening {} for profile append", path.display()))?;
        file.write_all(table.as_bytes()).with_context(|| {
            format!(
                "appending profile {tool_name}/{profile_name} to {}",
                path.display()
            )
        })?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    } else {
        write_config_file(path, &cfg.to_toml()?)?;
    }
    Ok(true)
}

fn toml_key_segment(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return value.to_string();
    }

    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\u{08}' => escaped.push_str("\\b"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\u{0c}' => escaped.push_str("\\f"),
            '\r' => escaped.push_str("\\r"),
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            c if c <= '\u{1f}' || c == '\u{7f}' => {
                let _ = write!(escaped, "\\u{:04X}", c as u32);
            }
            c => escaped.push(c),
        }
    }
    format!("\"{escaped}\"")
}

/// Scaffold a starter config, refusing to clobber it unless forced.
pub fn write_starter_config(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "config already exists at {} (use `rtr init --force` to overwrite)",
            path.display()
        );
    }
    write_config_file(path, STARTER_CONFIG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_config_contains_only_native_profile_tools() {
        let cfg = Config::parse(STARTER_CONFIG).unwrap();
        let claude = cfg.tool("claude").unwrap();
        assert_eq!(claude.command, vec!["claude"]);
        assert!(claude.profiles.is_empty());

        let codex = cfg.tool("codex").unwrap();
        assert_eq!(codex.command, vec!["codex"]);
        assert!(codex.profiles.is_empty());
    }

    #[test]
    fn removed_proxy_config_is_rejected() {
        for text in [
            "[proxy]\nport = 62888\n",
            "[tools.codex]\ncommand = [\"codex\"]\nhosts = [\"chatgpt.com\"]\n",
            "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.work]\nset = { Authorization = \"Bearer old\" }\n",
            "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.work]\nmetadata = { account = \"old\" }\n",
        ] {
            assert!(Config::parse(text).is_err(), "removed config parsed: {text}");
        }
    }

    #[test]
    fn tool_skills_source_parses_and_roundtrips() {
        let cfg = Config::parse(
            r#"
[tools.codex]
command = ["codex"]
skills_source = "~/.skills"

[tools.codex.profiles.personal]
"#,
        )
        .unwrap();
        let reparsed = Config::parse(&cfg.to_toml().unwrap()).unwrap();
        assert_eq!(
            reparsed.tool("codex").unwrap().skills_source.as_deref(),
            Some(Path::new("~/.skills"))
        );
    }

    #[test]
    fn ensure_profile_entry_in_file_preserves_comments_and_quotes_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config_file(
            &path,
            "# keep this comment\n[tools.codex]\ncommand = [\"codex\"]\n",
        )
        .unwrap();

        let mut cfg = Config::load(&path).unwrap();
        assert!(ensure_profile_entry_in_file(&path, &mut cfg, "codex", "work team").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"), "{text}");
        assert!(
            text.contains("[tools.codex.profiles.\"work team\"]"),
            "{text}"
        );
        assert!(Config::load(&path)
            .unwrap()
            .tool("codex")
            .unwrap()
            .profiles
            .contains_key("work team"));
    }

    #[test]
    fn ensure_profile_entry_preserves_existing_settings() {
        let mut cfg = Config::parse(
            "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.personal]\nenabled = false\n",
        )
        .unwrap();
        assert!(!cfg.ensure_profile_entry("codex", "personal").unwrap());
        assert!(
            !cfg.tool("codex")
                .unwrap()
                .profiles
                .get("personal")
                .unwrap()
                .enabled
        );
    }
}
