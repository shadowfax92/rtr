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

/// Choose the profile for one subscription run without changing legacy active-profile state.
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

pub fn resolve_preset(
    tool_name: &str,
    tool: &Tool,
    requested: Option<&str>,
) -> Result<(Option<String>, Vec<String>)> {
    let selected = requested
        .map(str::to_string)
        .or_else(|| tool.default_preset.clone());
    let Some(name) = selected else {
        return Ok((None, Vec::new()));
    };
    let Some(preset) = tool.presets.get(&name) else {
        bail!("tool '{tool_name}' has no preset '{name}'");
    };
    Ok((Some(name), preset.args.clone()))
}

pub fn build_argv(
    command: &[String],
    preset_args: &[String],
    trailing_args: &[String],
) -> Vec<String> {
    let mut argv = Vec::with_capacity(command.len() + preset_args.len() + trailing_args.len());
    argv.extend_from_slice(command);
    argv.extend_from_slice(preset_args);
    argv.extend_from_slice(trailing_args);
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Preset, Profile};
    use std::collections::BTreeMap;

    fn tool_with_profiles(names: &[&str]) -> Tool {
        Tool {
            command: vec!["cmd".to_string()],
            hosts: vec![],
            active: None,
            selection: Some("round-robin".to_string()),
            default_preset: None,
            presets: BTreeMap::new(),
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
    fn missing_profile_or_preset_errors_clearly() {
        let mut tool = tool_with_profiles(&["a"]);
        let mut state = State::default();
        let err = select_profile("codex", &tool, &mut state, Some("ghost"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no profile"), "got: {err}");

        tool.presets.insert(
            "x".to_string(),
            Preset {
                args: vec!["--x".to_string()],
            },
        );
        assert_eq!(
            resolve_preset("codex", &tool, Some("x")).unwrap(),
            (Some("x".to_string()), vec!["--x".to_string()])
        );
        let err = resolve_preset("codex", &tool, Some("ghost"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no preset"), "got: {err}");
    }

    #[test]
    fn argv_order_is_command_then_preset_then_trailing() {
        let argv = build_argv(
            &["codex".to_string(), "--base".to_string()],
            &["--preset".to_string(), "x".to_string()],
            &["--extra".to_string()],
        );
        assert_eq!(
            argv,
            vec![
                "codex".to_string(),
                "--base".to_string(),
                "--preset".to_string(),
                "x".to_string(),
                "--extra".to_string()
            ]
        );
    }
}
