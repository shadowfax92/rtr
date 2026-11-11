//! Quota observations live outside the strict legacy rotation schema. Writers
//! merge under their own short lock; transcript I/O never holds this lock.
//! Success and manual-clear watermarks prevent old tails resurrecting a limit.

use crate::{
    config::Tool,
    paths::Paths,
    session_status::{Limit, SessionStatus},
    state::State,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Default, Serialize, Deserialize)]
struct Entry {
    limit: Option<Limit>,
    native_kind: Option<String>,
    success_at: Option<DateTime<Utc>>,
    cleared_at: Option<DateTime<Utc>>,
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    entries: BTreeMap<String, BTreeMap<String, Entry>>,
}

/// Transfer provenance is published before its transcript. A crash can leave an
/// unused origin entry, but never an observable copy without its ownership bound.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Origin {
    pub path: PathBuf,
    pub bytes: u64,
    pub at: DateTime<Utc>,
}
type Origins = BTreeMap<String, BTreeMap<String, BTreeMap<String, Origin>>>;

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

fn observe(store: &mut Store, status: &SessionStatus) {
    let e = store
        .entries
        .entry(status.tool.clone())
        .or_default()
        .entry(status.profile.clone())
        .or_default();
    e.success_at = e.success_at.max(status.success_at);
    if let Some(limit) = &status.limit {
        let watermark = e.success_at.max(e.cleared_at);
        if watermark.is_none_or(|at| limit.at > at)
            && e.limit.as_ref().is_none_or(|l| limit.at >= l.at)
        {
            e.limit = Some(limit.clone());
            e.native_kind = limit.native_kind.clone();
        }
    }
    if e.limit
        .as_ref()
        .is_some_and(|l| e.success_at.max(e.cleared_at).is_some_and(|at| at > l.at))
    {
        e.limit = None;
    }
}

pub(crate) fn learn(paths: &Paths, statuses: &[SessionStatus]) -> Result<()> {
    if statuses
        .iter()
        .all(|s| s.limit.is_none() && s.success_at.is_none())
    {
        return Ok(());
    }
    let file = store_path(paths);
    crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(&file), || {
        let mut store: Store = read(&file)?;
        for s in statuses {
            observe(&mut store, s);
        }
        crate::file_lock::write_private_atomic(&file, &serde_json::to_string(&store)?)
    })
}

pub fn clear(paths: &Paths, tool: &str, profile: &str) -> Result<()> {
    crate::config::Config::load(&paths.config_file())?
        .tool(tool)?
        .profiles
        .get(profile)
        .context("unknown profile")?;
    let file = store_path(paths);
    crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(&file), || {
        let mut store: Store = read(&file)?;
        let e = store
            .entries
            .entry(tool.into())
            .or_default()
            .entry(profile.into())
            .or_default();
        e.limit = None;
        e.cleared_at = Some(Utc::now());
        crate::file_lock::write_private_atomic(&file, &serde_json::to_string(&store)?)
    })
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
    let mut store: Store = read(&store_path(paths))?;
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
                store = read(&store_path(paths))?;
            } else {
                for status in &statuses {
                    observe(&mut store, status);
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
        let mut store: Store = read(&store_path(paths))?;
        for status in recent_statuses(paths, tool, profile)? {
            observe(&mut store, &status);
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
    let limited = store
        .entries
        .get(tool)
        .into_iter()
        .flat_map(|profiles| profiles.iter())
        .filter_map(|(name, e)| {
            e.limit
                .as_ref()
                .filter(|l| l.until() > Utc::now())
                .map(|l| (name.clone(), l.until()))
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

pub(crate) fn held_limits(paths: &Paths) -> Result<BTreeMap<(String, String), DateTime<Utc>>> {
    let store: Store = read(&store_path(paths))?;
    Ok(store
        .entries
        .into_iter()
        .flat_map(|(tool, profiles)| {
            profiles.into_iter().filter_map(move |(p, e)| {
                e.limit
                    .filter(|l| l.until() > Utc::now())
                    .map(|l| ((tool.clone(), p), l.until()))
            })
        })
        .collect())
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

    #[test]
    fn expiry_and_success_clear_and_old_observations_do_not_regress() {
        let now = Utc::now();
        let mut store = Store::default();
        let mut status = SessionStatus {
            schema_version: 1,
            tool: "claude".into(),
            profile: "a".into(),
            id: "s".into(),
            cwd: Default::default(),
            model: None,
            effort: None,
            success_at: None,
            limit: Some(Limit {
                kind: "weekly".into(),
                at: now,
                resets_at: Some(now + chrono::Duration::hours(1)),
                active: true,
                message: "limit".into(),
                native_kind: Some("seven_day".into()),
            }),
        };
        observe(&mut store, &status);
        assert!(store.entries["claude"]["a"].limit.is_some());
        status.success_at = Some(now + chrono::Duration::seconds(1));
        observe(&mut store, &status);
        assert!(store.entries["claude"]["a"].limit.is_none());
        status.success_at = None;
        observe(&mut store, &status);
        assert!(store.entries["claude"]["a"].limit.is_none());
        let mut expired = status.limit.unwrap();
        expired.resets_at = Some(now - chrono::Duration::seconds(1));
        assert!(expired.until() < now);
    }
}
