//! Resolve user percentages once for display and automatic selection. Allocation
//! uses integer ratios; only presentation rounds the equal remainder. Callers
//! own the state lock and commit a selection after native-home preparation.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};

use crate::config::Tool;
use crate::state::{State, WeightedRotation};

/// Exact relative shares for enabled profiles, including zero-share profiles.
/// Disabled profiles have no entry. Empty tools remain inspectable/configurable.
#[derive(Debug)]
pub struct Allocation {
    pub weights: BTreeMap<String, i64>,
    pub total: i64,
    weighted: bool,
}

impl Allocation {
    pub fn percent(&self, profile: &str) -> Option<f64> {
        self.weights
            .get(profile)
            .map(|weight| 100.0 * *weight as f64 / self.total as f64)
    }
}

pub fn allocation(tool_name: &str, tool: &Tool) -> Result<Allocation> {
    let enabled: Vec<_> = tool
        .profiles
        .iter()
        .filter(|(_, profile)| profile.enabled)
        .collect();
    let fixed: i64 = enabled
        .iter()
        .filter_map(|(_, profile)| profile.share_percent.map(i64::from))
        .sum();
    let automatic = enabled
        .iter()
        .filter(|(_, profile)| profile.share_percent.is_none())
        .count() as i64;
    if fixed > 100 {
        bail!("tool '{tool_name}' fixed shares total {fixed}%, exceeding 100%; lower a share or run `rtr weight {tool_name} --reset`");
    }
    if !enabled.is_empty() && automatic == 0 && fixed != 100 {
        bail!("tool '{tool_name}' fixed shares total {fixed}% with no enabled profile to take the remainder; set shares to total 100%, enable a profile without a share, or run `rtr weight {tool_name} --reset`");
    }
    // Multiplying fixed shares by the number of remainder profiles expresses
    // fractional shares exactly (25/37.5/37.5 becomes 50/75/75).
    let scale = automatic.max(1);
    let weights: BTreeMap<_, _> = enabled
        .into_iter()
        .map(|(name, profile)| {
            (
                name.clone(),
                profile
                    .share_percent
                    .map_or(100 - fixed, |percent| i64::from(percent) * scale),
            )
        })
        .collect();
    let total = weights.values().sum();
    let weighted = automatic < weights.len() as i64;
    Ok(Allocation {
        weights,
        total,
        weighted,
    })
}

pub fn enabled_profiles(tool: &Tool) -> Vec<String> {
    tool.profiles
        .iter()
        .filter(|(_, profile)| profile.enabled)
        .map(|(name, _)| name.clone())
        .collect()
}

/// Choose an explicit profile without consuming a slot, or advance rotation.
pub fn select_profile(
    tool_name: &str,
    tool: &Tool,
    state: &mut State,
    forced: Option<&str>,
) -> Result<String> {
    select(tool_name, tool, state, None, forced)
}

pub fn select_fork_profile(
    tool_name: &str,
    tool: &Tool,
    state: &mut State,
    source: &str,
    forced: Option<&str>,
) -> Result<String> {
    select(tool_name, tool, state, Some(source), forced)
}

fn select(
    tool_name: &str,
    tool: &Tool,
    state: &mut State,
    source: Option<&str>,
    forced: Option<&str>,
) -> Result<String> {
    if let Some(name) = forced {
        validate_explicit(tool_name, tool, name)?;
        return Ok(name.to_owned());
    }
    let allocation = allocation(tool_name, tool)?;
    if allocation.weights.is_empty() {
        bail!("tool '{tool_name}' has no enabled profiles");
    }
    let mut eligible = allocation.weights.keys().map(String::as_str).collect();
    exclude_fork_source(&mut eligible, source);
    select_from(tool_name, &allocation, &eligible, state).map(|(profile, _)| profile)
}

pub(crate) fn validate_explicit<'a>(
    tool_name: &str,
    tool: &'a Tool,
    name: &str,
) -> Result<&'a crate::config::Profile> {
    let Some(profile) = tool.profiles.get(name) else {
        bail!("tool '{tool_name}' has no profile '{name}'");
    };
    if !profile.enabled {
        bail!("profile '{tool_name}/{name}' is disabled");
    }
    Ok(profile)
}

pub(crate) fn exclude_fork_source(eligible: &mut BTreeSet<&str>, source: Option<&str>) {
    if let Some(source) = source.filter(|_| eligible.len() > 1) {
        eligible.remove(source);
    }
}

/// Apply the mask to one turn. Rotation state still carries the full allocation
/// so changing model pools freezes excluded scores without resetting credit.
pub(crate) fn select_from(
    tool_name: &str,
    allocation: &Allocation,
    eligible: &BTreeSet<&str>,
    state: &mut State,
) -> Result<(String, crate::routing_log::SelectionReason)> {
    use crate::routing_log::SelectionReason;
    let total: i64 = eligible.iter().map(|name| allocation.weights[*name]).sum();
    if allocation.weighted {
        let rotation = state.weighted.entry(tool_name.to_owned()).or_default();
        if rotation.weights != allocation.weights
            || !rotation.scores.keys().eq(allocation.weights.keys())
        {
            *rotation = WeightedRotation {
                scores: allocation
                    .weights
                    .keys()
                    .map(|name| (name.clone(), 0))
                    .collect(),
                weights: allocation.weights.clone(),
            };
        }
        if total > 0 {
            return select_weighted(rotation, eligible, total)
                .map(|profile| (profile, SelectionReason::Weighted));
        }
    } else if state.weighted.remove(tool_name).is_some() {
        state.set_round_robin_cursor(tool_name, 0);
    }
    // A fork can rotate uniformly among zero-share alternatives. Its index
    // still refers to the full name-ordered profile list.
    let profiles: Vec<_> = allocation.weights.keys().collect();
    anyhow::ensure!(!profiles.is_empty(), "no usable profile");
    let start = state.round_robin_cursor(tool_name) % profiles.len();
    let index = (start..start + profiles.len())
        .find(|i| eligible.contains(profiles[i % profiles.len()].as_str()))
        .context("no usable profile")?
        % profiles.len();
    state.set_round_robin_cursor(tool_name, (index + 1) % profiles.len());
    Ok((profiles[index].clone(), SelectionReason::RoundRobin))
}

fn select_weighted(
    rotation: &mut WeightedRotation,
    eligible: &BTreeSet<&str>,
    total: i64,
) -> Result<String> {
    // Credit accrues to each eligible profile on every automatic selection.
    // Subtracting the total from the winner spaces its selections across time
    // instead of clustering a profile's entire allocation into a consecutive run.
    let mut selected: Option<(&str, i64)> = None;
    for (name, score) in &mut rotation.scores {
        let weight = rotation.weights[name];
        if weight == 0 || !eligible.contains(name.as_str()) {
            continue;
        }
        *score = score
            .checked_add(weight)
            .context("weighted scheduling score overflow")?;
        if selected.is_none_or(|(_, best)| *score > best) {
            selected = Some((name, *score));
        }
    }
    let selected = selected
        .context("no profile has a positive automatic share")?
        .0
        .to_string();
    let score = rotation.scores.get_mut(&selected).unwrap();
    *score = score
        .checked_sub(total)
        .context("weighted scheduling score overflow")?;
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    fn tool_with_profiles(names: &[&str]) -> Tool {
        Tool {
            command: vec!["cmd".to_string()],
            args: Vec::new(),
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

    #[test]
    fn every_integer_percentage_preserves_exact_fractional_remainder_over_a_cycle() {
        for percent in 0..=100 {
            let mut tool = tool_with_profiles(&["a", "b", "c"]);
            tool.profiles.get_mut("a").unwrap().share_percent = Some(percent);
            let mut state = State::default();
            let mut counts = BTreeMap::<String, usize>::new();
            for _ in 0..200 {
                *counts
                    .entry(select_profile("codex", &tool, &mut state, None).unwrap())
                    .or_default() += 1;
            }
            assert_eq!(
                counts.get("a").copied().unwrap_or(0),
                usize::from(percent) * 2,
                "{percent}%"
            );
            for name in ["b", "c"] {
                assert_eq!(
                    counts.get(name).copied().unwrap_or(0),
                    usize::from(100 - percent),
                    "{percent}%, {name}"
                );
            }
        }
    }

    #[test]
    fn weighted_progress_survives_serialization_and_is_independent_per_tool() {
        let mut tool = tool_with_profiles(&["a", "b"]);
        tool.profiles.get_mut("a").unwrap().share_percent = Some(25);
        let mut state = State::default();
        let mut selected = Vec::new();
        for _ in 0..4 {
            selected.push(select_profile("codex", &tool, &mut state, None).unwrap());
            let serialized = toml::to_string(&state).unwrap();
            assert_eq!(
                select_profile("codex", &tool, &mut state, Some("a")).unwrap(),
                "a"
            );
            assert_eq!(toml::to_string(&state).unwrap(), serialized);
            state = toml::from_str(&serialized).unwrap();
        }
        assert_eq!(selected, ["b", "a", "b", "b"]);
        assert_eq!(
            select_profile("claude", &tool, &mut state, None).unwrap(),
            "b"
        );
        assert_eq!(
            select_profile("claude", &tool, &mut state, None).unwrap(),
            "a"
        );
        assert_eq!(
            select_profile("codex", &tool, &mut state, None).unwrap(),
            "b"
        );
    }

    #[test]
    fn policy_changes_discard_old_credit_and_reset_returns_to_equal_rotation() {
        let mut tool = tool_with_profiles(&["a", "b", "c"]);
        let mut state = State::default();
        state.set_round_robin_cursor("codex", 2);
        tool.profiles.get_mut("a").unwrap().share_percent = Some(25);
        select_profile("codex", &tool, &mut state, None).unwrap();
        tool.profiles.remove("c");
        let new_first = select_profile("codex", &tool, &mut state, None).unwrap();
        assert_eq!(
            new_first,
            select_profile("codex", &tool, &mut State::default(), None).unwrap()
        );
        assert!(!state.weighted["codex"].weights.contains_key("c"));
        tool.profiles.get_mut("a").unwrap().share_percent = None;
        assert_eq!(
            select_profile("codex", &tool, &mut state, None).unwrap(),
            "a"
        );
        assert!(state.weighted.is_empty());
        assert_eq!(
            select_profile("codex", &tool, &mut state, None).unwrap(),
            "b"
        );
    }

    #[test]
    fn incomplete_or_excessive_fixed_allocations_do_not_advance_state() {
        let mut tool = tool_with_profiles(&["a", "b"]);
        tool.profiles.get_mut("a").unwrap().share_percent = Some(25);
        let mut state = State::default();
        select_profile("codex", &tool, &mut state, None).unwrap();
        let before = toml::to_string(&state).unwrap();
        for percent in [0, 25, 80, 100] {
            tool.profiles.get_mut("b").unwrap().share_percent = Some(percent);
            assert!(select_profile("codex", &tool, &mut state, None).is_err());
            assert_eq!(toml::to_string(&state).unwrap(), before);
            assert!(select_profile("codex", &tool, &mut state, Some("a")).is_ok());
        }
        tool.profiles.get_mut("b").unwrap().share_percent = Some(75);
        assert!(select_profile("codex", &tool, &mut state, None).is_ok());
    }
}

#[cfg(test)]
mod fork_tests {
    use super::*;
    fn tool() -> Tool {
        toml::from_str("command=['unused']\n[profiles.a]\n[profiles.b]\n[profiles.c]\n").unwrap()
    }
    #[test]
    fn forks_skip_source_and_keep_equal_rotation_progress() {
        let tool = tool();
        let mut state = State::default();
        let picks: Vec<_> = (0..6)
            .map(|_| select_fork_profile("codex", &tool, &mut state, "a", None).unwrap())
            .collect();
        assert_eq!(picks, ["b", "c", "b", "c", "b", "c"]);
        assert_eq!(
            select_profile("codex", &tool, &mut state, None).unwrap(),
            "a"
        );
    }
    #[test]
    fn forks_preserve_relative_weighted_shares_among_other_profiles() {
        let mut tool = tool();
        tool.profiles.get_mut("a").unwrap().share_percent = Some(20);
        tool.profiles.get_mut("b").unwrap().share_percent = Some(20);
        tool.profiles.get_mut("c").unwrap().share_percent = Some(60);
        let mut state = State::default();
        let picks: Vec<_> = (0..40)
            .map(|_| select_fork_profile("codex", &tool, &mut state, "a", None).unwrap())
            .collect();
        assert_eq!(picks.iter().filter(|p| *p == "b").count(), 10);
        assert_eq!(picks.iter().filter(|p| *p == "c").count(), 30);
        let before = state.clone();
        assert_eq!(
            select_fork_profile("codex", &tool, &mut state, "a", Some("a")).unwrap(),
            "a"
        );
        assert_eq!(
            toml::to_string(&state).unwrap(),
            toml::to_string(&before).unwrap()
        );
    }
    #[test]
    fn a_single_enabled_profile_forks_in_place_and_zero_share_alternatives_rotate() {
        let mut tool = tool();
        tool.profiles.get_mut("b").unwrap().enabled = false;
        tool.profiles.get_mut("c").unwrap().enabled = false;
        assert_eq!(
            select_fork_profile("codex", &tool, &mut State::default(), "a", None).unwrap(),
            "a"
        );
        for profile in tool.profiles.values_mut() {
            profile.enabled = true;
        }
        tool.profiles.get_mut("a").unwrap().share_percent = Some(100);
        assert_eq!(
            select_fork_profile("codex", &tool, &mut State::default(), "a", None).unwrap(),
            "b"
        );
    }
}
