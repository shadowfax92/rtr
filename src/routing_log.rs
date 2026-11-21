//! Private decision history for people, separate from child outcomes in `usage.jsonl`.
//!
//! Producers append typed projections after committing their own state. This log
//! never participates in routing, and readers need only the log, not native homes.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::paths::Paths;

const VERSION: u8 = 1;
const LABEL_BYTES: usize = 512;
const BLOCK_BYTES: usize = 16 * 1024;
const RECORD_BYTES: usize = 64 * 1024;
static APPEND_WARNING_SHOWN: AtomicBool = AtomicBool::new(false);

/// A bounded diagnostic label, never argv, native errors, or a session object.
/// Controls become visible escapes before persistence and terminal rendering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Label(String);

impl Label {
    pub fn new(value: impl AsRef<str>) -> Self {
        let mut label = String::new();
        let mut truncated = false;
        for character in value.as_ref().chars() {
            let text = if unsafe_label_char(character) || character == '\\' {
                character.escape_default().to_string()
            } else {
                character.to_string()
            };
            if label.len() + text.len() > LABEL_BYTES {
                truncated = true;
                break;
            }
            label.push_str(&text);
        }
        if truncated {
            while label.len() + '…'.len_utf8() > LABEL_BYTES {
                label.pop();
            }
            label.push('…');
        }
        Self(label)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Label {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for Label {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Display for Label {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Label {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let stored = String::deserialize(deserializer)?;
        if stored.len() > LABEL_BYTES || stored.chars().any(unsafe_label_char) {
            return Err(serde::de::Error::custom("invalid routing label"));
        }
        Ok(Self(stored))
    }
}

fn unsafe_label_char(character: char) -> bool {
    character.is_control()
        || matches!(character, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// A finite percentage in the native subscription window, distinct from counts.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Percent(f64);

impl Percent {
    pub fn new(value: f64) -> Result<Self> {
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            bail!("percentage must be finite and between 0 and 100");
        }
        Ok(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Percent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::new(f64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Claude,
    Codex,
}

impl Tool {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Launch,
    Fork,
    Resume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionReason {
    Weighted,
    RoundRobin,
    ExactHome,
    SoleUsableSource,
    NoUsableProfiles,
    EmptyPool,
}

impl SelectionReason {
    fn text(self) -> &'static str {
        match self {
            Self::Weighted => "weighted",
            Self::RoundRobin => "round-robin",
            Self::ExactHome => "exact home",
            Self::SoleUsableSource => "sole usable source",
            Self::NoUsableProfiles => "no usable profiles",
            Self::EmptyPool => "empty pool",
        }
    }

    fn unavailable(self) -> bool {
        matches!(self, Self::NoUsableProfiles | Self::EmptyPool)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearReason {
    Recovery,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceReason {
    Observed,
    Full,
    Held,
    Unknown,
    Missing,
    Stale,
    Expired,
}

/// An allowlisted projection of a controlling meter, hold, or unknown reading.
/// PR 6 maps inventory types here; the log owns no dependency on meter storage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub profile: Label,
    pub scope: Label,
    pub native_kind: Option<Label>,
    pub used_percent: Option<Percent>,
    pub observed_at: Option<DateTime<Utc>>,
    pub reset_at: Option<DateTime<Utc>>,
    pub hold_until: Option<DateTime<Utc>>,
    pub reason: EvidenceReason,
}

/// Shared projection of one launch, fork, or fixed-home resume decision.
/// A route requires a destination; an unavailable decision requires null and an
/// unavailable reason. Append and read validate these cross-field invariants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub operation: Operation,
    pub source: Option<Label>,
    pub destination: Option<Label>,
    pub requested_model: Option<Label>,
    pub effective_model: Option<Label>,
    pub reason: SelectionReason,
    pub admitted: usize,
    pub blocked: usize,
    pub share_percent: Option<Percent>,
    pub evidence: Vec<Evidence>,
    pub earliest_reset: Option<DateTime<Utc>>,
}

/// Event-specific fields are flat beside the envelope in JSON. Only these
/// diagnostic fields can cross the persistence boundary; native objects cannot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    HoldLearned {
        profile: Label,
        scope: Label,
        native_kind: Option<Label>,
        observed_at: DateTime<Utc>,
        reset_at: Option<DateTime<Utc>>,
        hold_until: DateTime<Utc>,
    },
    HoldCleared {
        profile: Label,
        scope: Label,
        observed_at: DateTime<Utc>,
        reason: ClearReason,
    },
    Route(Decision),
    Unavailable(Decision),
    ModelDrop {
        operation: Operation,
        source: Option<Label>,
        destination: Option<Label>,
        requested_model: Label,
        effective_model: Label,
        admitted: usize,
        blocked: usize,
        evidence: Vec<Evidence>,
    },
    ForkPrepared {
        source: Label,
        destination: Label,
        in_place: bool,
        prepared_id: Option<Label>,
    },
}

/// A versioned decision fact. Related facts share an operation ID, while `at`
/// records their event time rather than implying append order or child success.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingEvent {
    version: u8,
    pub at: DateTime<Utc>,
    pub operation_id: Label,
    pub tool: Tool,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl RoutingEvent {
    pub fn new(
        at: DateTime<Utc>,
        operation_id: impl Into<Label>,
        tool: Tool,
        kind: EventKind,
    ) -> Self {
        Self {
            version: VERSION,
            at,
            operation_id: operation_id.into(),
            tool,
            kind,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.version != VERSION || self.operation_id.as_str().is_empty() {
            bail!("invalid routing event envelope");
        }
        match &self.kind {
            EventKind::Route(decision)
                if decision.destination.is_none() || decision.reason.unavailable() =>
            {
                bail!("route requires a destination and selection reason");
            }
            EventKind::Unavailable(decision)
                if decision.destination.is_some() || !decision.reason.unavailable() =>
            {
                bail!("unavailable requires a null destination and unavailable reason");
            }
            EventKind::ForkPrepared {
                source,
                destination,
                in_place,
                prepared_id,
            } if *in_place != (source == destination) || (*in_place && prepared_id.is_some()) => {
                bail!("prepared fork identity does not match its preparation mode");
            }
            _ => {}
        }
        Ok(())
    }

    fn matches_profile(&self, profile: &Label) -> bool {
        match &self.kind {
            EventKind::HoldLearned { profile: own, .. }
            | EventKind::HoldCleared { profile: own, .. } => own == profile,
            EventKind::Route(decision) | EventKind::Unavailable(decision) => {
                decision.source.as_ref() == Some(profile)
                    || decision.destination.as_ref() == Some(profile)
            }
            EventKind::ModelDrop {
                source,
                destination,
                ..
            } => source.as_ref() == Some(profile) || destination.as_ref() == Some(profile),
            EventKind::ForkPrepared {
                source,
                destination,
                ..
            } => source == profile || destination == profile,
        }
    }
}

/// Append a decision's events together after releasing its state/hold lock.
/// The dedicated log lock protects whole batches without ordering observation
/// times. A failed append warns once per process and cannot cancel a handoff.
pub fn append_batch(paths: &Paths, events: &[RoutingEvent]) {
    if events.is_empty() {
        return;
    }
    if append(paths, events).is_err() && !APPEND_WARNING_SHOWN.swap(true, Ordering::Relaxed) {
        // A closed stderr must not turn an optional append into a panic.
        let _ = writeln!(
            std::io::stderr().lock(),
            "rtr: warning: routing log could not be saved; continuing"
        );
    }
}

fn append(paths: &Paths, events: &[RoutingEvent]) -> Result<()> {
    let mut batch = Vec::new();
    for event in events {
        event.validate()?;
        let record = serde_json::to_vec(event).context("serializing routing event")?;
        if record.len() > RECORD_BYTES {
            bail!("routing event exceeds the record size limit");
        }
        batch.extend(record);
        batch.push(b'\n');
    }

    crate::paths::ensure_private_dir(&paths.state_dir)?;
    let path = paths.state_dir.join("routing.jsonl");
    crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(&path), || {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .context("opening routing log")?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("routing log must be a regular file");
        }
        if metadata.permissions().mode() & 0o7777 != 0o600 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        if metadata.len() > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                // A complete JSON object without its newline is still torn.
                // Poison it before delimiting, so later appends cannot revive
                // a record which an earlier snapshot correctly ignored.
                batch.splice(0..0, *b"!\n");
            }
        }
        file.write_all(&batch).context("appending routing batch")?;
        file.flush().context("flushing routing batch")
    })
}

/// Read the requested matching tail and render it in append order.
/// The command opens only the log and takes no lock.
pub fn run(
    paths: &Paths,
    count: usize,
    tool: Option<&str>,
    profile: Option<&str>,
    json: bool,
) -> Result<()> {
    let (events, skipped) = read_tail(paths, count, tool, profile)?;
    if skipped > 0 {
        // Reject records without printing their contents or parser diagnostics.
        let _ = writeln!(
            std::io::stderr().lock(),
            "rtr: skipped {skipped} malformed, unsupported, or incomplete routing log records"
        );
    }
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    if json {
        serde_json::to_writer(&mut output, &events).context("writing routing log JSON")?;
        writeln!(output)?;
    } else {
        write_plain(&mut output, &events)?;
    }
    Ok(())
}

fn read_tail(
    paths: &Paths,
    count: usize,
    tool: Option<&str>,
    profile: Option<&str>,
) -> Result<(Vec<RoutingEvent>, usize)> {
    if count == 0 {
        return Ok((Vec::new(), 0));
    }
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(paths.state_dir.join("routing.jsonl"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => return Err(error).context("opening routing log"),
    };
    // The snapshot ends at the size we see now. Append-only writes preserve
    // that prefix, so the reader never needs to take the writer's lock.
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("routing log must be a regular file");
    }
    let mut position = metadata.len();
    let profile = profile.map(Label::new);
    let mut events = Vec::new();
    let mut skipped = 0;
    let mut block = [0; BLOCK_BYTES];
    let mut line = Vec::new();
    let mut terminated = false;
    'blocks: while position > 0 {
        let size = position.min(BLOCK_BYTES as u64) as usize;
        position -= size as u64;
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut block[..size])?;

        let mut end = size;
        // An extra boundary at byte zero closes the earliest line. Each
        // newline encountered in reverse closes the line on its right.
        let boundaries = memchr::memrchr_iter(b'\n', &block[..size])
            .map(|newline| newline + 1)
            .chain((position == 0).then_some(0));
        for start in boundaries {
            extend_reversed(&mut line, &block[start..end]);
            if terminated {
                line.reverse();
                match parse_record(&line) {
                    Some(event) => {
                        if tool.is_none_or(|tool| event.tool.as_str() == tool)
                            && profile
                                .as_ref()
                                .is_none_or(|name| event.matches_profile(name))
                        {
                            events.push(event);
                            if events.len() == count {
                                break 'blocks;
                            }
                        }
                    }
                    None => skipped += 1,
                }
            } else if !line.is_empty() {
                // The first nonempty chunk at EOF lacks a newline.
                skipped += 1;
            }
            line.clear();
            terminated = true;
            end = start.saturating_sub(1);
        }
        extend_reversed(&mut line, &block[..end]);
    }
    events.reverse();
    Ok((events, skipped))
}

fn extend_reversed(line: &mut Vec<u8>, fragment: &[u8]) {
    // The extra byte records overflow without retaining the rest of the line.
    let remaining = RECORD_BYTES + 1 - line.len();
    line.extend(fragment.iter().rev().take(remaining).copied());
}

fn parse_record(line: &[u8]) -> Option<RoutingEvent> {
    if line.len() > RECORD_BYTES {
        return None;
    }
    let event: RoutingEvent = serde_json::from_slice(line).ok()?;
    event.validate().ok()?;
    Some(event)
}

struct Row {
    time: String,
    event: &'static str,
    target: String,
    reasons: Vec<String>,
}

fn write_plain(output: &mut impl Write, events: &[RoutingEvent]) -> Result<()> {
    if events.is_empty() {
        writeln!(output, "No routing events.")?;
        return Ok(());
    }
    let zone = events[0].at.with_timezone(&Local).format("%Z").to_string();
    let same_zone = events
        .iter()
        .all(|event| event.at.with_timezone(&Local).format("%Z").to_string() == zone);
    let heading = format!("TIME ({})", if same_zone { &zone } else { "local" });
    let now = Utc::now();
    let rows: Vec<_> = events.iter().map(|event| plain_row(event, now)).collect();
    let time_width = rows
        .iter()
        .map(|row| width(&row.time))
        .max()
        .unwrap_or(0)
        .max(width(&heading));
    let event_width = 13;
    let target_width = rows
        .iter()
        .map(|row| width(&row.target))
        .max()
        .unwrap_or(0)
        .max(12);
    writeln!(
        output,
        "{}  {}  {}  REASON",
        padded(&heading, time_width),
        padded("EVENT", event_width),
        padded("TOOL/PROFILE", target_width)
    )?;
    let indent = " ".repeat(time_width + event_width + target_width + 6);
    for row in rows {
        writeln!(
            output,
            "{}  {}  {}  {}",
            padded(&row.time, time_width),
            padded(row.event, event_width),
            padded(&row.target, target_width),
            row.reasons.first().map(String::as_str).unwrap_or("")
        )?;
        for reason in row.reasons.iter().skip(1) {
            writeln!(output, "{indent}{reason}")?;
        }
    }
    Ok(())
}

fn plain_row(event: &RoutingEvent, now: DateTime<Utc>) -> Row {
    let tool = event.tool.as_str();
    let mut row = Row {
        time: crate::output::local_time(event.at),
        event: "",
        target: tool.to_string(),
        reasons: Vec::new(),
    };
    match &event.kind {
        EventKind::HoldLearned {
            profile,
            scope,
            native_kind,
            hold_until,
            ..
        } => {
            row.event = "hold learned";
            row.target = format!("{tool}/{profile}");
            let state = if *hold_until <= now {
                "hold expired"
            } else {
                "held until"
            };
            row.reasons.push(format!(
                "{state} {} · {scope}",
                crate::output::local_time(*hold_until)
            ));
            if let Some(kind) = native_kind {
                row.reasons.push(kind.to_string());
            }
        }
        EventKind::HoldCleared {
            profile,
            scope,
            reason,
            ..
        } => {
            row.event = "hold cleared";
            row.target = format!("{tool}/{profile}");
            row.reasons.push(format!(
                "{} · {scope}",
                match reason {
                    ClearReason::Recovery => "accepted recovery",
                    ClearReason::Manual => "manual clear",
                }
            ));
        }
        EventKind::Route(decision) | EventKind::Unavailable(decision) => {
            row.event = if matches!(event.kind, EventKind::Route(_)) {
                "route"
            } else {
                "unavailable"
            };
            row.target = target(
                tool,
                decision.source.as_ref(),
                decision.destination.as_ref(),
            );
            let mut reason = decision.reason.text().to_string();
            if let Some(model) = &decision.effective_model {
                reason.push_str(&format!(" · model {model}"));
            }
            if let Some(share) = decision.share_percent {
                reason.push_str(&format!(" · share {}%", share.get()));
            }
            reason.push_str(&format!(
                " · {} admitted / {} blocked",
                decision.admitted, decision.blocked
            ));
            row.reasons.push(reason);
            if let Some(reset) = decision.earliest_reset {
                row.reasons.push(format!(
                    "earliest retry {}",
                    crate::output::local_time(reset)
                ));
            }
            add_evidence(&mut row.reasons, &decision.evidence, event.at);
        }
        EventKind::ModelDrop {
            source,
            destination,
            requested_model,
            effective_model,
            admitted,
            blocked,
            evidence,
            ..
        } => {
            row.event = "model drop";
            row.target = target(tool, source.as_ref(), destination.as_ref());
            row.reasons
                .push(format!("{requested_model} → {effective_model}"));
            row.reasons
                .push(format!("{admitted} admitted / {blocked} blocked"));
            add_evidence(&mut row.reasons, evidence, event.at);
        }
        EventKind::ForkPrepared {
            source,
            destination,
            in_place,
            prepared_id,
        } => {
            row.event = "fork prepared";
            row.target = target(tool, Some(source), Some(destination));
            row.reasons.push(
                if *in_place {
                    "native fork in place"
                } else {
                    "copied to a fresh native ID"
                }
                .to_string(),
            );
            if let Some(id) = prepared_id {
                row.reasons.push(format!("prepared ID {id}"));
            }
        }
    }
    row
}

fn target(tool: &str, source: Option<&Label>, destination: Option<&Label>) -> String {
    match (source, destination) {
        (Some(source), Some(destination)) if source != destination => {
            format!("{tool}/{source} → {destination}")
        }
        (_, Some(destination)) => format!("{tool}/{destination}"),
        (Some(source), None) => format!("{tool}/{source}"),
        (None, None) => tool.to_string(),
    }
}

fn add_evidence(reasons: &mut Vec<String>, evidence: &[Evidence], at: DateTime<Utc>) {
    for item in evidence {
        let state = match item.reason {
            EvidenceReason::Observed => "observed",
            EvidenceReason::Full => "full",
            EvidenceReason::Held => "held",
            EvidenceReason::Unknown => "unknown",
            EvidenceReason::Missing => "missing",
            EvidenceReason::Stale => "stale",
            EvidenceReason::Expired => "expired",
        };
        let mut text = format!("{}: {}", item.profile, item.scope);
        if let Some(used) = item.used_percent {
            text.push_str(&format!(" {}% used", used.get()));
        }
        text.push_str(&format!(" · {state}"));
        if let Some(reset) = item.hold_until.or(item.reset_at) {
            text.push_str(&format!(" until {}", crate::output::local_time(reset)));
        }
        if let Some(observed) = item.observed_at {
            let minutes = (at - observed).num_minutes();
            let age = if minutes < 0 {
                "after event".to_string()
            } else if minutes < 1 {
                "<1m old".to_string()
            } else if minutes < 60 {
                format!("{minutes}m old")
            } else if minutes < 24 * 60 {
                format!("{}h{:02}m old", minutes / 60, minutes % 60)
            } else {
                format!("{}d{:02}h old", minutes / (24 * 60), minutes / 60 % 24)
            };
            text.push_str(&format!(" ({age})"));
        }
        if let Some(kind) = &item.native_kind {
            text.push_str(&format!(" · {kind}"));
        }
        reasons.push(text);
    }
}

fn width(text: &str) -> usize {
    ratatui::text::Span::raw(text).width()
}

fn padded(text: &str, columns: usize) -> String {
    format!("{text}{}", " ".repeat(columns.saturating_sub(width(text))))
}
