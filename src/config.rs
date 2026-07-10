//! Hand-editable launcher configuration for tools and native profiles.

use std::collections::BTreeMap;
use std::fmt::Write as _;
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
        let config: Self = toml::from_str(text).context("parsing config.toml")?;
        for name in config.tools.keys() {
            crate::tool_specs::get(name)
                .with_context(|| format!("invalid tool entry 'tools.{name}'"))?;
        }
        Ok(config)
    }

    pub fn tool(&self, name: &str) -> Result<&Tool> {
        self.tools
            .get(name)
            .with_context(|| format!("no tool named '{name}' in config.toml"))
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serializing config")
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

fn write_config_file(path: &Path, contents: &str) -> Result<()> {
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
    config: &mut Config,
    tool_name: &str,
    profile_name: &str,
) -> Result<bool> {
    if !config.ensure_profile_entry(tool_name, profile_name)? {
        return Ok(false);
    }

    let table = format!(
        "\n[tools.{}.profiles.{}]\nenabled = true\n",
        toml_key_segment(tool_name),
        toml_key_segment(profile_name)
    );
    let current =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let updated = format!("{current}{table}");
    if Config::parse(&updated).is_ok() {
        write_config_file(path, &updated)?;
    } else {
        write_config_file(path, &config.to_toml()?)?;
    }
    Ok(true)
}

/// Remove one profile while preserving unrelated hand-written TOML.
pub fn remove_profile_entry_in_file(
    path: &Path,
    config: &mut Config,
    tool_name: &str,
    profile_name: &str,
) -> Result<bool> {
    if !config.tool(tool_name)?.profiles.contains_key(profile_name) {
        return Ok(false);
    }

    let current =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut document = current
        .parse::<toml_edit::DocumentMut>()
        .context("parsing config.toml for profile removal")?;
    let removed = document
        .get_mut("tools")
        .and_then(|item| item.as_table_like_mut())
        .and_then(|tools| tools.get_mut(tool_name))
        .and_then(|item| item.as_table_like_mut())
        .and_then(|tool| tool.get_mut("profiles"))
        .and_then(|item| item.as_table_like_mut())
        .and_then(|profiles| profiles.remove(profile_name));
    if removed.is_none() {
        bail!("could not locate profile {tool_name}/{profile_name} in config.toml");
    }

    let updated = document.to_string();
    let updated_config = Config::parse(&updated)?;
    if updated_config
        .tool(tool_name)?
        .profiles
        .contains_key(profile_name)
    {
        bail!("profile {tool_name}/{profile_name} remained after config.toml removal");
    }
    write_config_file(path, &updated)?;
    *config = updated_config;
    Ok(true)
}

/// Flip one profile's enabled flag while preserving hand-written config comments.
pub fn set_profile_enabled_in_file(
    path: &Path,
    config: &mut Config,
    tool_name: &str,
    profile_name: &str,
    enabled: bool,
) -> Result<()> {
    config
        .tool_mut(tool_name)?
        .profiles
        .get_mut(profile_name)
        .with_context(|| format!("tool '{tool_name}' has no profile '{profile_name}'"))?
        .enabled = enabled;

    let current =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match edit_profile_enabled(&current, tool_name, profile_name, enabled) {
        Some(updated) if Config::parse(&updated).is_ok() => write_config_file(path, &updated),
        _ => write_config_file(path, &config.to_toml()?),
    }
}

fn edit_profile_enabled(
    text: &str,
    tool_name: &str,
    profile_name: &str,
    enabled: bool,
) -> Option<String> {
    let mut doc: toml_edit::DocumentMut = text.parse().ok()?;
    let profile = doc
        .get_mut("tools")
        .and_then(|tools| tools.as_table_like_mut()?.get_mut(tool_name))
        .and_then(|tool| tool.as_table_like_mut()?.get_mut("profiles"))
        .and_then(|profiles| profiles.as_table_like_mut()?.get_mut(profile_name))
        .and_then(|profile| profile.as_table_like_mut())?;
    // Mutate an existing value in place so comments on and above the line survive.
    match profile
        .get_mut("enabled")
        .and_then(|item| item.as_value_mut())
    {
        Some(value) => {
            let decor = value.decor().clone();
            *value = toml_edit::Value::from(enabled);
            *value.decor_mut() = decor;
        }
        None => {
            profile.insert("enabled", toml_edit::value(enabled));
        }
    }
    Some(doc.to_string())
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
            "[tools.curl]\ncommand = [\"curl\"]\n",
        ] {
            assert!(Config::parse(text).is_err(), "removed config parsed: {text}");
        }
    }

    #[test]
    fn tool_skills_source_parses() {
        let cfg = Config::parse(
            r#"
[tools.codex]
command = ["codex"]
skills_source = "~/.skills"

[tools.codex.profiles.personal]
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.tool("codex").unwrap().skills_source.as_deref(),
            Some(Path::new("~/.skills"))
        );
    }

    #[test]
    fn starter_config_is_private_and_requires_force_to_replace() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/config.toml");
        write_starter_config(&path, false).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_starter_config(&path, false).is_err());
        write_starter_config(&path, true).unwrap();
    }

    #[test]
    fn set_profile_enabled_flips_only_the_flag_and_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# pinned top comment\n\n[tools.codex]\ncommand = [\"codex\"] # inline note\n\n# work profile\n[tools.codex.profiles.work]\n# above the flag\nenabled = true # while rate-limited\n\n[tools.codex.profiles.other]\n";
        write_config_file(&path, original).unwrap();
        let mut config = Config::load(&path).unwrap();

        set_profile_enabled_in_file(&path, &mut config, "codex", "work", false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, original.replace("enabled = true", "enabled = false"));
        assert!(!config.tool("codex").unwrap().profiles["work"].enabled);
        let reloaded = Config::load(&path).unwrap();
        assert!(!reloaded.tool("codex").unwrap().profiles["work"].enabled);
        assert!(reloaded.tool("codex").unwrap().profiles["other"].enabled);
    }

    #[test]
    fn set_profile_enabled_round_trips_explicitly_for_quoted_and_bare_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config_file(
            &path,
            "# keep\n[tools.codex]\ncommand = [\"codex\"]\n\n[tools.codex.profiles.\"work team\"]\n",
        )
        .unwrap();
        let mut config = Config::load(&path).unwrap();

        set_profile_enabled_in_file(&path, &mut config, "codex", "work team", false).unwrap();
        let disabled = std::fs::read_to_string(&path).unwrap();
        assert!(disabled.contains("# keep"), "{disabled}");
        assert!(
            disabled.contains("[tools.codex.profiles.\"work team\"]\nenabled = false"),
            "{disabled}"
        );
        assert_eq!(disabled.matches("enabled").count(), 1, "{disabled}");

        set_profile_enabled_in_file(&path, &mut config, "codex", "work team", true).unwrap();
        let enabled = std::fs::read_to_string(&path).unwrap();
        assert!(enabled.contains("# keep"), "{enabled}");
        assert!(
            enabled.contains("[tools.codex.profiles.\"work team\"]\nenabled = true"),
            "{enabled}"
        );
        assert!(Config::load(&path).unwrap().tool("codex").unwrap().profiles["work team"].enabled);
    }

    #[test]
    fn set_profile_enabled_keeps_config_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config_file(
            &path,
            "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.work]\n",
        )
        .unwrap();
        let mut config = Config::load(&path).unwrap();

        set_profile_enabled_in_file(&path, &mut config, "codex", "work", false).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn set_profile_enabled_rejects_unknown_targets_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.work]\n";
        write_config_file(&path, original).unwrap();
        let mut config = Config::load(&path).unwrap();

        let tool_error = set_profile_enabled_in_file(&path, &mut config, "curl", "work", false)
            .unwrap_err()
            .to_string();
        assert!(tool_error.contains("no tool named 'curl'"), "{tool_error}");
        let profile_error =
            set_profile_enabled_in_file(&path, &mut config, "codex", "ghost", false)
                .unwrap_err()
                .to_string();
        assert!(
            profile_error.contains("tool 'codex' has no profile 'ghost'"),
            "{profile_error}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn profile_entry_preserves_comments_and_quotes_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config_file(
            &path,
            "# keep this comment\n[tools.codex]\ncommand = [\"codex\"]\n",
        )
        .unwrap();
        let mut config = Config::load(&path).unwrap();

        assert!(ensure_profile_entry_in_file(&path, &mut config, "codex", "work team").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"), "{text}");
        assert!(
            text.contains("[tools.codex.profiles.\"work team\"]"),
            "{text}"
        );
    }

    #[test]
    fn removed_profile_entry_preserves_comments_formatting_and_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = r#"# keep this header
[tools.codex]
command=["codex"] # keep spacing

# remove this profile
[tools.codex.profiles."work team"]
enabled = false

# keep this sibling comment
[tools.codex.profiles.personal]
enabled=true # keep inline
"#;
        write_config_file(&path, original).unwrap();
        let mut config = Config::load(&path).unwrap();

        assert!(remove_profile_entry_in_file(&path, &mut config, "codex", "work team").unwrap());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("work team"), "{text}");
        for preserved in [
            "# keep this header",
            "command=[\"codex\"] # keep spacing",
            "# keep this sibling comment",
            "[tools.codex.profiles.personal]",
            "enabled=true # keep inline",
        ] {
            assert!(
                text.contains(preserved),
                "missing {preserved:?} in:\n{text}"
            );
        }
        assert!(!config
            .tool("codex")
            .unwrap()
            .profiles
            .contains_key("work team"));
        assert!(config
            .tool("codex")
            .unwrap()
            .profiles
            .contains_key("personal"));
    }
}
