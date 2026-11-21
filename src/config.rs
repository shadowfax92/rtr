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
    #[serde(default)]
    pub routing: RoutingConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub claude: ClaudeRoutingConfig,
}

/// Fallback policy for automatic Claude routes. Existing configs inherit it.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeRoutingConfig {
    pub auto_drop: bool,
    pub fallback_model: String,
}

impl Default for ClaudeRoutingConfig {
    fn default() -> Self {
        Self {
            auto_drop: true,
            fallback_model: "claude-opus-5-5".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub command: Vec<String>,
    /// Native CLI defaults merged ahead of per-invocation arguments.
    ///
    /// Keeping these separate from `command` preserves wrapper executables and
    /// lets an explicit runtime option replace its configured default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_source: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<Vec<CopyMapping>>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CopyMapping {
    pub source: PathBuf,
    pub destination: PathBuf,
}

/// Persistent launch policy for one native home. An absent share participates
/// in the equal remainder; zero share keeps the profile available for explicit use.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bypass: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_percent: Option<u8>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            enabled: true,
            bypass: false,
            share_percent: None,
        }
    }
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).context("parsing config.toml")?;
        if !is_opus_model(&config.routing.claude.fallback_model) {
            bail!("routing.claude.fallback_model must be a recognized Claude Opus model");
        }
        for (name, tool) in &config.tools {
            crate::tool_specs::get(name)
                .with_context(|| format!("invalid tool entry 'tools.{name}'"))?;
            if tool.skills_source.is_some() && tool.copy.is_some() {
                bail!(
                    "tool '{name}' cannot set both 'skills_source' and 'copy'; use 'copy' for all startup mappings"
                );
            }
            for (profile_name, profile) in &tool.profiles {
                if profile.share_percent.is_some_and(|percent| percent > 100) {
                    bail!(
                        "profile '{name}/{profile_name}' share_percent must be between 0 and 100"
                    );
                }
            }
        }
        // Aggregate shares are validated by selection and allocation edits.
        // Disabling/removing profiles can leave an incomplete allocation, and
        // inspection, explicit launches, and reset must remain available then.
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

fn is_opus_model(model: &str) -> bool {
    // A fallback must name a native Opus target. Applying the family rule
    // alone would also accept invented versions and retirements.
    let id = model.strip_suffix("[1m]").unwrap_or(model);
    match id {
        "opus" | "claude-opus-5-5" | "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7"
        | "claude-opus-4-6" => true,
        "claude-opus-4-5" | "claude-opus-4-5-20251101" => model == id,
        _ => false,
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

#[derive(Clone, Copy)]
enum ProfileFlag {
    Enabled,
    Bypass,
}

impl ProfileFlag {
    fn key(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Bypass => "bypass",
        }
    }

    fn set(self, profile: &mut Profile, value: bool) {
        match self {
            Self::Enabled => profile.enabled = value,
            Self::Bypass => profile.bypass = value,
        }
    }
}

/// Set one profile's enabled flag while preserving hand-written config comments.
pub fn set_profile_enabled_in_file(
    path: &Path,
    config: &mut Config,
    tool_name: &str,
    profile_name: &str,
    enabled: bool,
) -> Result<()> {
    set_profile_flag_in_file(
        path,
        config,
        tool_name,
        profile_name,
        ProfileFlag::Enabled,
        enabled,
    )
}

/// Set one profile's bypass flag while preserving hand-written config comments.
pub fn set_profile_bypass_in_file(
    path: &Path,
    config: &mut Config,
    tool_name: &str,
    profile_name: &str,
    bypass: bool,
) -> Result<()> {
    set_profile_flag_in_file(
        path,
        config,
        tool_name,
        profile_name,
        ProfileFlag::Bypass,
        bypass,
    )
}

fn set_profile_flag_in_file(
    path: &Path,
    config: &mut Config,
    tool_name: &str,
    profile_name: &str,
    flag: ProfileFlag,
    value: bool,
) -> Result<()> {
    let profile = config
        .tool_mut(tool_name)?
        .profiles
        .get_mut(profile_name)
        .with_context(|| format!("tool '{tool_name}' has no profile '{profile_name}'"))?;
    flag.set(profile, value);

    let current =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match edit_profile_flag(&current, tool_name, profile_name, flag.key(), value) {
        Some(updated) if Config::parse(&updated).is_ok() => write_config_file(path, &updated),
        _ => write_config_file(path, &config.to_toml()?),
    }
}

fn edit_profile_flag(
    text: &str,
    tool_name: &str,
    profile_name: &str,
    key: &str,
    value: bool,
) -> Option<String> {
    let mut doc: toml_edit::DocumentMut = text.parse().ok()?;
    let profile = doc
        .get_mut("tools")
        .and_then(|tools| tools.as_table_like_mut()?.get_mut(tool_name))
        .and_then(|tool| tool.as_table_like_mut()?.get_mut("profiles"))
        .and_then(|profiles| profiles.as_table_like_mut()?.get_mut(profile_name))
        .and_then(|profile| profile.as_table_like_mut())?;
    match profile.get_mut(key).and_then(|item| item.as_value_mut()) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = toml_edit::Value::from(value);
            *existing.decor_mut() = decor;
        }
        None => {
            profile.insert(key, toml_edit::value(value));
        }
    }
    Some(doc.to_string())
}

/// Edit percentage overrides as one transaction. The caller holds the config
/// lock across loading and writing, so concurrent profile edits cannot be lost.
/// None resets the tool, including disabled profiles; scheduler state is owned
/// by selection and reconciles this policy on the next automatic launch.
pub fn set_profile_shares_in_file(
    path: &Path,
    config: &mut Config,
    tool_name: &str,
    setting: Option<(&str, u8)>,
) -> Result<()> {
    let mut updated_config = config.clone();
    let tool = updated_config.tool_mut(tool_name)?;
    let names = if let Some((name, percent)) = setting {
        if percent > 100 {
            bail!("share must be a whole percentage between 0 and 100");
        }
        tool.profiles
            .get_mut(name)
            .with_context(|| format!("tool '{tool_name}' has no profile '{name}'"))?
            .share_percent = Some(percent);
        vec![name.to_string()]
    } else {
        for profile in tool.profiles.values_mut() {
            profile.share_percent = None;
        }
        tool.profiles.keys().cloned().collect()
    };
    crate::selection::allocation(tool_name, tool)?;

    let current =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut =
        current.parse().context("parsing config for share edits")?;
    for name in names {
        let profile = doc
            .get_mut("tools")
            .and_then(|tools| tools.as_table_like_mut()?.get_mut(tool_name))
            .and_then(|tool| tool.as_table_like_mut()?.get_mut("profiles"))
            .and_then(|profiles| profiles.as_table_like_mut()?.get_mut(&name))
            .and_then(|profile| profile.as_table_like_mut())
            .with_context(|| {
                format!("could not locate profile {tool_name}/{name} in config.toml")
            })?;
        if let Some((_, percent)) = setting {
            let mut value = toml_edit::Value::from(i64::from(percent));
            if let Some(existing) = profile
                .get("share_percent")
                .and_then(|item| item.as_value())
            {
                *value.decor_mut() = existing.decor().clone();
            }
            profile.insert("share_percent", toml_edit::Item::Value(value));
        } else {
            profile.remove("share_percent");
        }
    }
    let updated = doc.to_string();
    Config::parse(&updated)?;
    if updated != current {
        write_config_file(path, &updated)?;
    }
    *config = updated_config;
    Ok(())
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
    fn tool_default_args_parse_and_round_trip() {
        let cfg = Config::parse(
            r#"
[tools.codex]
command = ["codex"]
args = ["-m", "gpt-default", "-c", "model_reasoning_effort=max"]
"#,
        )
        .unwrap();

        assert_eq!(
            cfg.tool("codex").unwrap().args,
            ["-m", "gpt-default", "-c", "model_reasoning_effort=max"]
        );
        let round_tripped = Config::parse(&cfg.to_toml().unwrap()).unwrap();
        assert_eq!(
            round_tripped.tool("codex").unwrap().args,
            cfg.tool("codex").unwrap().args
        );
    }

    #[test]
    fn tool_copy_mappings_parse_and_round_trip() {
        let cfg = Config::parse(
            r#"
[tools.claude]
command = ["claude"]
copy = [
  { source = "~/.skills", destination = "skills" },
  { source = "shared/CLAUDE.md", destination = "CLAUDE.md" },
]

[tools.claude.profiles.work]
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.tool("claude").unwrap().copy.as_deref(),
            Some(
                [
                    CopyMapping {
                        source: PathBuf::from("~/.skills"),
                        destination: PathBuf::from("skills"),
                    },
                    CopyMapping {
                        source: PathBuf::from("shared/CLAUDE.md"),
                        destination: PathBuf::from("CLAUDE.md"),
                    },
                ]
                .as_slice()
            )
        );

        let serialized = cfg.to_toml().unwrap();
        let round_tripped = Config::parse(&serialized).unwrap();
        assert_eq!(
            round_tripped.tool("claude").unwrap().copy,
            cfg.tool("claude").unwrap().copy
        );
    }

    #[test]
    fn tool_copy_mappings_cannot_mix_with_legacy_skills_source() {
        let error = Config::parse(
            r#"
[tools.codex]
command = ["codex"]
skills_source = "~/.codex/skills"
copy = [{ source = "~/.skills", destination = "skills" }]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot set both"), "{error}");
    }

    #[test]
    fn profile_bypass_defaults_false_and_serializes_only_when_true() {
        let config = Config::parse(
            "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.normal]\n[tools.codex.profiles.fallback]\nbypass = true\n",
        )
        .unwrap();
        let profiles = &config.tool("codex").unwrap().profiles;
        assert!(!profiles["normal"].bypass);
        assert!(profiles["fallback"].bypass);

        let serialized = config.to_toml().unwrap();
        assert_eq!(
            serialized.matches("bypass = true").count(),
            1,
            "{serialized}"
        );
        assert!(!serialized.contains("bypass = false"), "{serialized}");
        let round_tripped = Config::parse(&serialized).unwrap();
        assert!(!round_tripped.tool("codex").unwrap().profiles["normal"].bypass);
        assert!(round_tripped.tool("codex").unwrap().profiles["fallback"].bypass);
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
    fn set_profile_bypass_flips_only_the_flag_and_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# pinned top comment\n\n[tools.codex]\ncommand = [\"codex\"] # inline note\n\n# work profile\n[tools.codex.profiles.work]\n# above the flag\nbypass = true # while auth is broken\n";
        write_config_file(&path, original).unwrap();
        let mut config = Config::load(&path).unwrap();

        set_profile_bypass_in_file(&path, &mut config, "codex", "work", false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, original.replace("bypass = true", "bypass = false"));
        assert!(!config.tool("codex").unwrap().profiles["work"].bypass);
        assert!(!Config::load(&path).unwrap().tool("codex").unwrap().profiles["work"].bypass);
    }

    #[test]
    fn profile_flag_edit_falls_back_to_serialization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config_file(&path, "[tools.codex]\ncommand = [\"codex\"]\n").unwrap();
        let mut config =
            Config::parse("[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.work]\n")
                .unwrap();

        set_profile_bypass_in_file(&path, &mut config, "codex", "work", true).unwrap();

        let reloaded = Config::load(&path).unwrap();
        assert!(reloaded.tool("codex").unwrap().profiles["work"].bypass);
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
