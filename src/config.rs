//! `config.toml` model: proxy settings and per-tool definitions (command,
//! target hosts, header-rewrite profiles).
//!
//! The active profile per tool can be defaulted here (`active = "..."`) but the
//! live selection set by `rtr switch` lives in `state.toml` (see [`crate::state`])
//! so this file stays hand-editable and keeps its comments.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

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
    pub default_preset: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub presets: BTreeMap<String, Preset>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Preset {
    #[serde(default)]
    pub args: Vec<String>,
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

    /// Ensure a profile table exists without overwriting existing profile settings.
    pub fn ensure_profile_entry(&mut self, tool_name: &str, profile_name: &str) -> Result<bool> {
        let tool = self.tool_mut(tool_name)?;
        if tool.profiles.contains_key(profile_name) {
            return Ok(false);
        }
        tool.profiles
            .insert(profile_name.to_string(), Profile::default());
        Ok(true)
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

/// Add a missing first-class profile while preserving hand-written config comments.
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
        write_secret_file(path, &cfg.to_toml()?)?;
    }
    Ok(true)
}

fn toml_key_segment(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
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
        assert!(claude.profiles.is_empty());

        let codex = cfg.tool("codex").unwrap();
        assert_eq!(codex.command, vec!["codex".to_string()]);
        assert_eq!(codex.hosts, vec!["chatgpt.com".to_string()]);
        assert_eq!(codex.selection.as_deref(), Some("round-robin"));
        assert_eq!(codex.active.as_deref(), None);
        assert!(codex.profiles.is_empty());
    }

    #[test]
    fn ensure_profile_entry_in_file_preserves_comments_and_quotes_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_secret_file(
            &path,
            r#"# keep this comment
[tools.codex]
command = ["codex"]
"#,
        )
        .unwrap();

        let mut cfg = Config::load(&path).unwrap();
        let created = ensure_profile_entry_in_file(&path, &mut cfg, "codex", "work team").unwrap();
        assert!(created);
        assert!(cfg
            .tool("codex")
            .unwrap()
            .profiles
            .contains_key("work team"));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"), "{text}");
        assert!(
            text.contains("[tools.codex.profiles.\"work team\"]"),
            "{text}"
        );
        assert!(text.contains("enabled = true"), "{text}");
        assert!(Config::parse(&text)
            .unwrap()
            .tool("codex")
            .unwrap()
            .profiles
            .contains_key("work team"));

        let before = text.clone();
        let created_again =
            ensure_profile_entry_in_file(&path, &mut cfg, "codex", "work team").unwrap();
        assert!(!created_again);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn ensure_profile_entry_in_file_rewrites_when_append_would_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_secret_file(
            &path,
            r#"
[tools.codex]
command = ["codex"]
profiles = {}
"#,
        )
        .unwrap();

        let mut cfg = Config::load(&path).unwrap();
        let created = ensure_profile_entry_in_file(&path, &mut cfg, "codex", "personal").unwrap();
        assert!(created);

        let reparsed = Config::load(&path).unwrap();
        assert!(reparsed
            .tool("codex")
            .unwrap()
            .profiles
            .contains_key("personal"));
    }

    #[test]
    fn ensure_profile_entry_preserves_existing_settings() {
        let mut cfg = Config::parse(
            r#"
[tools.codex]
command = ["codex"]

[tools.codex.profiles.personal]
enabled = false
set = { Authorization = "Bearer old" }
"#,
        )
        .unwrap();

        let created = cfg.ensure_profile_entry("codex", "personal").unwrap();
        assert!(!created);
        let profile = cfg.tool("codex").unwrap().profiles.get("personal").unwrap();
        assert!(!profile.enabled);
        assert_eq!(
            profile.set.get("Authorization").map(String::as_str),
            Some("Bearer old")
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
        assert!(codex.presets.is_empty());
        assert_eq!(codex.default_preset, None);
        let profile = codex.profiles.get("work").unwrap();
        assert!(profile.enabled);
        assert!(profile.metadata.is_empty());
        assert_eq!(
            profile.set.get("Authorization").map(String::as_str),
            Some("Bearer old")
        );
    }

    #[test]
    fn presets_roundtrip_in_arg_order() {
        let cfg = Config::parse(
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
        .unwrap();
        let text = cfg.to_toml().unwrap();
        let reparsed = Config::parse(&text).unwrap();
        let preset = reparsed
            .tool("codex")
            .unwrap()
            .presets
            .get("xhigh")
            .unwrap();
        assert_eq!(
            preset.args,
            vec![
                "-m".to_string(),
                "gpt-5.5".to_string(),
                "-c".to_string(),
                "model_reasoning_effort=xhigh".to_string()
            ]
        );
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
