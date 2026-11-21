//! Per-profile evidence assembled for one command. Native measurements and
//! owned rejections keep separate timestamps and share one evaluator.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

pub const FRESHNESS_MINUTES: i64 = 15;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MeterWindow {
    FiveHour,
    Week,
    Minutes(u64),
    Native(String),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ModelFamily {
    Fable,
    Opus,
    Sonnet,
    Haiku,
    Codex,
    Unknown,
}

impl ModelFamily {
    /// Accepts native Claude aliases and versioned model IDs for scoped meters.
    pub fn from_claude_model(model: &str) -> Self {
        let model = model.to_ascii_lowercase();
        let model = model.strip_suffix("[1m]").unwrap_or(&model);
        let model = model.strip_prefix("claude-").unwrap_or(model);
        let (family, version) = model.split_once('-').unwrap_or((model, ""));
        if model != family
            && !version
                .split('-')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        {
            return Self::Unknown;
        }
        match family {
            "fable" => Self::Fable,
            "opus" => Self::Opus,
            "sonnet" => Self::Sonnet,
            "haiku" => Self::Haiku,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MeterScope {
    AllModels,
    Model(ModelFamily),
    Unclassified(String),
}

/// Identity is independent of observation time, reset precision, and slot.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MeterKey {
    pub window: MeterWindow,
    pub scope: MeterScope,
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Percent(f64);

impl Percent {
    pub fn new(value: f64) -> Option<Self> {
        (0.0..=100.0).contains(&value).then_some(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexSlot {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeterSource {
    ClaudeCache,
    CodexRollout { slot: CodexSlot },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Meter {
    pub key: MeterKey,
    pub used_percent: Percent,
    pub reset_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub source: MeterSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HoldScope {
    Profile,
    Meter(MeterKey),
}

/// An owned rejection with an optional provider reset. Holds with unknown
/// resets have a local retry time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hold {
    pub applies_to: HoldScope,
    pub native_kind: Option<String>,
    pub at: DateTime<Utc>,
    pub reset_at: Option<DateTime<Utc>>,
}

impl Hold {
    pub fn until(&self) -> DateTime<Utc> {
        self.reset_at.unwrap_or_else(|| {
            self.at
                .checked_add_signed(Duration::seconds(
                    crate::session_status::UNKNOWN_HOLD_SECONDS,
                ))
                .unwrap_or(DateTime::<Utc>::MAX_UTC)
        })
    }
}

/// Common success covers profile-wide and all-model evidence. Model-scoped
/// evidence requires a success recorded for that family.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SuccessWatermarks {
    pub common: Option<DateTime<Utc>>,
    pub models: BTreeMap<ModelFamily, DateTime<Utc>>,
}

impl SuccessWatermarks {
    fn for_scope(&self, scope: &MeterScope) -> Option<DateTime<Utc>> {
        match scope {
            MeterScope::AllModels => self.common.max(self.models.values().copied().max()),
            MeterScope::Model(model) => self.models.get(model).copied(),
            MeterScope::Unclassified(_) => None,
        }
    }
}

/// Credits describe paid usage. The subscription evaluator uses the meters.
#[derive(Clone, Debug, PartialEq)]
pub struct CreditInfo {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<f64>,
}

/// Assemble native measurements and persisted hold evidence for one profile
/// and one command. Listing and routing can evaluate the same observations.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfileLimits {
    pub meters: Vec<Meter>,
    pub holds: Vec<Hold>,
    pub success_watermarks: SuccessWatermarks,
    pub cleared_at: Option<DateTime<Utc>>,
    pub credit_info: Option<CreditInfo>,
}

/// The routing meaning of one native reading at the command's decision time.
/// Recovery states retain the sample so listing can explain why it no longer
/// blocks without presenting an old percentage as known headroom.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeterState {
    Fresh,
    Full,
    Expired,
    Cleared,
    Superseded(DateTime<Utc>),
    Unknown,
}

impl ProfileLimits {
    /// Retain active owned rejections across all scopes. Routing applies its
    /// requested-model filter afterward; listing shows every active scope.
    pub fn active_holds(&self, now: DateTime<Utc>) -> impl Iterator<Item = &Hold> {
        self.holds.iter().filter(move |hold| {
            let scope = match &hold.applies_to {
                HoldScope::Profile => &MeterScope::AllModels,
                HoldScope::Meter(key) => &key.scope,
            };
            hold.at <= now
                && hold.until() > now
                && self.cleared_at.is_none_or(|clear| clear < hold.at)
                && self
                    .success_watermarks
                    .for_scope(scope)
                    .is_none_or(|success| success <= hold.at)
        })
    }

    /// Share freshness, recovery, and applicability between routing and listing.
    /// Only a fresh under-full sample establishes room; retired full readings
    /// leave room unknown until another native measurement arrives.
    pub fn meter_state(&self, meter: &Meter, model: ModelFamily, now: DateTime<Utc>) -> MeterState {
        if meter.observed_at > now {
            return MeterState::Unknown;
        }
        let full = meter.used_percent.get() == 100.0;
        if full
            && self
                .cleared_at
                .is_some_and(|clear| clear >= meter.observed_at)
        {
            return MeterState::Cleared;
        }
        let Some(reset) = meter.reset_at else {
            return MeterState::Unknown;
        };
        if reset <= now {
            return MeterState::Expired;
        }
        if reset <= meter.observed_at || !meter.key.scope.applies_to(model) {
            return MeterState::Unknown;
        }
        if full {
            // Codex can report a full window during a turn and still finish
            // that turn. Completion recovers holds, but only reset, manual
            // clear, or a later quota sample can retire its full meter.
            if model != ModelFamily::Codex {
                if let Some(success) = self
                    .success_watermarks
                    .for_scope(&meter.key.scope)
                    .filter(|success| *success > meter.observed_at)
                {
                    return MeterState::Superseded(success);
                }
            }
            MeterState::Full
        } else if now - meter.observed_at <= Duration::minutes(FRESHNESS_MINUTES) {
            MeterState::Fresh
        } else {
            MeterState::Unknown
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Blocker {
    Meter(Meter),
    Hold(Hold),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Evaluation {
    Room,
    Unknown,
    Out(Vec<Blocker>),
}

impl MeterScope {
    pub(crate) fn applies_to(&self, model: ModelFamily) -> bool {
        match self {
            Self::AllModels => true,
            Self::Model(family) => *family == model && model != ModelFamily::Unknown,
            Self::Unclassified(_) => false,
        }
    }
}

/// Evaluate one model at the command's decision time. Missing or expired
/// evidence leaves headroom unknown. Any applicable blocker still wins.
pub fn evaluate(limits: &ProfileLimits, model: ModelFamily, now: DateTime<Utc>) -> Evaluation {
    let mut blockers = Vec::new();
    for hold in limits.active_holds(now) {
        let scope = match &hold.applies_to {
            HoldScope::Profile => &MeterScope::AllModels,
            HoldScope::Meter(key) => &key.scope,
        };
        if scope.applies_to(model) {
            blockers.push(Blocker::Hold(hold.clone()));
        }
    }

    let mut applicable = 0;
    let mut common_five_hour = false;
    let mut common_week = false;
    let mut fable_week = false;
    let mut uncertain = false;
    for meter in limits
        .meters
        .iter()
        .filter(|m| m.key.scope.applies_to(model))
    {
        applicable += 1;
        match limits.meter_state(meter, model, now) {
            MeterState::Full => blockers.push(Blocker::Meter(meter.clone())),
            MeterState::Fresh => {
                match (&meter.key.scope, &meter.key.window) {
                    (MeterScope::AllModels, MeterWindow::FiveHour) => common_five_hour = true,
                    (MeterScope::AllModels, MeterWindow::Week) => common_week = true,
                    (MeterScope::Model(ModelFamily::Fable), MeterWindow::Week) => fable_week = true,
                    _ => {}
                }
                continue;
            }
            _ => {}
        }
        uncertain = true;
    }

    let required = match model {
        ModelFamily::Unknown => false,
        ModelFamily::Codex => applicable > 0,
        ModelFamily::Fable => common_five_hour && common_week && fable_week,
        _ => common_five_hour && common_week,
    };
    if !blockers.is_empty() {
        Evaluation::Out(blockers)
    } else if required && !uncertain {
        Evaluation::Room
    } else {
        Evaluation::Unknown
    }
}
