//! Quota observations live outside the strict legacy rotation schema. Writers
//! merge under a quota lock. Learning gathers transcript evidence before locking.
//! Success and manual-clear watermarks prevent old tails resurrecting a limit.

use crate::{
    limits::{
        self, Blocker, Evaluation, Hold, HoldScope, MeterKey, MeterScope, ModelFamily,
        ProfileLimits, SuccessWatermarks,
    },
    native_usage,
    paths::Paths,
    routing_log::{self, ClearReason, EventKind, RoutingEvent},
    session_status::SessionStatus,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

mod store;
use store::{Observation, Scope, Store};

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
    preview_holds(paths, &[])
}

/// Apply the same owned observations as a real preflight without saving
/// watermarks, taking the quota lock, or emitting hold transitions.
pub(crate) fn preview_holds(paths: &Paths, statuses: &[SessionStatus]) -> Result<HoldIndex> {
    project_holds(paths, statuses, Observation::Routing)
}

/// Include retired rejections in the read-only listing preview. Their
/// watermarks let the renderer explain the recovery without showing a block.
pub(crate) fn listing_holds(paths: &Paths, statuses: &[SessionStatus]) -> Result<HoldIndex> {
    project_holds(paths, statuses, Observation::Listing)
}

fn project_holds(
    paths: &Paths,
    statuses: &[SessionStatus],
    observation: Observation,
) -> Result<HoldIndex> {
    let mut store = Store::read(&store_path(paths))?;
    let now = Utc::now();
    for ((tool, profile), statuses) in group_observations(statuses) {
        store
            .entry(tool, profile)
            .observe(&statuses, now, observation);
    }
    Ok(store
        .entries
        .iter()
        .flat_map(|(tool, profiles)| {
            profiles.iter().map(move |(profile, entry)| {
                let evidence = match observation {
                    Observation::Routing => entry.evidence(),
                    Observation::Listing => entry.listing_evidence(),
                };
                ((tool.clone(), profile.clone()), evidence)
            })
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

fn group_observations(statuses: &[SessionStatus]) -> BTreeMap<(&str, &str), Vec<&SessionStatus>> {
    let mut observations = BTreeMap::<_, Vec<_>>::new();
    for status in statuses.iter().filter(|status| has_evidence(status)) {
        observations
            .entry((status.tool.as_str(), status.profile.as_str()))
            .or_default()
            .push(status);
    }
    observations
}

pub(crate) fn learn(paths: &Paths, statuses: &[SessionStatus]) -> Result<()> {
    let observations = group_observations(statuses);
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
            let empty_entry = store::Entry::default();
            for ((tool, profile), statuses) in observations {
                let before = store
                    .entries
                    .get(tool)
                    .and_then(|profiles| profiles.get(profile))
                    .unwrap_or(&empty_entry);
                let mut entry = before.clone();
                entry.observe(&statuses, now, Observation::Routing);
                if &entry == before {
                    continue;
                }
                changed = true;
                if let Some(tool) = log_tool(tool) {
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
                        let hold = rejection.hold();
                        // Natural expiry has already ended this hold. Only a
                        // recovery that removes a live hold belongs in the log.
                        if hold.until() <= now
                            || entry.rejections().iter().any(|hold| hold.scope == scope)
                        {
                            continue;
                        }
                        if let Some(observed_at) =
                            entry.success_at(scope).filter(|at| *at > hold.at)
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
                *store.entry(tool, profile) = entry;
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
    let home = paths.profile_home_dir(tool, profile);
    let file = store_path(paths);
    let (events, missing_origins) =
        crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(&file), || {
            let mut store = Store::read(&file)?;
            // Read native evidence after taking the lock. A refresh while
            // waiting must be visible if the new watermark will cover it.
            let now = Utc::now();
            let native = if tool == "codex" {
                // Freeze filenames before reading origins so that a published
                // copy always has provenance in the reader's snapshot.
                let histories = native_usage::codex_histories(&home, |_| {});
                read_origins(paths).map(|origins| {
                    native_usage::read_discovered(&home, tool, &origins, now, &histories)
                })
            } else {
                Ok(native_usage::read_owned(
                    &home,
                    tool,
                    &OriginIndex::default(),
                    now,
                ))
            };
            let missing_origins = native.is_err();
            let native = native.unwrap_or_default();
            let entry = store.entry(tool, profile);
            let evidence = entry.evidence();
            let limits = ProfileLimits {
                meters: native.meters,
                holds: evidence.holds,
                success_watermarks: evidence.success_watermarks,
                cleared_at: evidence.cleared_at,
                credit_info: native.credit_info,
            };
            let scopes = blocking_scopes(&limits, tool, now);
            // Advance the watermark even when nothing was blocking.
            entry.clear(now);
            store.write(&file, now)?;

            let operation = uuid::Uuid::new_v4().to_string();
            let events = log_tool(tool)
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
                .collect::<Vec<_>>();
            Ok((events, missing_origins))
        })?;
    if missing_origins {
        let _ = writeln!(
            std::io::stderr().lock(),
            "rtr: warning: could not read Codex fork provenance; clear-limit skipped native meter events"
        );
    }
    routing_log::append_batch(paths, &events);
    Ok(())
}

/// The log deduplicates by quota scope before formatting display labels.
#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum ClearScope {
    Meter(MeterKey),
    Profile,
}

impl ClearScope {
    fn label(&self) -> String {
        match self {
            Self::Meter(key) => Scope::meter_label(key),
            Self::Profile => Scope::Profile.label(),
        }
    }
}

/// A profile clear covers every applicable model. Deduplicate scopes because
/// a hold and full meter can share one, and common meters apply to many models.
fn blocking_scopes(limits: &ProfileLimits, tool: &str, now: DateTime<Utc>) -> BTreeSet<ClearScope> {
    let mut models = BTreeSet::new();
    if tool == "codex" {
        // Codex full meters have different success precedence from Claude.
        models.insert(ModelFamily::Codex);
    } else {
        let scopes = limits.meters.iter().map(|meter| &meter.key.scope).chain(
            limits
                .holds
                .iter()
                .filter_map(|hold| match &hold.applies_to {
                    HoldScope::Profile => None,
                    HoldScope::Meter(key) => Some(&key.scope),
                }),
        );
        models.extend(scopes.filter_map(|scope| match scope {
            MeterScope::Model(family) if *family != ModelFamily::Codex => Some(*family),
            _ => None,
        }));
        // With no recognized family, Unknown still evaluates all-model holds
        // and meters without granting applicability to unclassified evidence.
        if models.is_empty() {
            models.insert(ModelFamily::Unknown);
        }
    }
    let mut scopes = BTreeSet::new();
    for model in models {
        if let Evaluation::Out(blockers) = limits::evaluate(limits, model, now) {
            for blocker in blockers {
                scopes.insert(match blocker {
                    Blocker::Meter(meter) => ClearScope::Meter(meter.key),
                    Blocker::Hold(hold) => match hold.applies_to {
                        HoldScope::Profile => ClearScope::Profile,
                        HoldScope::Meter(key) => ClearScope::Meter(key),
                    },
                });
            }
        }
    }
    scopes
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

/// Freeze published filenames before reading fork origins. A concurrent
/// transfer publishes provenance first, so every listed copy can see its
/// origin when the reader later snapshots the index.
pub(crate) fn discover_claude_histories(home: &Path) -> Result<Vec<(SystemTime, PathBuf)>> {
    let mut files = Vec::new();
    for project in crate::conversations::directories_if_exists(&home.join("projects"))? {
        files.extend(crate::conversations::jsonl_files(&project)?);
    }
    let mut histories: Vec<_> = files
        .into_iter()
        .filter_map(|path| Some((path.metadata().ok()?.modified().ok()?, path)))
        .collect();
    histories.sort_unstable_by(|a, b| b.cmp(a));
    Ok(histories)
}

/// Inspect at most four recent top-level histories in the frozen file list.
pub(crate) fn recent_statuses(
    home: &Path,
    tool_name: &str,
    profile: &str,
    origins: &OriginIndex,
    histories: &[(SystemTime, PathBuf)],
) -> Result<Vec<SessionStatus>> {
    let mut statuses = Vec::new();
    for (_, path) in histories {
        if statuses.len() == 4 {
            break;
        }
        let id = if tool_name == "claude" {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        } else {
            crate::codex_history::read_header(path)?.and_then(|header| {
                let payload = &header["payload"];
                if payload["source"].get("subagent").is_some() {
                    return None;
                }
                payload["id"].as_str().map(str::to_string)
            })
        };
        if let Some(id) = id {
            statuses.push(crate::session_status::inspect_with_origin(
                tool_name,
                profile,
                &id,
                path,
                origins.for_session(home, &id),
            )?);
        }
    }
    Ok(statuses)
}
