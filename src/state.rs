//! `state.toml`: round-robin cursors for automatic profile selection.
//!
//! Kept separate from `config.toml` so launches never rewrite the user's
//! hand-edited configuration.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct State {
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
        let text = toml::to_string_pretty(self).context("serializing state")?;
        crate::file_lock::write_private_atomic(path, &text)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Load, mutate, and atomically save state while holding the state lock.
    pub fn update_locked<R, F>(path: &Path, update: F) -> Result<R>
    where
        F: FnOnce(&mut Self) -> Result<R>,
    {
        crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(path), || {
            let mut state = Self::load(path)?;
            let result = update(&mut state)?;
            state.save(path)?;
            Ok(result)
        })
    }

    pub fn round_robin_cursor(&self, tool: &str) -> usize {
        self.round_robin.get(tool).copied().unwrap_or(0)
    }

    pub fn set_round_robin_cursor(&mut self, tool: &str, cursor: usize) {
        self.round_robin.insert(tool.to_string(), cursor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_state_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let st = State::load(&dir.path().join("nope.toml")).unwrap();
        assert!(st.round_robin.is_empty());
    }

    #[test]
    fn round_robin_cursor_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime").join("state.toml");
        let mut st = State::default();
        st.set_round_robin_cursor("codex", 2);
        st.save(&path).unwrap();

        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded.round_robin_cursor("codex"), 2);
    }

    #[test]
    fn update_locked_persists_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime").join("state.toml");
        State::update_locked(&path, |st| {
            st.set_round_robin_cursor("codex", st.round_robin_cursor("codex") + 1);
            Ok(())
        })
        .unwrap();
        State::update_locked(&path, |st| {
            st.set_round_robin_cursor("codex", st.round_robin_cursor("codex") + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(State::load(&path).unwrap().round_robin_cursor("codex"), 2);
    }

    #[test]
    fn removed_active_profile_state_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        std::fs::write(&path, "[active]\ncodex = \"personal\"\n").unwrap();
        assert!(State::load(&path).is_err());
    }
}
