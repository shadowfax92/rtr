//! Plan one native handoff from a per-command inventory, then commit its slot.
//! Native reads and quota learning happen before scheduling. The rotation lock
//! covers fresh hold reads, selection and native-home preparation, then releases
//! before reports, history copies, routing events or child startup.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::SystemTime,
};

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::config::{Config, RoutingConfig, Tool};
use crate::launch_settings::{single_line, ResolvedSettings};
use crate::limits::{
    self, Blocker, Evaluation, HoldScope, Meter, MeterKey, MeterScope, MeterWindow, ModelFamily,
    ProfileLimits,
};
use crate::paths::Paths;
use crate::quota::{self, HoldIndex, LimitedProfile, NoUsableProfiles};
use crate::routing_log::{
    self, Decision, EventKind, Evidence, EvidenceReason, Label, Operation, RoutingEvent,
    SelectionReason,
};
use crate::selection::{self, Allocation};
use crate::session_status::SessionStatus;
use crate::state::State;
use crate::tool_specs::{self, ToolSpec};
use crate::usage_refresh::{self, Store, Target};

#[derive(Clone, Copy)]
pub(crate) enum Intent<'a> {
    Launch {
        forced: Option<&'a str>,
    },
    Rerun {
        source: &'a str,
    },
    Fork {
        source: &'a str,
        forced: Option<&'a str>,
    },
    Resume {
        source: &'a str,
    },
}

impl<'a> Intent<'a> {
    fn fixed(self) -> Option<&'a str> {
        match self {
            Self::Launch { forced } | Self::Fork { forced, .. } => forced,
            Self::Resume { source } => Some(source),
            Self::Rerun { .. } => None,
        }
    }

    fn source(self) -> Option<&'a str> {
        match self {
            Self::Fork { source, .. } | Self::Resume { source } | Self::Rerun { source } => {
                Some(source)
            }
            Self::Launch { .. } => None,
        }
    }

    fn operation(self) -> Operation {
        match self {
            Self::Launch { .. } | Self::Rerun { .. } => Operation::Launch,
            Self::Fork { .. } => Operation::Fork,
            Self::Resume { .. } => Operation::Resume,
        }
    }

    fn is_launch(self) -> bool {
        matches!(self, Self::Launch { .. } | Self::Rerun { .. })
    }
}

/// Planning inputs shared by preview and real selection.
pub(crate) struct Request<'a> {
    pub tool_name: &'a str,
    pub intent: Intent<'a>,
    pub model: Option<&'a str>,
}

/// An inventory owns the native samples and transcript observations for this
/// command. Committed holds are read separately so concurrent learners and
/// manual clears remain visible at reservation time.
#[derive(Default)]
struct Inventory {
    limits: BTreeMap<String, ProfileLimits>,
    statuses: Vec<SessionStatus>,
}

/// The same frozen native paths feed the meter and hold readers.
struct ProfileFiles<'a> {
    name: &'a str,
    home: PathBuf,
    histories: Vec<(SystemTime, PathBuf)>,
}

impl Inventory {
    fn read(paths: &Paths, tool: &str, homes: &[&str]) -> Result<Self> {
        if homes.is_empty() {
            return Ok(Self::default());
        }
        let warn = |profile: &str, error: &anyhow::Error| {
            eprintln!(
                "rtr: could not inspect histories for {tool}/{}: {}",
                single_line(profile),
                single_line(&format!("{error:#}")),
            );
        };
        let files: Vec<_> = homes
            .iter()
            .map(|&name| {
                let home = paths.profile_home_dir(tool, name);
                let histories = if tool == "codex" {
                    crate::native_usage::codex_histories(&home, |error| warn(name, &error.into()))
                } else {
                    quota::discover_claude_histories(&home).unwrap_or_else(|error| {
                        warn(name, &error);
                        Vec::new()
                    })
                };
                ProfileFiles {
                    name,
                    home,
                    histories,
                }
            })
            .collect();
        // Transfer publishes origins before linking the transcript. Snapshot
        // origins after freezing filenames so a copy can never be mistaken for
        // legacy history by either preflight reader.
        let origins = quota::read_origins(paths)?;
        let observed_at = Utc::now();
        let read = |files: &[ProfileFiles<'_>]| {
            let mut inventory = Self::default();
            for ProfileFiles {
                name,
                home,
                histories,
            } in files
            {
                let native = crate::native_usage::read_discovered(
                    home,
                    tool,
                    &origins,
                    observed_at,
                    histories,
                );
                inventory.limits.insert(
                    (*name).to_owned(),
                    ProfileLimits {
                        meters: native.meters,
                        credit_info: native.credit_info,
                        ..ProfileLimits::default()
                    },
                );
                // History I/O leaves this home's saved holds and readable
                // meters intact. Other candidates still have their own evidence.
                match quota::recent_statuses(home, tool, name, &origins, histories) {
                    Ok(statuses) => inventory.statuses.extend(statuses),
                    Err(error) => warn(name, &error),
                }
            }
            inventory
        };

        if files.len() == 1 {
            return Ok(read(&files));
        }
        std::thread::scope(|scope| {
            let workers: Vec<_> = files
                .chunks(files.len().div_ceil(4))
                .map(|chunk| {
                    let read = &read;
                    scope.spawn(move || read(chunk))
                })
                .collect();
            let mut inventory = Self::default();
            for worker in workers {
                let part = worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("quota preflight worker panicked"))?;
                inventory.limits.extend(part.limits);
                inventory.statuses.extend(part.statuses);
            }
            Ok(inventory)
        })
    }

    fn apply_holds(&mut self, tool: &str, holds: &mut HoldIndex) {
        // Bypassed launches have no inventory entry. Isolated evidence from
        // that name cannot describe the native default home.
        for (profile, limits) in &mut self.limits {
            if let Some(saved) = holds.remove(&(tool.to_owned(), profile.clone())) {
                limits.holds = saved.holds;
                limits.success_watermarks = saved.success_watermarks;
                limits.cleared_at = saved.cleared_at;
            }
        }
    }

    fn merge_codex_refresh(&mut self, store: &Store) {
        let now = Utc::now();
        for (profile, limits) in &mut self.limits {
            store.merge_codex(profile, &mut limits.meters, now);
        }
    }

    fn refresh_stale(
        &mut self,
        paths: &Paths,
        spec: &'static ToolSpec,
        tool: &Tool,
        homes: &[&str],
    ) {
        let now = Utc::now();
        let targets: Vec<_> = homes
            .iter()
            .copied()
            .filter(|profile| {
                self.limits
                    .get(*profile)
                    .is_some_and(|limits| needs_refresh(&limits.meters, now))
                    && paths.profile_home_dir(spec.name, profile).is_dir()
            })
            .map(|profile| Target {
                tool: spec,
                profile,
                command: &tool.command,
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        let batch = usage_refresh::refresh(paths, &targets);
        if spec == &tool_specs::CODEX {
            self.merge_codex_refresh(&batch.store);
        } else {
            // The batch returns only verified cache reads. On a failed probe,
            // keep the sample this preflight already read from the home.
            for (profile, native) in batch.claude {
                if let Some(limits) = self.limits.get_mut(profile) {
                    limits.meters = native.meters;
                }
            }
        }
    }
}

fn needs_refresh(meters: &[Meter], now: DateTime<Utc>) -> bool {
    // Extra Codex buckets describe distinct limits. They can be listed, but
    // a recent extra alone does not establish current subscription usage.
    let relevant = || {
        meters
            .iter()
            .filter(|meter| !matches!(meter.key.scope, MeterScope::Unclassified(_)))
    };
    let latest = relevant().map(|meter| meter.observed_at).max();
    // An older rollout can supply a window omitted from a new App Server
    // reply. That window can prompt another check if it resets after the reply.
    latest.is_none_or(|at| {
        relevant().any(|meter| {
            (meter.observed_at == at && !usage_refresh::is_fresh(meter, now))
                || meter
                    .reset_at
                    .is_some_and(|reset| reset > at && reset <= now)
        })
    })
}

/// Build listing evidence with routing's native inventory and owned-success
/// overlay. Previewing updates only memory, without learning or locking quota.
pub(crate) fn listing_limits(
    paths: &Paths,
    config: &Config,
    refresh: &Store,
) -> Result<BTreeMap<(String, String), ProfileLimits>> {
    let mut inventories = Vec::new();
    let mut statuses = Vec::new();
    for (tool, settings) in &config.tools {
        let homes: Vec<_> = settings
            .profiles
            .iter()
            .filter(|(_, profile)| !profile.bypass || !profile.enabled)
            .map(|(name, _)| name.as_str())
            .collect();
        let mut inventory = Inventory::read(paths, tool, &homes)?;
        if tool == "codex" {
            inventory.merge_codex_refresh(refresh);
        }
        // Enabled bypass launches use the native default home. Keep their
        // persisted isolated holds visible, but do not overlay isolated tails.
        for name in settings.profiles.keys() {
            inventory.limits.entry(name.clone()).or_default();
        }
        statuses.append(&mut inventory.statuses);
        inventories.push((tool, inventory));
    }

    let mut holds = quota::listing_holds(paths, &statuses)?;
    let mut limits = BTreeMap::new();
    for (tool, mut inventory) in inventories {
        inventory.apply_holds(tool, &mut holds);
        limits.extend(
            inventory
                .limits
                .into_iter()
                .map(|(profile, limits)| ((tool.clone(), profile), limits)),
        );
    }
    Ok(limits)
}

impl Request<'_> {
    /// Prepare the native home inside the rotation transaction. Failure
    /// leaves the trial uncommitted, while earlier quota learning persists.
    /// Preview uses the same selection with in-memory hold observations.
    pub(crate) fn reserve<T>(
        &self,
        paths: &Paths,
        tool: &Tool,
        policy: &RoutingConfig,
        dry_run: bool,
        prepare: impl FnOnce(&mut Route) -> Result<T>,
    ) -> Result<T> {
        let fixed = self.intent.fixed();
        let allocation = if let Some(profile) = fixed {
            selection::validate_explicit(self.tool_name, tool, profile)?;
            None
        } else {
            Some(selection::allocation(self.tool_name, tool)?)
        };
        let profiles: Vec<&str> = if let Some(profile) = fixed {
            vec![profile]
        } else {
            allocation
                .iter()
                .flat_map(|allocation| {
                    allocation.weights.iter().filter_map(|(name, weight)| {
                        ((!self.intent.is_launch() || *weight > 0)
                            && (!matches!(self.intent, Intent::Rerun { .. })
                                || !tool.profiles[name].bypass))
                            .then_some(name.as_str())
                    })
                })
                .collect()
        };
        let homes: Vec<_> = profiles
            .iter()
            .copied()
            .filter(|profile| !self.intent.is_launch() || !tool.profiles[*profile].bypass)
            .collect();
        let mut inventory = Inventory::read(paths, self.tool_name, &homes)?;
        if self.tool_name == "codex" {
            inventory.merge_codex_refresh(&Store::load(paths));
        }
        // Probe before the rotation transaction. A limit-at-launch rerun
        // reads the first batch's evidence and the newly learned hold.
        if policy.refresh
            && !dry_run
            && fixed.is_none()
            && !matches!(self.intent, Intent::Rerun { .. })
        {
            inventory.refresh_stale(paths, tool_specs::get(self.tool_name)?, tool, &homes);
        }
        let preview_holds = if dry_run {
            Some(quota::preview_holds(paths, &inventory.statuses)?)
        } else {
            quota::learn(paths, &inventory.statuses)?;
            None
        };
        let mut unavailable = None;
        let run = |state: &mut State| {
            let mut holds = preview_holds.map_or_else(|| quota::read_holds(paths), Ok)?;
            inventory.apply_holds(self.tool_name, &mut holds);
            let now = Utc::now();
            let requested = Pass::evaluate(self.tool_name, self.model, &profiles, &inventory, now);

            let fallback = (fixed.is_none()
                && !matches!(self.intent, Intent::Rerun { .. })
                && self.tool_name == "claude"
                && policy.claude.auto_drop
                && requested.family == ModelFamily::Fable
                && requested.admitted() == 0
                && !profiles.is_empty())
            .then(|| {
                Pass::evaluate(
                    self.tool_name,
                    Some(&policy.claude.fallback_model),
                    &profiles,
                    &inventory,
                    now,
                )
            });
            // With zero Fable admissions, every Opus admission must have
            // lost a Fable-specific blocker. A common blocker survives both.
            let drop = fallback.as_ref().filter(|pass| pass.admitted() > 0);
            let selected = drop.unwrap_or(&requested);

            let mut trial = state.clone();
            let mut eligible = selected.eligible();
            if let Intent::Rerun { source } = self.intent {
                // The failed home cannot be the retry destination, even if a
                // concurrent clear or a scoped hold leaves it eligible again.
                eligible.remove(source);
            } else {
                selection::exclude_fork_source(&mut eligible, self.intent.source());
            }
            let (destination, reason) = if let Some(profile) = fixed {
                (profile.to_owned(), SelectionReason::ExactHome)
            } else if let Some(allocation) = allocation.as_ref().filter(|_| !eligible.is_empty()) {
                let (destination, reason) =
                    selection::select_from(self.tool_name, allocation, &eligible, &mut trial)?;
                let reason = if matches!(self.intent, Intent::Rerun { .. }) {
                    SelectionReason::RerunAfterLimitBeforeModelActivity
                } else if self.intent.source() == Some(destination.as_str()) {
                    SelectionReason::SoleUsableSource
                } else {
                    reason
                };
                (destination, reason)
            } else {
                let error = requested.unavailable(fallback.as_ref());
                let reason = if profiles.is_empty() {
                    SelectionReason::EmptyPool
                } else {
                    SelectionReason::NoUsableProfiles
                };
                let mut decision = self.decision(&requested, None, reason, None, &inventory);
                decision.earliest_reset = error.earliest_reset;
                if !dry_run {
                    unavailable = Some(RoutingEvent::new(
                        now,
                        operation_id(),
                        log_tool(self.tool_name),
                        EventKind::Unavailable(decision),
                    ));
                }
                return Err(error.into());
            };

            let decision = self.decision(
                selected,
                Some(&destination),
                reason,
                allocation.as_ref(),
                &inventory,
            );
            let mut route = Route::new(self.tool_name, destination, decision, now);
            route.model_drop = drop.map(|_| ModelDrop {
                model: policy.claude.fallback_model.clone(),
                blocked: requested.entries.len(),
                evidence: requested.evidence(&inventory, None),
            });
            let prepared = prepare(&mut route)?;
            *state = trial;
            Ok((prepared, route))
        };
        let result = if dry_run || fixed.is_some() {
            let mut state = if fixed.is_some() {
                State::default()
            } else {
                State::load(&paths.state_file())?
            };
            run(&mut state)
        } else {
            State::update_locked(&paths.state_file(), run)
        };

        if let Some(event) = unavailable {
            routing_log::append_batch(paths, &[event]);
        }
        let (prepared, route) = result?;
        if !dry_run {
            route.record(paths, None);
        }
        Ok(prepared)
    }

    fn decision(
        &self,
        pass: &Pass<'_>,
        destination: Option<&str>,
        reason: SelectionReason,
        allocation: Option<&Allocation>,
        inventory: &Inventory,
    ) -> Decision {
        Decision {
            operation: self.intent.operation(),
            source: self.intent.source().map(Label::from),
            destination: destination.map(Label::from),
            requested_model: self.model.map(Label::from),
            effective_model: destination.and_then(|_| self.model.map(Label::from)),
            reason,
            admitted: pass.admitted(),
            blocked: pass.entries.len() - pass.admitted(),
            share_percent: destination
                .and_then(|name| allocation?.percent(name))
                .and_then(|percent| routing_log::Percent::new(percent).ok()),
            evidence: pass.evidence(inventory, destination),
            earliest_reset: None,
        }
    }

    /// Exact resume keeps its original identity even when quota metadata is
    /// unreadable. Log the fixed route with unknown headroom in that case.
    pub(crate) fn record_resume(
        &self,
        paths: &Paths,
        id: Option<&str>,
        effective_model: Option<&str>,
    ) {
        let profile = self.intent.fixed().expect("resume has an original home");
        let profiles = [profile];
        let inventory = Inventory::read(paths, self.tool_name, &profiles)
            .and_then(|mut inventory| {
                if self.tool_name == "codex" {
                    inventory.merge_codex_refresh(&Store::load(paths));
                }
                let mut holds = quota::preview_holds(paths, &inventory.statuses)?;
                inventory.apply_holds(self.tool_name, &mut holds);
                Ok(inventory)
            })
            .unwrap_or_else(|error| {
                eprintln!(
                    "rtr: could not check usage limit for {}/{}: {}",
                    self.tool_name,
                    single_line(profile),
                    single_line(&format!("{error:#}"))
                );
                Inventory::default()
            });
        let now = Utc::now();
        let pass = Pass::evaluate(self.tool_name, effective_model, &profiles, &inventory, now);
        let mut decision = self.decision(
            &pass,
            Some(profile),
            SelectionReason::ExactHome,
            None,
            &inventory,
        );
        decision.effective_model = effective_model.map(Label::from);
        Route::new(self.tool_name, profile.to_owned(), decision, now).record(paths, id);
    }
}

fn family(tool: &str, model: Option<&str>) -> ModelFamily {
    if tool == "codex" {
        ModelFamily::Codex
    } else {
        model.map_or(ModelFamily::Unknown, ModelFamily::from_claude_model)
    }
}

/// Evaluate one model across the routable pool. Unknown evidence still
/// permits a handoff, so a missing reading cannot trigger an Opus drop.
struct Pass<'a> {
    family: ModelFamily,
    now: DateTime<Utc>,
    entries: BTreeMap<&'a str, Evaluation>,
}

impl<'a> Pass<'a> {
    fn evaluate(
        tool: &str,
        model: Option<&str>,
        profiles: &[&'a str],
        inventory: &Inventory,
        now: DateTime<Utc>,
    ) -> Self {
        let family = family(tool, model);
        let unknown = ProfileLimits::default();
        Self {
            family,
            now,
            entries: profiles
                .iter()
                .map(|&name| {
                    let limits = inventory.limits.get(name).unwrap_or(&unknown);
                    (name, limits::evaluate(limits, family, now))
                })
                .collect(),
        }
    }

    fn admitted(&self) -> usize {
        self.entries
            .values()
            .filter(|evaluation| !matches!(evaluation, Evaluation::Out(_)))
            .count()
    }

    fn eligible(&self) -> BTreeSet<&'a str> {
        self.entries
            .iter()
            .filter_map(|(&name, evaluation)| {
                (!matches!(evaluation, Evaluation::Out(_))).then_some(name)
            })
            .collect()
    }

    fn unavailable(&self, fallback: Option<&Self>) -> NoUsableProfiles {
        let limited: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(&profile, evaluation)| {
                let primary_release = release(evaluation)?;
                let until = fallback
                    .and_then(|pass| release(&pass.entries[profile]))
                    .map_or(primary_release, |secondary_release| {
                        primary_release.min(secondary_release)
                    });
                Some(LimitedProfile {
                    profile: profile.into(),
                    until,
                })
            })
            .collect();
        NoUsableProfiles {
            schema_version: 1,
            destination: None,
            earliest_reset: limited.iter().map(|limited| limited.until).min(),
            limited,
        }
    }

    fn evidence(&self, inventory: &Inventory, selected: Option<&str>) -> Vec<Evidence> {
        let mut evidence = Vec::new();
        for (&profile, evaluation) in &self.entries {
            match evaluation {
                Evaluation::Out(blockers) => {
                    evidence.extend(blockers.iter().map(|blocker| match blocker {
                        Blocker::Meter(meter) => {
                            meter_evidence(profile, meter, EvidenceReason::Full)
                        }
                        Blocker::Hold(hold) => Evidence {
                            profile: profile.into(),
                            scope: match &hold.applies_to {
                                HoldScope::Profile => "profile".into(),
                                HoldScope::Meter(key) => meter_label(key).into(),
                            },
                            native_kind: hold.native_kind.as_deref().map(Label::from),
                            used_percent: None,
                            observed_at: Some(hold.at),
                            reset_at: hold.reset_at,
                            hold_until: Some(hold.until()),
                            reason: EvidenceReason::Held,
                        },
                    }))
                }
                Evaluation::Room | Evaluation::Unknown if selected == Some(profile) => {
                    if matches!(evaluation, Evaluation::Unknown) {
                        evidence.push(Evidence {
                            profile: profile.into(),
                            scope: "subscription".into(),
                            native_kind: None,
                            used_percent: None,
                            observed_at: None,
                            reset_at: None,
                            hold_until: None,
                            reason: EvidenceReason::Unknown,
                        });
                    }
                    if let Some(limits) = inventory.limits.get(profile) {
                        evidence.extend(
                            limits
                                .meters
                                .iter()
                                .filter(|meter| meter.key.scope.applies_to(self.family))
                                .map(|meter| {
                                    let reason = match meter.reset_at {
                                        Some(reset) if reset <= self.now => EvidenceReason::Expired,
                                        None => EvidenceReason::Unknown,
                                        _ if self.now - meter.observed_at
                                            > chrono::Duration::minutes(
                                                limits::FRESHNESS_MINUTES,
                                            ) =>
                                        {
                                            EvidenceReason::Stale
                                        }
                                        _ => EvidenceReason::Observed,
                                    };
                                    meter_evidence(profile, meter, reason)
                                }),
                        );
                    }
                }
                _ => {}
            }
        }
        evidence
    }
}

fn release(evaluation: &Evaluation) -> Option<DateTime<Utc>> {
    let Evaluation::Out(blockers) = evaluation else {
        return None;
    };
    blockers
        .iter()
        .filter_map(|blocker| match blocker {
            Blocker::Meter(meter) => meter.reset_at,
            Blocker::Hold(hold) => Some(hold.until()),
        })
        .max()
}

fn meter_evidence(profile: &str, meter: &Meter, reason: EvidenceReason) -> Evidence {
    Evidence {
        profile: profile.into(),
        scope: meter_label(&meter.key).into(),
        native_kind: None,
        used_percent: routing_log::Percent::new(meter.used_percent.get()).ok(),
        observed_at: Some(meter.observed_at),
        reset_at: meter.reset_at,
        hold_until: None,
        reason,
    }
}

fn meter_label(key: &MeterKey) -> String {
    let window = match &key.window {
        MeterWindow::FiveHour => "5h".to_owned(),
        MeterWindow::Week => "weekly".to_owned(),
        MeterWindow::Minutes(minutes) => format!("{minutes}m"),
        MeterWindow::Native(label) => label.clone(),
    };
    let scope = match &key.scope {
        MeterScope::AllModels => "all",
        MeterScope::Model(ModelFamily::Fable) => "Fable",
        MeterScope::Model(ModelFamily::Opus) => "Opus",
        MeterScope::Model(ModelFamily::Sonnet) => "Sonnet",
        MeterScope::Model(ModelFamily::Haiku) => "Haiku",
        MeterScope::Model(ModelFamily::Codex) => "Codex",
        MeterScope::Model(ModelFamily::Unknown) => "unknown",
        MeterScope::Unclassified(label) => label,
    };
    format!("{window} {scope}")
}

fn log_tool(tool: &str) -> routing_log::Tool {
    if tool == "claude" {
        routing_log::Tool::Claude
    } else {
        routing_log::Tool::Codex
    }
}

fn operation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

struct ModelDrop {
    model: String,
    blocked: usize,
    evidence: Vec<Evidence>,
}

/// The committed route carries one operation ID through the fork copy/report
/// boundary, where a second event can finally name the prepared native ID.
pub(crate) struct Route {
    pub profile: String,
    pub operation_id: String,
    decision: Decision,
    tool: routing_log::Tool,
    at: DateTime<Utc>,
    model_drop: Option<ModelDrop>,
}

impl Route {
    fn new(tool: &str, profile: String, decision: Decision, at: DateTime<Utc>) -> Self {
        Self {
            profile,
            operation_id: operation_id(),
            decision,
            tool: log_tool(tool),
            at,
            model_drop: None,
        }
    }

    pub(crate) fn apply_settings(&mut self, settings: &mut ResolvedSettings) {
        if let Some(drop) = &self.model_drop {
            settings.replace_claude_model(&drop.model);
        }
        // Codex cache finalization can change the inherited model, but both
        // the original and finalized model use the common Codex family.
        self.decision.effective_model = settings.values.model.as_deref().map(Label::from);
    }

    fn event(&self, kind: EventKind) -> RoutingEvent {
        RoutingEvent::new(self.at, self.operation_id.as_str(), self.tool, kind)
    }

    fn record(self, paths: &Paths, resume_id: Option<&str>) {
        let tool = self.tool.as_str();
        let profile = single_line(&self.profile);
        let mut events = Vec::new();
        if let Some(drop) = &self.model_drop {
            eprintln!(
                "rtr: Fable unavailable on every eligible profile. Using {} in {tool}/{profile}.",
                single_line(&drop.model)
            );
            events.push(
                self.event(EventKind::ModelDrop {
                    operation: self.decision.operation,
                    source: self.decision.source.clone(),
                    destination: self.decision.destination.clone(),
                    requested_model: self
                        .decision
                        .requested_model
                        .clone()
                        .expect("drop has a requested model"),
                    effective_model: drop.model.as_str().into(),
                    admitted: self.decision.admitted,
                    blocked: drop.blocked,
                    evidence: drop.evidence.clone(),
                }),
            );
        }
        match self.decision.reason {
            SelectionReason::ExactHome => {
                for evidence in &self.decision.evidence {
                    if let Some(until) = evidence.hold_until.or(evidence.reset_at) {
                        if matches!(evidence.reason, EvidenceReason::Full | EvidenceReason::Held) {
                            let action = if self.decision.operation == Operation::Resume {
                                "resuming in the original home"
                            } else {
                                "obeying explicit profile"
                            };
                            let end =
                                if evidence.hold_until.is_some() && evidence.reset_at.is_none() {
                                    "retry after"
                                } else {
                                    "reset"
                                };
                            eprintln!("rtr: warning: {tool}/{profile} is limited until {}; {}, {end}; {action}",
                                crate::output::local_time(until), evidence.scope);
                        }
                    }
                }
                if self.decision.blocked > 0 {
                    if let Some(id) = resume_id {
                        eprintln!(
                            "rtr: rotate a copy with: rtr fork {} -t {tool} -p {}",
                            crate::runner::shell_quote(id),
                            crate::runner::shell_quote(&self.profile)
                        );
                    }
                }
            }
            SelectionReason::SoleUsableSource => {
                let model = self
                    .decision
                    .effective_model
                    .as_ref()
                    .map_or("the requested model", Label::as_str);
                eprintln!("rtr: forking in place on {tool}/{profile} (no other profile has room for {model})");
            }
            _ => {}
        }
        events.push(self.event(EventKind::Route(self.decision.clone())));
        routing_log::append_batch(paths, &events);
    }
}

pub(crate) fn record_fork_prepared(
    paths: &Paths,
    tool: &str,
    operation_id: &str,
    source: &str,
    destination: &str,
    copied_id: Option<&str>,
) {
    routing_log::append_batch(
        paths,
        &[RoutingEvent::new(
            Utc::now(),
            operation_id,
            log_tool(tool),
            EventKind::ForkPrepared {
                source: source.into(),
                destination: destination.into(),
                in_place: source == destination,
                prepared_id: copied_id.map(Label::from),
            },
        )],
    );
}
