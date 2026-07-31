use anyhow::{bail, Result};

use crate::config::Tool;
use crate::state::State;

pub fn enabled_profiles(tool: &Tool) -> Vec<String> {
    tool.profiles
        .iter()
        .filter(|(_, profile)| profile.enabled)
        .map(|(name, _)| name.clone())
        .collect()
}

/// Choose a forced profile or advance the tool's round-robin cursor.
pub fn select_profile(
    tool_name: &str,
    tool: &Tool,
    state: &mut State,
    forced: Option<&str>,
) -> Result<String> {
    if let Some(name) = forced {
        let Some(profile) = tool.profiles.get(name) else {
            bail!("tool '{tool_name}' has no profile '{name}'");
        };
        if !profile.enabled {
            bail!("profile '{tool_name}/{name}' is disabled");
        }
        return Ok(name.to_string());
    }

    let profiles = enabled_profiles(tool);
    if profiles.is_empty() {
        bail!("tool '{tool_name}' has no enabled profiles");
    }
    let idx = state.round_robin_cursor(tool_name) % profiles.len();
    let selected = profiles[idx].clone();
    state.set_round_robin_cursor(tool_name, (idx + 1) % profiles.len());
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    fn tool_with_profiles(names: &[&str]) -> Tool {
        Tool {
            command: vec!["cmd".to_string()],
            skills_source: None,
            copy: None,
            profiles: names
                .iter()
                .map(|name| ((*name).to_string(), Profile::default()))
                .collect(),
        }
    }

    #[test]
    fn forced_profile_validates_without_changing_cursor() {
        let tool = tool_with_profiles(&["work", "personal"]);
        let mut state = State::default();
        state.set_round_robin_cursor("codex", 1);
        let selected = select_profile("codex", &tool, &mut state, Some("work")).unwrap();
        assert_eq!(selected, "work");
        assert_eq!(state.round_robin_cursor("codex"), 1);
    }

    #[test]
    fn round_robin_advances_across_enabled_profiles() {
        let tool = tool_with_profiles(&["a", "b"]);
        let mut state = State::default();
        assert_eq!(
            select_profile("claude", &tool, &mut state, None).unwrap(),
            "a"
        );
        assert_eq!(
            select_profile("claude", &tool, &mut state, None).unwrap(),
            "b"
        );
        assert_eq!(
            select_profile("claude", &tool, &mut state, None).unwrap(),
            "a"
        );
    }

    #[test]
    fn stale_cursor_after_profile_removal_visits_every_remaining_profile() {
        let tool = tool_with_profiles(&["a", "b"]);
        let mut state = State::default();
        state.set_round_robin_cursor("codex", 8);

        assert_eq!(
            select_profile("codex", &tool, &mut state, None).unwrap(),
            "a"
        );
        assert_eq!(
            select_profile("codex", &tool, &mut state, None).unwrap(),
            "b"
        );
        assert_eq!(state.round_robin_cursor("codex"), 0);
    }

    #[test]
    fn disabled_profiles_are_not_selected() {
        let mut tool = tool_with_profiles(&["a", "b"]);
        tool.profiles.get_mut("a").unwrap().enabled = false;
        let mut state = State::default();
        assert_eq!(enabled_profiles(&tool), vec!["b".to_string()]);
        assert_eq!(
            select_profile("codex", &tool, &mut state, None).unwrap(),
            "b"
        );
        let err = select_profile("codex", &tool, &mut state, Some("a"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("disabled"), "got: {err}");
    }

    #[test]
    fn missing_profile_errors_clearly() {
        let tool = tool_with_profiles(&["a"]);
        let mut state = State::default();
        let err = select_profile("codex", &tool, &mut state, Some("ghost"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no profile"), "got: {err}");
    }
}
