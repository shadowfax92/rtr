//! Quota observations live outside the strict legacy rotation schema. Writers
//! merge under their own short lock; transcript I/O never holds this lock.
//! Success and manual-clear watermarks prevent old tails resurrecting a limit.

use crate::{
    config::Tool,
    limits::{Hold, SuccessWatermarks},
    paths::Paths,
    routing_log::{self, ClearReason, EventKind, RoutingEvent},
    session_status::SessionStatus,
    state::State,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

mod store;
use store::{Scope, Store};

/// Transfer provenance is published before its transcript. A crash can leave an
/// unused origin entry, but never an observable copy without its ownership bound.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Origin {
    pub path: PathBuf,
    pub bytes: u64,
    pub at: DateTime<Utc>,
}
type Origins = BTreeMap<String, BTreeMap<String, BTreeMap<String, Origin>>>;

/// Fork provenance indexed by isolated home and native session ID for one read
/// command. An entry's path identifies the physical file its offset protects.
#[derive(Default)]
pub struct OriginIndex {
    homes: BTreeMap<PathBuf, BTreeMap<String, Origin>>,
    boundaries: BTreeMap<PathBuf, u64>,
}

impl OriginIndex {
    pub(crate) fn for_session(&self, home: &Path, id: &str) -> Option<&Origin> {
        self.homes.get(home)?.get(id)
    }

    pub(crate) fn copy_boundary(&self, path: &Path) -> Option<u64> {
        self.boundaries.get(path).copied()
    }
}

pub fn read_origins(paths: &Paths) -> Result<OriginIndex> {
    let origins: Origins = read(&origins_path(paths))?;
    let homes: BTreeMap<_, _> = origins
        .into_iter()
        .flat_map(|(tool, profiles)| {
            profiles
                .into_iter()
                .map(move |(profile, ids)| (paths.profile_home_dir(&tool, &profile), ids))
        })
        .collect();
    let boundaries = homes
        .values()
        .flat_map(|sessions| {
            sessions
                .values()
                .map(|origin| (origin.path.clone(), origin.bytes))
        })
        .collect();
    Ok(OriginIndex { homes, boundaries })
}

/// The persisted rejection and recovery evidence for one isolated profile.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HoldEvidence {
    pub holds: Vec<Hold>,
    pub success_watermarks: SuccessWatermarks,
    pub cleared_at: Option<DateTime<Utc>>,
}

pub type HoldIndex = BTreeMap<(String, String), HoldEvidence>;

/// Read the persisted evidence. Legacy state converts in memory until a
/// writer has a new observation or an explicit clear to publish.
pub fn read_holds(paths: &Paths) -> Result<HoldIndex> {
    let store = Store::read(&store_path(paths))?;
    Ok(store
        .entries
        .iter()
        .flat_map(|(tool, profiles)| {
            profiles
                .iter()
                .map(move |(profile, entry)| ((tool.clone(), profile.clone()), entry.evidence()))
        })
        .collect())
}

fn read<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}
fn store_path(paths: &Paths) -> PathBuf {
    paths.state_file().with_file_name("quota.json")
}
fn origins_path(paths: &Paths) -> PathBuf {
    paths.state_file().with_file_name("fork-origins.json")
}

pub(crate) fn origin(paths: &Paths, tool: &str, profile: &str, id: &str) -> Result<Option<Origin>> {
    Ok(read::<Origins>(&origins_path(paths))?
        .get(tool)
        .and_then(|p| p.get(profile))
        .and_then(|ids| ids.get(id))
        .cloned())
}

pub(crate) fn record_origin(
    paths: &Paths,
    tool: &str,
    profile: &str,
    id: &str,
    path: &Path,
    bytes: u64,
) -> Result<()> {
    let file = origins_path(paths);
    crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(&file), || {
        let mut origins: Origins = read(&file)?;
        origins
            .entry(tool.into())
            .or_default()
            .entry(profile.into())
            .or_default()
            .insert(
                id.into(),
                Origin {
                    path: path.into(),
                    bytes,
                    at: Utc::now(),
                },
            );
        crate::file_lock::write_private_atomic(&file, &serde_json::to_string(&origins)?)
    })
}

fn has_evidence(status: &SessionStatus) -> bool {
    status.limit.is_some()
        || status.successes.common.is_some()
        || !status.successes.models.is_empty()
}

fn log_tool(tool: &str) -> Option<routing_log::Tool> {
    match tool {
        "claude" => Some(routing_log::Tool::Claude),
        "codex" => Some(routing_log::Tool::Codex),
        _ => None,
    }
}

pub(crate) fn learn(paths: &Paths, statuses: &[SessionStatus]) -> Result<()> {
    let mut observations = BTreeMap::<_, Vec<_>>::new();
    for status in statuses.iter().filter(|status| has_evidence(status)) {
        observations
            .entry((status.tool.as_str(), status.profile.as_str()))
            .or_default()
            .push(status);
    }
    if observations.is_empty() {
        return Ok(());
    }

    let file = store_path(paths);
    let events =
        crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(&file), || {
            let now = Utc::now();
            let operation = uuid::Uuid::new_v4().to_string();
            let mut store = Store::read(&file)?;
            let mut events = Vec::new();
            let mut changed = false;
            for ((tool, profile), statuses) in observations {
                let entry = store.entry(tool, profile);
                let before = entry.clone();
                for status in statuses {
                    entry.observe(status);
                }
                changed |= *entry != before;
                let Some(tool) = log_tool(tool) else { continue };
                // Report the net change to the committed entry. A transient hold
                // suppressed by newer evidence in the same batch was never active.
                for rejection in entry.rejections() {
                    if before.rejections().contains(rejection) {
                        continue;
                    }
                    let hold = rejection.hold();
                    events.push(RoutingEvent::new(
                        now,
                        operation.as_str(),
                        tool,
                        EventKind::HoldLearned {
                            profile: profile.into(),
                            scope: rejection.scope.label().into(),
                            hold_until: hold.until(),
                            native_kind: hold.native_kind.map(Into::into),
                            observed_at: hold.at,
                            reset_at: hold.reset_at,
                        },
                    ));
                }
                for rejection in before.rejections() {
                    let scope = rejection.scope;
                    if entry.rejections().iter().any(|hold| hold.scope == scope) {
                        continue;
                    }
                    if let Some(observed_at) = entry
                        .success_at(scope)
                        .filter(|at| *at > rejection.hold().at)
                    {
                        events.push(RoutingEvent::new(
                            now,
                            operation.as_str(),
                            tool,
                            EventKind::HoldCleared {
                                profile: profile.into(),
                                scope: scope.label().into(),
                                observed_at,
                                reason: ClearReason::Recovery,
                            },
                        ));
                    }
                }
            }
            if changed {
                store.write(&file, now)?;
            }
            Ok(events)
        })?;
    routing_log::append_batch(paths, &events);
    Ok(())
}

pub fn clear(paths: &Paths, tool: &str, profile: &str) -> Result<()> {
    crate::config::Config::load(&paths.config_file())?
        .tool(tool)?
        .profiles
        .get(profile)
        .context("unknown profile")?;
    let file = store_path(paths);
    let events =
        crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(&file), || {
            let mut store = Store::read(&file)?;
            let now = Utc::now();
            let entry = store.entry(tool, profile);
            let mut scopes: Vec<_> = entry.rejections().iter().map(|hold| hold.scope).collect();
            if scopes.is_empty() {
                scopes.push(Scope::Profile);
            }
            entry.clear(now);
            store.write(&file, now)?;

            // An empty entry can still clear earlier numeric observations.
            let operation = uuid::Uuid::new_v4().to_string();
            Ok(log_tool(tool)
                .into_iter()
                .flat_map(|tool| {
                    let operation = operation.as_str();
                    scopes.iter().map(move |scope| {
                        RoutingEvent::new(
                            now,
                            operation,
                            tool,
                            EventKind::HoldCleared {
                                profile: profile.into(),
                                scope: scope.label().into(),
                                observed_at: now,
                                reason: ClearReason::Manual,
                            },
                        )
                    })
                })
                .collect::<Vec<_>>())
        })?;
    routing_log::append_batch(paths, &events);
    Ok(())
}

/// Exit 75 is typed, so JSON/report output and the one-line stderr diagnostic
/// share one selection failure without flattening every other failure to 75.
#[derive(Debug, Serialize)]
pub struct NoUsableProfiles {
    pub schema_version: u8,
    pub destination: Option<String>,
    pub earliest_reset: Option<DateTime<Utc>>,
    pub limited: Vec<LimitedProfile>,
}
#[derive(Debug, Serialize)]
pub struct LimitedProfile {
    pub profile: String,
    pub until: DateTime<Utc>,
}
impl std::fmt::Display for NoUsableProfiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no usable profile; earliest reset {}",
            self.earliest_reset
                .map(crate::output::local_time)
                .unwrap_or_else(|| "unknown (no enabled profiles)".into())
        )
    }
}
impl std::error::Error for NoUsableProfiles {}

pub(crate) struct Availability {
    limited: BTreeMap<String, DateTime<Utc>>,
}
impl Availability {
    pub(crate) fn warn(&self, tool: &str, profile: &str) {
        if let Some(until) = self.limited.get(profile) {
            eprintln!(
                "rtr: warning: {tool}/{} is limited until {}; obeying explicit profile",
                crate::launch_settings::single_line(profile),
                crate::output::local_time(*until)
            );
        }
    }
    pub(crate) fn select(
        &self,
        tool_name: &str,
        tool: &Tool,
        state: &mut State,
        source: Option<&str>,
        forced: Option<&str>,
    ) -> Result<String> {
        if let Some(name) = forced {
            self.warn(tool_name, name);
            return crate::selection::select_profile(tool_name, tool, state, forced);
        }
        let blocked: BTreeSet<_> = self.limited.keys().cloned().collect();
        let allocation = if source.is_none() {
            Some(crate::selection::allocation(tool_name, tool)?)
        } else {
            None
        };
        if !tool.profiles.iter().any(|(n, p)| {
            p.enabled
                && !blocked.contains(n)
                && allocation
                    .as_ref()
                    .is_none_or(|a| a.weights.get(n).is_some_and(|w| *w > 0))
        }) {
            return Err(NoUsableProfiles {
                schema_version: 1,
                destination: None,
                earliest_reset: self
                    .limited
                    .iter()
                    .filter(|(n, _)| tool.profiles.get(*n).is_some_and(|p| p.enabled))
                    .map(|(_, until)| *until)
                    .min(),
                limited: self
                    .limited
                    .iter()
                    .filter(|(n, _)| tool.profiles.get(*n).is_some_and(|p| p.enabled))
                    .map(|(profile, until)| LimitedProfile {
                        profile: profile.clone(),
                        until: *until,
                    })
                    .collect(),
            }
            .into());
        }
        crate::selection::select_available(tool_name, tool, state, source, &blocked)
    }
}

/// Validate a tentative destination before committing its rotation slot. A
/// healthy candidate needs only its own recent tails; rejected candidates never
/// consume credit. If every remembered profile is blocked, inspect the rest for
/// a newer success (for example, purchased credits) before returning exit 75.
pub(crate) fn choose(
    paths: &Paths,
    tool_name: &str,
    tool: &Tool,
    state: &mut State,
    source: Option<&str>,
    forced: Option<&str>,
    persist: bool,
) -> Result<String> {
    let mut store = Store::read(&store_path(paths))?;
    let mut checked = BTreeSet::new();
    loop {
        let available = from_store(&store, tool_name);
        let mut trial = state.clone();
        // Explicit selections are validated before I/O and warned only after
        // their own preflight, so the warning includes freshly learned limits.
        let selection = if forced.is_some() {
            crate::selection::select_profile(tool_name, tool, &mut trial, forced)
        } else {
            available.select(tool_name, tool, &mut trial, source, None)
        };
        let candidate = match selection {
            Ok(name) => name,
            Err(error) if error.downcast_ref::<NoUsableProfiles>().is_some() => {
                let next = tool
                    .profiles
                    .iter()
                    .find(|(name, p)| p.enabled && !checked.contains(*name))
                    .map(|(name, _)| name.clone());
                match next {
                    Some(name) => name,
                    None => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        if checked.insert(candidate.clone()) {
            let statuses = recent_statuses(paths, tool_name, &candidate)?;
            if persist {
                learn(paths, &statuses)?;
                // Merge another watchdog's observations rather than overwriting
                // them with the preflight snapshot captured before transcript I/O.
                store = Store::read(&store_path(paths))?;
            } else {
                for status in statuses.iter().filter(|status| has_evidence(status)) {
                    store.entry(&status.tool, &status.profile).observe(status);
                }
            }
        }
        let available = from_store(&store, tool_name);
        if forced.is_some() {
            available.warn(tool_name, &candidate);
            return Ok(candidate);
        }
        if !available.limited.contains_key(&candidate) {
            // Retry selection against the updated pool: a fallback scan may
            // have recovered a profile that changes weighted/equal eligibility.
            let mut confirmed = state.clone();
            let target = match available.select(tool_name, tool, &mut confirmed, source, None) {
                Ok(target) => target,
                Err(error) if error.downcast_ref::<NoUsableProfiles>().is_some() => continue,
                Err(error) => return Err(error),
            };
            if target == candidate {
                *state = confirmed;
                return Ok(target);
            }
        }
    }
}

/// Resume retains exact-home identity even for disabled profiles. Inspect quota
/// only to warn: do not route, persist observations, or block native startup if
/// optional warning metadata is unavailable.
pub(crate) fn warn_for_resume(paths: &Paths, tool: &str, profile: &str) {
    let check = || -> Result<()> {
        let mut store = Store::read(&store_path(paths))?;
        for status in recent_statuses(paths, tool, profile)? {
            store.entry(tool, profile).observe(&status);
        }
        from_store(&store, tool).warn(tool, profile);
        Ok(())
    };
    if let Err(error) = check() {
        eprintln!(
            "rtr: could not check usage limit for {}/{}: {}",
            tool,
            crate::launch_settings::single_line(profile),
            crate::launch_settings::single_line(&format!("{error:#}"))
        );
    }
}

fn from_store(store: &Store, tool: &str) -> Availability {
    let now = Utc::now();
    let limited = store
        .entries
        .get(tool)
        .into_iter()
        .flat_map(|profiles| profiles.iter())
        .filter_map(|(name, entry)| {
            // This compatibility caller has no model. Model-aware routing
            // reads the full HoldIndex once it resolves the requested family.
            entry
                .blocked_until(now, false)
                .map(|until| (name.clone(), until))
        })
        .collect();
    Availability { limited }
}

/// File metadata chooses four recent top-level histories without opening the
/// session catalog or indexing user dialogue. Native homes remain read-only.
fn recent_statuses(paths: &Paths, tool_name: &str, profile: &str) -> Result<Vec<SessionStatus>> {
    let home = paths.profile_home_dir(tool_name, profile);
    let files = if tool_name == "claude" {
        let mut files = Vec::new();
        for project in crate::conversations::directories_if_exists(&home.join("projects"))? {
            files.extend(crate::conversations::jsonl_files(&project)?);
        }
        files
    } else {
        crate::conversations::codex_transcript_files(&home)?
    };
    let mut recent = files
        .into_iter()
        .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
        .collect::<Vec<_>>();
    recent.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut statuses = Vec::new();
    for (_, path) in recent.into_iter().take(4) {
        let id = if tool_name == "claude" {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        } else {
            crate::codex_history::read_header(&path)?
                .and_then(|h| h["payload"]["id"].as_str().map(str::to_string))
        };
        if let Some(id) = id {
            statuses.push(crate::session_status::inspect(
                paths, tool_name, profile, &id, &path,
            )?);
        }
    }
    Ok(statuses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;
    #[test]
    fn filtered_shares_keep_original_ratios() {
        let tool = Tool {
            command: vec!["unused".into()],
            args: vec![],
            copy: None,
            skills_source: None,
            profiles: [("a", 10), ("b", 45), ("c", 45)]
                .into_iter()
                .map(|(n, w)| {
                    (
                        n.into(),
                        Profile {
                            share_percent: Some(w),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        };
        let available = Availability {
            limited: [("b".into(), Utc::now() + chrono::Duration::hours(1))].into(),
        };
        let mut state = State::default();
        let mut counts = BTreeMap::<String, usize>::new();
        for _ in 0..110 {
            *counts
                .entry(
                    available
                        .select("claude", &tool, &mut state, None, None)
                        .unwrap(),
                )
                .or_default() += 1;
        }
        assert_eq!(counts, [("a".into(), 20), ("c".into(), 90)].into());
        assert_eq!(
            available
                .select("claude", &tool, &mut state, Some("c"), None)
                .unwrap(),
            "a"
        );
    }
}
