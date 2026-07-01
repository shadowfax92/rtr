//! `state.toml`: the live active-profile selection set by `rtr switch`.
//!
//! Kept separate from `config.toml` so switching never rewrites (and loses the
//! comments of) the user's hand-edited config. The effective active profile is
//! the state override if present, else the tool's `active` default in config.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct State {
    #[serde(default)]
    pub active: BTreeMap<String, String>,
    #[serde(default)]
    pub round_robin: BTreeMap<String, usize>,
}

impl State {
    /// Load state, treating a missing file as empty (first run).
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).context("parsing state.toml"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing state")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn set_active(&mut self, tool: &str, profile: &str) {
        self.active.insert(tool.to_string(), profile.to_string());
    }

    pub fn round_robin_cursor(&self, tool: &str) -> usize {
        self.round_robin.get(tool).copied().unwrap_or(0)
    }

    pub fn set_round_robin_cursor(&mut self, tool: &str, cursor: usize) {
        self.round_robin.insert(tool.to_string(), cursor);
    }

    pub fn active_for(&self, tool: &str, config: &Config) -> Option<String> {
        if let Some(p) = self.active.get(tool) {
            return Some(p.clone());
        }
        config.tools.get(tool).and_then(|t| t.active.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn missing_state_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let st = State::load(&dir.path().join("nope.toml")).unwrap();
        assert!(st.active.is_empty());
    }

    #[test]
    fn set_active_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime").join("state.toml");
        let mut st = State::default();
        st.set_active("codex", "codex-2");
        st.save(&path).unwrap();

        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded.active.get("codex").map(String::as_str), Some("codex-2"));
    }

    #[test]
    fn active_for_prefers_state_over_config_default() {
        let cfg = Config::parse(
            r#"
[tools.codex]
command = ["codex"]
active = "codex-1"

[tools.codex.profiles.codex-1]
set = {}

[tools.codex.profiles.codex-2]
set = {}
"#,
        )
        .unwrap();
        let mut st = State::default();
        assert_eq!(st.active_for("codex", &cfg).as_deref(), Some("codex-1"));
        st.set_active("codex", "codex-2");
        assert_eq!(st.active_for("codex", &cfg).as_deref(), Some("codex-2"));
        assert_eq!(st.active_for("ghost", &cfg), None);
    }
}
