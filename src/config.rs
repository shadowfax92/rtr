//! `config.toml` model: proxy settings and per-tool definitions (command,
//! target hosts, skills source, header-rewrite profiles).
//!
//! The active profile per tool can be defaulted here (`active = "..."`) but the
//! live selection set by `rtr switch` lives in `state.toml` (see [`crate::state`])
//! so this file stays hand-editable and keeps its comments.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 62888;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub tools: BTreeMap<String, Tool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProxyConfig {
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
        }
    }
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Tool {
    pub command: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_source: Option<PathBuf>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Profile {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            set: BTreeMap::new(),
            remove: Vec::new(),
            enabled: true,
            metadata: BTreeMap::new(),
        }
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
        reject_removed_preset_config(text)?;
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

    /// Resolve a `switch` invocation to a concrete `(tool, profile)`.
    ///
    /// `switch <tool> <profile>` is explicit. `switch <profile>` is accepted
    /// only when the profile name is unique across all tools.
    pub fn resolve_switch(&self, first: &str, second: Option<&str>) -> Result<(String, String)> {
        if let Some(profile) = second {
            let tool = self.tool(first)?;
            if !tool.profiles.contains_key(profile) {
                bail!("tool '{first}' has no profile '{profile}'");
            }
            return Ok((first.to_string(), profile.to_string()));
        }

        let owners: Vec<&String> = self
            .tools
            .iter()
            .filter(|(_, t)| t.profiles.contains_key(first))
            .map(|(name, _)| name)
            .collect();
        match owners.as_slice() {
            [] => bail!("no profile named '{first}' in any tool"),
            [only] => Ok(((*only).clone(), first.to_string())),
            many => {
                let names: Vec<&str> = many.iter().map(|s| s.as_str()).collect();
                bail!(
                    "profile '{first}' exists in multiple tools ({}); use `rtr switch <tool> <profile>`",
                    names.join(", ")
                )
            }
        }
    }
}

fn reject_removed_preset_config(text: &str) -> Result<()> {
    let value: toml::Value = toml::from_str(text).context("parsing config.toml")?;
    let Some(tools) = value.get("tools").and_then(toml::Value::as_table) else {
        return Ok(());
    };

    let mut removed = Vec::new();
    for (tool_name, tool_value) in tools {
        let Some(tool) = tool_value.as_table() else {
            continue;
        };
        if tool.contains_key("default_preset") {
            removed.push(format!("tools.{tool_name}.default_preset"));
        }
        if tool.contains_key("presets") {
            removed.push(format!("tools.{tool_name}.presets"));
        }
    }

    if !removed.is_empty() {
        bail!(
            "preset config was removed; delete {} and pass runtime args directly to `rtr claude` or `rtr codex`",
            removed.join(", ")
        );
    }
    Ok(())
}

pub const STARTER_CONFIG: &str = r#"# rtr configuration
# Secrets live here in plaintext — this file is created with 0600 perms.

[proxy]
# Local port the MITM proxy binds on (127.0.0.1 only).
port = 62888

[tools.claude]
command = ["claude"]
hosts = [".anthropic.com"]
selection = "round-robin"

[tools.codex]
command = ["codex"]
hosts = ["chatgpt.com"]
selection = "round-robin"
"#;

/// Write `contents` to `path`, then tighten perms to `0600` (it holds secrets).
pub fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        crate::paths::create_private_dir_all(parent)?;
    }
    crate::file_lock::write_private_atomic(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))?;
    Ok(())
}

/// Scaffold a starter config, refusing to clobber an existing one unless forced.
pub fn write_starter_config(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "config already exists at {} (use `rtr init --force` to overwrite)",
            path.display()
        );
    }
    write_secret_file(path, STARTER_CONFIG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_config_parses_with_subscription_tools() {
        let cfg = Config::parse(STARTER_CONFIG).unwrap();
        assert_eq!(cfg.proxy.port, 62888);
        let claude = cfg.tool("claude").unwrap();
        assert_eq!(claude.command, vec!["claude".to_string()]);
        assert_eq!(claude.hosts, vec![".anthropic.com".to_string()]);
        assert_eq!(claude.selection.as_deref(), Some("round-robin"));
        assert_eq!(claude.skills_source, None);
        assert!(claude.profiles.is_empty());

        let codex = cfg.tool("codex").unwrap();
        assert_eq!(codex.command, vec!["codex".to_string()]);
        assert_eq!(codex.hosts, vec!["chatgpt.com".to_string()]);
        assert_eq!(codex.selection.as_deref(), Some("round-robin"));
        assert_eq!(codex.skills_source, None);
        assert_eq!(codex.active.as_deref(), None);
        assert!(codex.profiles.is_empty());
    }

    #[test]
    fn tool_skills_source_parses_and_roundtrips() {
        let cfg = Config::parse(
            r#"
[tools.codex]
command = ["codex"]
skills_source = "~/.skills"

[tools.codex.profiles.personal]
set = {}
"#,
        )
        .unwrap();
        let text = cfg.to_toml().unwrap();
        let reparsed = Config::parse(&text).unwrap();
        assert_eq!(
            reparsed.tool("codex").unwrap().skills_source.as_deref(),
            Some(Path::new("~/.skills"))
        );
    }

    #[test]
    fn existing_configs_parse_with_new_defaults() {
        let cfg = Config::parse(
            r#"
[tools.codex]
command = ["codex"]
hosts = ["chatgpt.com"]
active = "work"

[tools.codex.profiles.work]
set = { Authorization = "Bearer old" }
remove = []
"#,
        )
        .unwrap();
        let codex = cfg.tool("codex").unwrap();
        let profile = codex.profiles.get("work").unwrap();
        assert!(profile.enabled);
        assert!(profile.metadata.is_empty());
        assert_eq!(
            profile.set.get("Authorization").map(String::as_str),
            Some("Bearer old")
        );
    }

    #[test]
    fn removed_preset_config_errors_clearly() {
        let err = Config::parse(
            r#"
[tools.codex]
command = ["codex"]
default_preset = "xhigh"

[tools.codex.presets.xhigh]
args = ["-m", "gpt-5.5", "-c", "model_reasoning_effort=xhigh"]

[tools.codex.profiles.personal]
set = {}
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("preset config was removed"), "got: {err}");
        assert!(err.contains("tools.codex.default_preset"), "got: {err}");
        assert!(err.contains("tools.codex.presets"), "got: {err}");
    }

    #[test]
    fn roundtrips_losslessly() {
        let original = Config::parse(STARTER_CONFIG).unwrap();
        let mut codex = original.tools.get("codex").cloned().unwrap();
        codex.profiles.insert(
            "personal".to_string(),
            Profile {
                set: [("Authorization".to_string(), "Bearer abc".to_string())]
                    .into_iter()
                    .collect(),
                ..Profile::default()
            },
        );
        let mut cfg = original.clone();
        cfg.tools.insert("codex".to_string(), codex);

        let text = cfg.to_toml().unwrap();
        let reparsed = Config::parse(&text).unwrap();
        assert_eq!(
            reparsed
                .tool("codex")
                .unwrap()
                .profiles
                .get("personal")
                .unwrap()
                .set
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer abc")
        );
        assert_eq!(reparsed.proxy.port, cfg.proxy.port);
    }

    #[test]
    fn default_port_when_proxy_section_absent() {
        let cfg = Config::parse("[tools.x]\ncommand=[\"x\"]\n").unwrap();
        assert_eq!(cfg.proxy.port, DEFAULT_PORT);
    }

    #[test]
    fn resolve_switch_two_args() {
        let cfg = Config::parse(
            r#"
[tools.codex]
command = ["codex"]

[tools.codex.profiles.personal]
set = {}
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_switch("codex", Some("personal")).unwrap(),
            ("codex".to_string(), "personal".to_string())
        );
        assert!(cfg.resolve_switch("codex", Some("nope")).is_err());
        assert!(cfg.resolve_switch("nope", Some("personal")).is_err());
    }

    #[test]
    fn resolve_switch_unique_single_token() {
        let cfg = Config::parse(
            r#"
[tools.claude]
command = ["claude"]

[tools.claude.profiles.work]
set = {}
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_switch("work", None).unwrap(),
            ("claude".to_string(), "work".to_string())
        );
        assert!(cfg.resolve_switch("missing", None).is_err());
    }

    #[test]
    fn write_starter_config_is_0600_and_refuses_clobber() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");

        write_starter_config(&path, false).unwrap();
        assert!(path.exists());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
        Config::load(&path).unwrap().tool("codex").unwrap();

        assert!(write_starter_config(&path, false).is_err());
        write_starter_config(&path, true).unwrap();
    }

    #[test]
    fn resolve_switch_ambiguous_single_token_errors() {
        let text = r#"
[tools.a]
command = ["a"]
[tools.a.profiles.shared]
set = {}
[tools.b]
command = ["b"]
[tools.b.profiles.shared]
set = {}
"#;
        let cfg = Config::parse(text).unwrap();
        let err = cfg.resolve_switch("shared", None).unwrap_err().to_string();
        assert!(err.contains("multiple tools"), "got: {err}");
    }
}
