//! `quota.json` has one authoritative scoped entry per profile. The flat
//! fields beside it are a compatibility projection for pre-scoped binaries.

use super::{read, HoldEvidence};
use crate::{
    limits::{Hold, HoldScope, MeterKey, MeterScope, MeterWindow, ModelFamily, SuccessWatermarks},
    session_status::{Limit, SessionStatus},
};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

const SCHEMA_VERSION: u8 = 2;

// The public inventory types have no on-disk contract. Keep serde here so
// changes to quota.json stay within the store that owns its migration.
#[derive(Serialize, Deserialize)]
#[serde(remote = "ModelFamily", rename_all = "snake_case")]
enum ModelFamilyFormat {
    Fable,
    Opus,
    Sonnet,
    Haiku,
    Codex,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
struct Family(#[serde(with = "ModelFamilyFormat")] ModelFamily);

/// Only transcript scopes with an explicit applicability rule are stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Scope {
    Profile,
    CommonFiveHour,
    CommonWeek,
    ModelWeek(#[serde(with = "ModelFamilyFormat")] ModelFamily),
}

impl Scope {
    fn from_limit(tool: &str, native_kind: Option<&str>, family: Option<ModelFamily>) -> Self {
        match (tool, native_kind, family) {
            ("claude", Some("five_hour"), _) => Self::CommonFiveHour,
            ("claude", Some("seven_day"), _) => Self::CommonWeek,
            ("claude", Some("weekly_scoped"), Some(family)) => Self::ModelWeek(family),
            _ => Self::Profile,
        }
    }

    fn inventory(self) -> HoldScope {
        let (window, scope) = match self {
            Self::Profile => return HoldScope::Profile,
            Self::CommonFiveHour => (MeterWindow::FiveHour, MeterScope::AllModels),
            Self::CommonWeek => (MeterWindow::Week, MeterScope::AllModels),
            Self::ModelWeek(family) => (MeterWindow::Week, MeterScope::Model(family)),
        };
        HoldScope::Meter(MeterKey { window, scope })
    }

    pub(super) fn label(self) -> String {
        match self.inventory() {
            HoldScope::Profile => "profile".into(),
            HoldScope::Meter(key) => Self::meter_label(&key),
        }
    }

    /// Hold transitions and native-meter clears name the same window/scope.
    pub(super) fn meter_label(key: &MeterKey) -> String {
        let window = match &key.window {
            MeterWindow::FiveHour => "5h".into(),
            MeterWindow::Week => "weekly".into(),
            MeterWindow::Minutes(minutes) => format!("{minutes}m"),
            MeterWindow::Native(label) => label.clone(),
        };
        let scope = match &key.scope {
            MeterScope::AllModels => "all".into(),
            MeterScope::Model(family) => format!("{family:?}"),
            MeterScope::Unclassified(label) => label.clone(),
        };
        format!("{window} {scope}")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct Rejection {
    pub(super) scope: Scope,
    native_kind: Option<String>,
    at: DateTime<Utc>,
    reset_at: Option<DateTime<Utc>>,
}

impl Rejection {
    fn from_limit(tool: &str, limit: &Limit, native_kind: Option<String>) -> Self {
        Self {
            scope: Scope::from_limit(tool, native_kind.as_deref(), limit.family),
            native_kind,
            at: limit.at,
            reset_at: limit.resets_at,
        }
    }

    pub(super) fn hold(&self) -> Hold {
        Hold {
            applies_to: self.scope.inventory(),
            native_kind: self.native_kind.clone(),
            at: self.at,
            reset_at: self.reset_at,
        }
    }

    fn legacy_limit(&self) -> Limit {
        Limit {
            kind: match self.native_kind.as_deref() {
                Some("seven_day") => "weekly",
                Some(native) => native,
                None => "usage",
            }
            .into(),
            at: self.at,
            resets_at: self.reset_at,
            active: true,
            message: String::new(),
            native_kind: None,
            family: None,
        }
    }

    fn supersedes(&self, previous: &Self) -> bool {
        // A later observation owns the scope. For the same observation prefer
        // a concrete reset to a retry estimate and retain the later known reset.
        (&self.at, &self.reset_at, &self.native_kind)
            > (&previous.at, &previous.reset_at, &previous.native_kind)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
struct Scoped {
    holds: Vec<Rejection>,
    common_success_at: Option<DateTime<Utc>>,
    model_success_at: BTreeMap<Family, DateTime<Utc>>,
}

/// The merge retains the latest rejection in each scope until recovery. An
/// expired rejection still bars older observations from replacing it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Entry {
    scoped: Scoped,
    pub(super) cleared_at: Option<DateTime<Utc>>,
}

fn common_success(scoped: &Scoped) -> Option<DateTime<Utc>> {
    scoped
        .common_success_at
        .max(scoped.model_success_at.values().copied().max())
}

fn success_for(scoped: &Scoped, scope: Scope) -> Option<DateTime<Utc>> {
    match scope {
        Scope::ModelWeek(family) => scoped.model_success_at.get(&Family(family)).copied(),
        _ => common_success(scoped),
    }
}

fn recovered(
    clear: Option<DateTime<Utc>>,
    success: Option<DateTime<Utc>>,
    at: DateTime<Utc>,
) -> bool {
    clear.is_some_and(|clear| clear >= at) || success.is_some_and(|success| success > at)
}

impl Entry {
    pub(super) fn rejections(&self) -> &[Rejection] {
        &self.scoped.holds
    }

    pub(super) fn success_at(&self, scope: Scope) -> Option<DateTime<Utc>> {
        success_for(&self.scoped, scope)
    }

    pub(super) fn observe(&mut self, status: &SessionStatus) {
        self.scoped.common_success_at = self.scoped.common_success_at.max(status.successes.common);
        for (&family, &at) in &status.successes.models {
            let latest = self
                .scoped
                .model_success_at
                .entry(Family(family))
                .or_insert(at);
            *latest = (*latest).max(at);
        }

        let mut holds = std::mem::take(&mut self.scoped.holds);
        holds.retain(|hold| !recovered(self.cleared_at, self.success_at(hold.scope), hold.at));
        if let Some(limit) = &status.limit {
            let rejection = Rejection::from_limit(&status.tool, limit, limit.native_kind.clone());
            if !recovered(
                self.cleared_at,
                self.success_at(rejection.scope),
                rejection.at,
            ) {
                if let Some(previous) = holds.iter_mut().find(|hold| hold.scope == rejection.scope)
                {
                    if rejection.supersedes(previous) {
                        *previous = rejection;
                    }
                } else {
                    holds.push(rejection);
                }
            }
        }
        self.scoped.holds = holds;
    }

    pub(super) fn clear(&mut self, at: DateTime<Utc>) {
        self.cleared_at = self.cleared_at.max(Some(at));
        self.scoped.holds.clear();
    }

    fn latest_active(
        &self,
        now: DateTime<Utc>,
        applicable: impl Fn(Scope) -> bool,
    ) -> Option<(&Rejection, DateTime<Utc>)> {
        self.rejections()
            .iter()
            .filter(|hold| {
                hold.at <= now
                    && applicable(hold.scope)
                    && !recovered(self.cleared_at, self.success_at(hold.scope), hold.at)
            })
            .map(|hold| (hold, hold.hold().until()))
            .filter(|(_, until)| *until > now)
            .max_by_key(|(hold, until)| (*until, hold.at))
    }

    pub(super) fn evidence(&self) -> HoldEvidence {
        HoldEvidence {
            holds: self.rejections().iter().map(Rejection::hold).collect(),
            success_watermarks: SuccessWatermarks {
                common: self.scoped.common_success_at,
                models: self
                    .scoped
                    .model_success_at
                    .iter()
                    .map(|(family, at)| (family.0, *at))
                    .collect(),
            },
            cleared_at: self.cleared_at,
        }
    }
}

type Profiles<T> = BTreeMap<String, BTreeMap<String, T>>;

#[derive(Default, Serialize, Deserialize)]
struct DiskStore {
    schema_version: Option<u8>,
    entries: Profiles<DiskEntry>,
}

#[derive(Serialize, Deserialize)]
struct DiskEntry {
    limit: Option<Limit>,
    native_kind: Option<String>,
    success_at: Option<DateTime<Utc>>,
    cleared_at: Option<DateTime<Utc>>,
    scoped: Option<Scoped>,
}

impl DiskEntry {
    fn into_entry(self, tool: &str) -> Entry {
        // Older writers ignore the version and scoped fields. If one rewrites
        // the file, only its surviving single hold and common watermark import.
        Entry {
            scoped: self.scoped.unwrap_or_else(|| Scoped {
                holds: self
                    .limit
                    .iter()
                    .map(|limit| Rejection::from_limit(tool, limit, self.native_kind.clone()))
                    .collect(),
                common_success_at: self.success_at,
                ..Scoped::default()
            }),
            cleared_at: self.cleared_at,
        }
    }

    fn from_entry(entry: &Entry, now: DateTime<Utc>) -> Self {
        let projected = entry.latest_active(now, |_| true).map(|(hold, _)| hold);
        // The old reader treats every success as profile-wide. Hide a later
        // other-family success from it when that would erase its projected hold.
        let success_at =
            common_success(&entry.scoped).filter(|at| projected.is_none_or(|hold| *at <= hold.at));
        Self {
            limit: projected.map(Rejection::legacy_limit),
            native_kind: projected.and_then(|hold| hold.native_kind.clone()),
            success_at,
            cleared_at: entry.cleared_at,
            scoped: Some(entry.scoped.clone()),
        }
    }
}

pub(super) struct Store {
    pub(super) entries: Profiles<Entry>,
}

impl Store {
    pub(super) fn read(path: &Path) -> Result<Self> {
        let disk: DiskStore = read(path)?;
        if let Some(version) = disk
            .schema_version
            .filter(|version| *version != SCHEMA_VERSION)
        {
            bail!(
                "unsupported quota schema version {version} in {}",
                path.display()
            );
        }
        if disk.schema_version.is_some()
            && disk
                .entries
                .values()
                .flat_map(BTreeMap::values)
                .any(|entry| entry.scoped.is_none())
        {
            bail!("missing scoped quota entry in {}", path.display());
        }
        Ok(Self {
            entries: disk
                .entries
                .into_iter()
                .map(|(tool, profiles)| {
                    let profiles = profiles
                        .into_iter()
                        .map(|(profile, entry)| (profile, entry.into_entry(&tool)))
                        .collect();
                    (tool, profiles)
                })
                .collect(),
        })
    }

    pub(super) fn entry(&mut self, tool: &str, profile: &str) -> &mut Entry {
        self.entries
            .entry(tool.into())
            .or_default()
            .entry(profile.into())
            .or_default()
    }

    pub(super) fn write(&self, path: &Path, now: DateTime<Utc>) -> Result<()> {
        let disk = DiskStore {
            schema_version: Some(SCHEMA_VERSION),
            entries: self
                .entries
                .iter()
                .map(|(tool, profiles)| {
                    let profiles = profiles
                        .iter()
                        .map(|(profile, entry)| {
                            (profile.clone(), DiskEntry::from_entry(entry, now))
                        })
                        .collect();
                    (tool.clone(), profiles)
                })
                .collect(),
        };
        crate::file_lock::write_private_atomic(path, &serde_json::to_string(&disk)?)
    }
}
