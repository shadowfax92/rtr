//! `config.toml` model: proxy settings and per-tool definitions (command,
//! target hosts, header-rewrite profiles).
//!
//! The active profile per tool can be defaulted here (`active = "..."`) but the
//! live selection set by `rtr switch` lives in `state.toml` (see [`crate::state`])
//! so this file stays hand-editable and keeps its comments.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use toml_edit::{value, DocumentMut, Item, Table, Value};

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
    /// Program plus base args, e.g. `["codex"]`. User args are appended at run.
    pub command: Vec<String>,
    /// Hostnames whose traffic is intercepted and eligible for rewrite. An entry
    /// is an exact host, a dot-suffix (`.example.com`, apex + subdomains), or a
    /// bare `*` for every host (`*.example.com` is not a glob — use the dot
    /// form). Omitting `hosts` defaults to `*` (intercept all).
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Default active profile (overridden by state.toml).
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// A set of header mutations applied to intercepted requests for a tool.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Profile {
    /// Header name -> value to set (overwrites if present, adds if absent).
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Header names to remove.
    #[serde(default)]
    pub remove: Vec<String>,
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

/// Starter config written by `rtr init`. Hand-authored (not serialized) so it
/// keeps explanatory comments.
pub const STARTER_CONFIG: &str = r#"# rtr configuration
# Secrets live here in plaintext — this file is created with 0600 perms.

[proxy]
# Local port the MITM proxy binds on (127.0.0.1 only).
port = 62888

# A tool rtr can launch with interception: `rtr codex` or `rtr run codex`.
[tools.codex]
command = ["codex"]
# Only traffic to these hosts is intercepted; everything else tunnels untouched.
# Use ["*"] — or omit `hosts` entirely — to intercept ALL of the tool's traffic.
hosts = ["api.openai.com", "chatgpt.com"]
# Which profile is active by default (override live with `rtr switch`).
active = "codex-1"

# `rtr setup codex [profile]` imports the captured Authorization header here.
[tools.codex.profiles.codex-1]
set = { }
remove = []

[tools.codex.profiles.codex-2]
set = { }
remove = []

# Claude Code profile switching: `rtr setup claude`, then `rtr claude`.
[tools.claude]
command = ["claude"]
hosts = ["api.anthropic.com"]
active = "claude-1"

[tools.claude.profiles.claude-1]
set = { }
remove = []

[tools.claude.profiles.claude-2]
set = { }
remove = []
"#;

const CODEX_DEFAULT: &str = r#"[tools.codex]
command = ["codex"]
hosts = ["api.openai.com", "chatgpt.com"]
active = "codex-1"

[tools.codex.profiles.codex-1]
set = { }
remove = []

[tools.codex.profiles.codex-2]
set = { }
remove = []
"#;

const CLAUDE_DEFAULT: &str = r#"[tools.claude]
command = ["claude"]
hosts = ["api.anthropic.com"]
active = "claude-1"

[tools.claude.profiles.claude-1]
set = { }
remove = []

[tools.claude.profiles.claude-2]
set = { }
remove = []
"#;

/// Write `contents` to `path`, then tighten perms to `0600` (it holds secrets).
pub fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        crate::paths::create_private_dir_all(parent)?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
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

fn parse_document(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .context("parsing config.toml for edit")
}

fn table_mut<'a>(item: &'a mut Item, label: &str) -> Result<&'a mut Table> {
    item.or_insert(toml_edit::table())
        .as_table_mut()
        .with_context(|| format!("{label} must be a table"))
}

fn default_tool_item(tool: &str) -> Result<Item> {
    let text = match tool {
        "codex" => CODEX_DEFAULT,
        "claude" => CLAUDE_DEFAULT,
        _ => bail!("no built-in defaults for tool '{tool}'"),
    };
    let doc = parse_document(text)?;
    Ok(doc["tools"][tool].clone())
}

fn ensure_tool(doc: &mut DocumentMut, tool: &str) -> Result<()> {
    let tools = table_mut(&mut doc["tools"], "tools")?;
    if tools.get(tool).is_none() {
        tools[tool] = default_tool_item(tool)?;
    }
    Ok(())
}

fn write_document(path: &Path, doc: &DocumentMut) -> Result<()> {
    write_secret_file(path, &doc.to_string())
}

/// Ensure config exists and includes built-in Codex and Claude tool entries.
pub fn ensure_default_tools(path: &Path) -> Result<()> {
    if !path.exists() {
        return write_starter_config(path, false);
    }

    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let mut doc = parse_document(&text)?;
    ensure_tool(&mut doc, "codex")?;
    ensure_tool(&mut doc, "claude")?;
    write_document(path, &doc)
}

fn set_authorization(profile: &mut Table, authorization: &str) -> Result<()> {
    match &mut profile["set"] {
        Item::None => {
            let mut set = Table::new();
            set["Authorization"] = value(authorization);
            profile["set"] = Item::Table(set);
        }
        Item::Table(set) => {
            set["Authorization"] = value(authorization);
        }
        Item::Value(Value::InlineTable(set)) => {
            set.insert("Authorization", Value::from(authorization));
            set.fmt();
        }
        other => bail!("profile set must be a table, got {}", other.type_name()),
    }
    Ok(())
}

/// Import a captured Authorization header into a tool profile in config.toml.
pub fn import_authorization_header(
    path: &Path,
    tool: &str,
    profile: &str,
    authorization: &str,
) -> Result<()> {
    ensure_default_tools(path)?;
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let mut doc = parse_document(&text)?;
    ensure_tool(&mut doc, tool)?;

    let tools = table_mut(&mut doc["tools"], "tools")?;
    let tool_table = table_mut(&mut tools[tool], &format!("tools.{tool}"))?;
    let profiles = table_mut(
        &mut tool_table["profiles"],
        &format!("tools.{tool}.profiles"),
    )?;
    let profile_table = table_mut(
        &mut profiles[profile],
        &format!("tools.{tool}.profiles.{profile}"),
    )?;
    set_authorization(profile_table, authorization)?;
    write_document(path, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_config_parses_with_codex_example() {
        let cfg = Config::parse(STARTER_CONFIG).unwrap();
        assert_eq!(cfg.proxy.port, 62888);
        let codex = cfg.tool("codex").unwrap();
        assert_eq!(codex.command, vec!["codex".to_string()]);
        assert_eq!(
            codex.hosts,
            vec!["api.openai.com".to_string(), "chatgpt.com".to_string()]
        );
        assert_eq!(codex.active.as_deref(), Some("codex-1"));
        assert!(codex.profiles.contains_key("codex-1"));
        assert!(codex.profiles.contains_key("codex-2"));

        let claude = cfg.tool("claude").unwrap();
        assert_eq!(claude.command, vec!["claude".to_string()]);
        assert_eq!(claude.hosts, vec!["api.anthropic.com".to_string()]);
        assert_eq!(claude.active.as_deref(), Some("claude-1"));
        assert!(claude.profiles.contains_key("claude-1"));
        assert!(claude.profiles.contains_key("claude-2"));
    }

    #[test]
    fn roundtrips_losslessly() {
        let original = Config::parse(STARTER_CONFIG).unwrap();
        let mut codex = original.tools.get("codex").cloned().unwrap();
        codex
            .profiles
            .get_mut("codex-1")
            .unwrap()
            .set
            .insert("Authorization".to_string(), "Bearer abc".to_string());
        let mut cfg = original.clone();
        cfg.tools.insert("codex".to_string(), codex);

        let text = cfg.to_toml().unwrap();
        let reparsed = Config::parse(&text).unwrap();
        assert_eq!(
            reparsed
                .tool("codex")
                .unwrap()
                .profiles
                .get("codex-1")
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
        let cfg = Config::parse(STARTER_CONFIG).unwrap();
        assert_eq!(
            cfg.resolve_switch("codex", Some("codex-2")).unwrap(),
            ("codex".to_string(), "codex-2".to_string())
        );
        assert!(cfg.resolve_switch("codex", Some("nope")).is_err());
        assert!(cfg.resolve_switch("nope", Some("codex-1")).is_err());
    }

    #[test]
    fn resolve_switch_unique_single_token() {
        let cfg = Config::parse(STARTER_CONFIG).unwrap();
        assert_eq!(
            cfg.resolve_switch("codex-2", None).unwrap(),
            ("codex".to_string(), "codex-2".to_string())
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
        // Parses back into a valid config.
        Config::load(&path).unwrap().tool("codex").unwrap();

        // Refuses to overwrite without force, allows with force.
        assert!(write_starter_config(&path, false).is_err());
        write_starter_config(&path, true).unwrap();
    }

    #[test]
    fn ensure_default_tools_creates_missing_config_and_adds_missing_tools() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        ensure_default_tools(&path).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.tools.contains_key("codex"));
        assert!(cfg.tools.contains_key("claude"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        std::fs::write(
            &path,
            r#"# keep this comment
[proxy]
port = 0

[tools.custom]
command = ["custom"]
"#,
        )
        .unwrap();

        ensure_default_tools(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"), "{text}");
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.tools.contains_key("custom"));
        assert!(cfg.tools.contains_key("codex"));
        assert!(cfg.tools.contains_key("claude"));
    }

    #[test]
    fn import_authorization_header_updates_profile_and_preserves_comments() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"# user comment
[proxy]
port = 0

[tools.claude]
command = ["claude"]
hosts = ["api.anthropic.com"]
active = "claude-1"

[tools.claude.profiles.claude-1]
set = {}
remove = []
"#,
        )
        .unwrap();

        import_authorization_header(&path, "claude", "claude-2", "Bearer abc").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# user comment"), "{text}");
        let cfg = Config::load(&path).unwrap();
        let profile = cfg
            .tool("claude")
            .unwrap()
            .profiles
            .get("claude-2")
            .unwrap();
        assert_eq!(
            profile.set.get("Authorization").map(String::as_str),
            Some("Bearer abc")
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
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
