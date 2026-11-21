//! Joins read-only native usage, persisted holds, and launch counts for `rtr ls`.
//! Both layouts project the same evidence. A hold describes a rejection
//! independently of the last measured utilization.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};

use crate::{
    config::{Config, Profile},
    limits::{
        CodexSlot, Hold, HoldScope, Meter, MeterKey, MeterScope, MeterSource, MeterWindow,
        ModelFamily, FRESHNESS_MINUTES,
    },
    native_usage::{self, NativeRead},
    output::{clean, display_width, local_time, Style, Tone},
    paths::Paths,
    quota::{self, HoldEvidence, HoldIndex},
    usage::{self, Stats},
};

type Key = (String, String);
type Cell = (String, Tone);
const PROFILE_HEADERS: [&str; 6] = ["PROFILE", "STATE", "HOME", "SHARE", "RUNS", "FAILED"];
const MIN_TABLE_WIDTH: usize = 100;

pub fn run(paths: &Paths, all: bool, style: Style) -> Result<()> {
    let config = match Config::load(&paths.config_file()) {
        Ok(config) => config,
        // Historical usage can outlive the entire config, not only a profile.
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Config::default()
        }
        Err(error) => return Err(error),
    };
    let day = (!all).then(|| Local::now().date_naive());
    let stats = match usage::read_events(&paths.usage_file()) {
        Ok(events) => Some(usage::aggregate(&events, day)),
        Err(error) => {
            eprintln!("rtr: usage unavailable: {error:#}");
            None
        }
    };
    let holds = quota::read_holds(paths)?;
    let jobs: Vec<_> = config
        .tools
        .iter()
        .flat_map(|(tool, settings)| {
            settings
                .profiles
                .iter()
                .filter(|(_, profile)| !profile.bypass || !profile.enabled)
                .map(move |(name, _)| (tool, name))
        })
        .map(|(tool, name)| {
            let home = paths.profile_home_dir(tool, name);
            let histories = if tool == "codex" {
                native_usage::codex_histories(&home, |_| {})
            } else {
                Vec::new()
            };
            (tool, name, home, histories)
        })
        .collect();
    // Read origins after enumerating candidates. A published copy always has
    // a provenance record by the time its native filename becomes visible.
    let origins = quota::read_origins(paths)?;
    let now = Utc::now();
    let mut snapshots = BTreeMap::new();
    // Independent home reads use one clock and provenance index. Bound I/O to
    // four workers and join each batch before rendering any of its results.
    for batch in jobs.chunks(4) {
        std::thread::scope(|scope| {
            let readers: Vec<_> = batch
                .iter()
                .map(|(tool, name, home, histories)| {
                    let origins = &origins;
                    scope.spawn(move || {
                        let snapshot =
                            native_usage::read_discovered(home, tool, origins, now, histories);
                        (((*tool).clone(), (*name).clone()), snapshot)
                    })
                })
                .collect();
            snapshots.extend(
                readers
                    .into_iter()
                    .map(|reader| reader.join().expect("usage reader panicked")),
            );
        });
    }
    let mut listing = Listing::new(style, terminal_width(), now);
    listing.render(&config, stats.as_ref(), all, holds, snapshots);
    print!("{}", listing.out);
    Ok(())
}

#[cfg(test)]
pub fn render(config: &Config, stats: Option<&Stats>, label: &str, style: Style) -> String {
    let mut listing = Listing::new(style, None, Utc::now());
    listing.render(
        config,
        stats,
        label == "all time",
        Default::default(),
        Default::default(),
    );
    listing.out
}

/// One profile's policy, measured usage, and active rejections. Bypassed
/// launches use the native default home. A disabled profile can still serve
/// exact resumes from its isolated home, which also owns any persisted holds.
struct ProfileRow {
    summary: [Cell; 6],
    native: NativeRead,
    holds: Vec<Hold>,
    cleared_at: Option<DateTime<Utc>>,
    detail: String,
}

impl ProfileRow {
    fn name(&self) -> &str {
        &self.summary[0].0
    }

    fn bypassed(&self) -> bool {
        self.summary[2].0 == "bypassed"
    }

    fn shared_observation(&self) -> Option<Observation> {
        let mut observations = self.native.meters.iter().map(Observation::from);
        let first = observations.next()?;
        observations
            .all(|observation| observation == first)
            .then_some(first)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Observation {
    at: DateTime<Utc>,
    source: &'static str,
}

impl From<&Meter> for Observation {
    fn from(meter: &Meter) -> Self {
        Self {
            at: meter.observed_at,
            source: match meter.source {
                MeterSource::ClaudeCache => "cache",
                MeterSource::CodexRollout { .. } => "rollout",
            },
        }
    }
}

/// A column compares the same window and scope across accounts. If a Codex
/// observation supplies that identity twice, the native slot disambiguates it.
struct MeterColumn {
    key: MeterKey,
    slot: Option<CodexSlot>,
}

impl MeterColumn {
    fn label(&self) -> String {
        let label = meter_label(&self.key);
        match self.slot {
            Some(CodexSlot::Primary) => format!("{label} (primary)"),
            Some(CodexSlot::Secondary) => format!("{label} (secondary)"),
            None => label,
        }
    }

    fn reading<'a>(&self, row: &'a ProfileRow) -> Option<&'a Meter> {
        row.native.meters.iter().find(|meter| {
            meter.key == self.key
                && self
                    .slot
                    .is_none_or(|slot| meter.source == MeterSource::CodexRollout { slot })
        })
    }
}

fn meter_columns(tool: &str, rows: &[ProfileRow]) -> Vec<MeterColumn> {
    let mut duplicates = BTreeSet::new();
    let mut keys = BTreeSet::new();
    if tool == "claude" {
        keys.extend(claude_windows());
    }
    for row in rows {
        let mut seen = BTreeSet::new();
        for meter in &row.native.meters {
            if !seen.insert(&meter.key) {
                duplicates.insert(meter.key.clone());
            }
            keys.insert(meter.key.clone());
        }
        keys.extend(row.holds.iter().filter_map(|hold| match &hold.applies_to {
            HoldScope::Meter(key) => Some(key.clone()),
            HoldScope::Profile => None,
        }));
    }
    keys.into_iter()
        .flat_map(|key| {
            let slots: &[_] = if duplicates.contains(&key) {
                &[Some(CodexSlot::Primary), Some(CodexSlot::Secondary)]
            } else {
                &[None]
            };
            slots.iter().map(move |slot| MeterColumn {
                key: key.clone(),
                slot: *slot,
            })
        })
        .collect()
}

fn claude_windows() -> [MeterKey; 3] {
    [
        MeterKey {
            window: MeterWindow::FiveHour,
            scope: MeterScope::AllModels,
        },
        MeterKey {
            window: MeterWindow::Week,
            scope: MeterScope::AllModels,
        },
        MeterKey {
            window: MeterWindow::Week,
            scope: MeterScope::Model(ModelFamily::Fable),
        },
    ]
}

fn meter_label(key: &MeterKey) -> String {
    let window = match &key.window {
        MeterWindow::FiveHour => "5h".into(),
        MeterWindow::Week => "Week".into(),
        MeterWindow::Minutes(minutes) => format!("{minutes}m"),
        MeterWindow::Native(label) => label.clone(),
    };
    let scope = match &key.scope {
        MeterScope::AllModels if key.window == MeterWindow::Week => "all".into(),
        MeterScope::AllModels => return window,
        MeterScope::Model(model) => format!("{model:?}"),
        MeterScope::Unclassified(label) => label.clone(),
    };
    format!("{window}, {scope}")
}

fn active_holds(evidence: HoldEvidence, now: DateTime<Utc>) -> Vec<Hold> {
    // The hold index includes recovery watermarks and expired entries so
    // routing and listing can both evaluate evidence at their command clock.
    let watermarks = evidence.success_watermarks;
    let common = watermarks
        .common
        .max(watermarks.models.values().copied().max());
    evidence
        .holds
        .into_iter()
        .filter(|hold| {
            let success = match &hold.applies_to {
                HoldScope::Profile => common,
                HoldScope::Meter(key) => match &key.scope {
                    MeterScope::AllModels => common,
                    MeterScope::Model(model) => watermarks.models.get(model).copied(),
                    MeterScope::Unclassified(_) => None,
                },
            };
            hold.at <= now
                && hold.until() > now
                && evidence.cleared_at.is_none_or(|at| at < hold.at)
                && success.is_none_or(|at| at <= hold.at)
        })
        .collect()
}

fn summary(
    (tool, name): (&str, &str),
    profile: Option<&Profile>,
    stats: Option<&Stats>,
    share: String,
    limited: bool,
) -> [Cell; 6] {
    let (state, state_tone, home, home_tone) = match profile {
        Some(profile) => (
            if profile.enabled {
                "enabled"
            } else {
                "disabled"
            },
            if profile.enabled {
                Tone::Good
            } else {
                Tone::Muted
            },
            if profile.bypass {
                "bypassed"
            } else {
                "isolated"
            },
            if profile.bypass {
                Tone::Warning
            } else {
                Tone::Normal
            },
        ),
        None => ("removed", Tone::Muted, "-", Tone::Muted),
    };
    let state = if limited {
        format!("{state}; limited")
    } else {
        state.into()
    };
    let counts = stats.and_then(|stats| stats.get(tool)?.get(name));
    let count = |n: usize, tone| {
        if stats.is_none() {
            ("-".into(), Tone::Muted)
        } else {
            (n.to_string(), if n == 0 { Tone::Muted } else { tone })
        }
    };
    [
        (name.into(), Tone::Strong),
        (state, if limited { Tone::Warning } else { state_tone }),
        (home.into(), home_tone),
        (share, Tone::Accent),
        count(counts.map_or(0, |counts| counts.runs), Tone::Normal),
        count(counts.map_or(0, |counts| counts.failures), Tone::Bad),
    ]
}

fn detail(tool: &str, profile: &Profile, native: &NativeRead) -> String {
    let mut labels = Vec::new();
    if profile.bypass {
        labels.push("native default / unknown".into());
        if !native.meters.is_empty() {
            labels.push("meters from isolated home".into());
        }
    } else if tool == "codex" && native.meters.is_empty() {
        labels.push("no window reported".into());
    }
    if let Some(credits) = native
        .credit_info
        .as_ref()
        .filter(|credit| credit.has_credits)
    {
        labels.push(match (credits.unlimited, credits.balance) {
            (true, _) => "credits unlimited".into(),
            (false, Some(balance)) => format!("credits {balance}"),
            (false, None) => "credits available".into(),
        });
    }
    labels.join(" · ")
}

/// Renders one inspection using the same clock and terminal width for every
/// tool group, so meter age, expiry, and hold state agree across layouts.
struct Listing {
    out: String,
    style: Style,
    width: Option<usize>,
    now: DateTime<Utc>,
}

impl Listing {
    fn new(style: Style, width: Option<usize>, now: DateTime<Utc>) -> Self {
        Self {
            out: String::new(),
            style,
            width,
            now,
        }
    }

    fn render(
        &mut self,
        config: &Config,
        stats: Option<&Stats>,
        all: bool,
        mut holds: HoldIndex,
        mut snapshots: BTreeMap<Key, NativeRead>,
    ) {
        let period = if all { "all time" } else { "today" };
        self.line(0, &format!("rtr profiles · {period}"), Tone::Strong);
        self.out.push('\n');
        let mut allocation_errors = Vec::new();
        let mut configured = false;
        for (tool, settings) in &config.tools {
            let allocation = crate::selection::allocation(tool, settings);
            let rows: Vec<_> = settings
                .profiles
                .iter()
                .map(|(name, profile)| {
                    let key = (tool.clone(), name.clone());
                    let evidence = holds.remove(&key).unwrap_or_default();
                    let cleared_at = evidence.cleared_at;
                    let holds = active_holds(evidence, self.now);
                    let native = snapshots.remove(&key).unwrap_or_default();
                    ProfileRow {
                        summary: summary(
                            (tool, name),
                            Some(profile),
                            stats,
                            crate::weights::share_label(name, profile, allocation.as_ref().ok()),
                            !holds.is_empty(),
                        ),
                        detail: detail(tool, profile, &native),
                        native,
                        holds,
                        cleared_at,
                    }
                })
                .collect();
            if !rows.is_empty() {
                if configured {
                    self.out.push('\n');
                }
                configured = true;
                self.group(tool, &rows, false);
            }
            if let Err(error) = allocation {
                allocation_errors.push(error);
            }
        }
        if !configured {
            self.line(0, "No configured profiles.", Tone::Muted);
        }
        if let Some(stats) = stats {
            let mut heading = false;
            for (tool, profiles) in stats {
                let removed: Vec<_> = profiles
                    .keys()
                    .filter(|name| {
                        !config
                            .tools
                            .get(tool)
                            .is_some_and(|settings| settings.profiles.contains_key(*name))
                    })
                    .map(|name| ProfileRow {
                        summary: summary((tool, name), None, Some(stats), "-".into(), false),
                        native: NativeRead::default(),
                        holds: Vec::new(),
                        detail: String::new(),
                        cleared_at: None,
                    })
                    .collect();
                if !removed.is_empty() {
                    self.out.push('\n');
                    if !heading {
                        self.line(0, "Removed profiles · recorded usage", Tone::Muted);
                        heading = true;
                    }
                    self.group(tool, &removed, true);
                }
            }
        }
        if stats.is_none() {
            self.out.push('\n');
            self.line(0, "Usage unavailable. Counts shown as -.", Tone::Warning);
        }
        for error in allocation_errors {
            self.out.push('\n');
            self.line(
                0,
                &format!("Automatic selection unavailable: {error}"),
                Tone::Warning,
            );
        }
        if !all {
            self.out.push('\n');
            self.line(0, "Tip: rtr ls -a shows all-time usage.", Tone::Muted);
        }
    }

    fn group(&mut self, tool: &str, rows: &[ProfileRow], removed: bool) {
        let columns = if removed {
            Vec::new()
        } else {
            meter_columns(tool, rows)
        };
        if self.width.is_none_or(|width| width >= MIN_TABLE_WIDTH)
            && self.table(tool, rows, &columns)
        {
            return;
        }
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                self.out.push('\n');
            }
            self.block(tool, row, &columns);
        }
    }

    fn table(&mut self, tool: &str, profiles: &[ProfileRow], columns: &[MeterColumn]) -> bool {
        let show_seen = profiles
            .iter()
            .any(|row| row.shared_observation().is_some());
        // With no width constraint, keep the entire profile on one line so
        // a redirected listing can be searched or filtered by profile.
        let inline_detail = self.width.is_none();
        let show_detail = profiles.iter().any(|row| {
            (!row.detail.is_empty() && (inline_detail || !row.bypassed()))
                || (inline_detail && !row.holds.is_empty())
        });
        let mut headers: Vec<_> = PROFILE_HEADERS.map(|label| label.to_owned()).into();
        headers.extend(columns.iter().map(MeterColumn::label));
        if show_seen {
            headers.push("SEEN".into());
        }
        if show_detail {
            headers.push("DETAIL".into());
        }
        let mut widths: Vec<_> = headers.iter().map(|label| display_width(label)).collect();
        let rows: Vec<_> = profiles
            .iter()
            .map(|row| {
                let shared = row.shared_observation();
                let mut cells = row.summary.to_vec();
                for column in columns {
                    let cell = self
                        .meter_cell(row, column, 10, tool)
                        .map(|(mut text, tone)| {
                            if let Some(meter) = column.reading(row) {
                                let _ = write!(text, " {}", reset_label(meter));
                                if shared.is_none() {
                                    let _ = write!(text, " · {}", self.observation(meter.into()));
                                }
                            }
                            (text, tone)
                        });
                    cells.push(cell.unwrap_or_else(|| (String::new(), Tone::Normal)));
                }
                if show_seen {
                    cells.push((
                        shared
                            .map(|seen| self.observation(seen))
                            .unwrap_or_default(),
                        Tone::Muted,
                    ));
                }
                if show_detail {
                    let detail = if inline_detail {
                        let mut detail = row.detail.clone();
                        for hold in &row.holds {
                            if !detail.is_empty() {
                                detail.push_str(" · ");
                            }
                            detail.push_str(&hold_label(hold, self.now));
                        }
                        detail
                    } else if row.bypassed() {
                        String::new()
                    } else {
                        row.detail.clone()
                    };
                    cells.push((detail, Tone::Muted));
                }
                for (width, (text, _)) in widths.iter_mut().zip(&cells) {
                    *width = (*width).max(display_width(text));
                }
                cells
            })
            .collect();
        // Size actual text and labels before emitting any part of the table.
        // An additional native scope can make just its tool group need blocks.
        let columns_width: usize = widths.iter().sum();
        let separators = widths.len() - 1;
        let width = self.width.unwrap_or(usize::MAX);
        let gap = if columns_width + 2 * separators <= width {
            "  "
        } else if columns_width + separators <= width {
            " "
        } else {
            return false;
        };

        self.line(0, tool, Tone::Accent);
        let header: Vec<_> = headers
            .into_iter()
            .map(|text| (text, Tone::Muted))
            .collect();
        self.table_line(&header, &widths, gap);
        for cells in rows {
            self.table_line(&cells, &widths, gap);
        }
        if inline_detail {
            return true;
        }
        if profiles.iter().any(ProfileRow::bypassed) {
            self.line(
                2,
                "Bypassed homes use the native default / unknown usage.",
                Tone::Muted,
            );
        }
        for row in profiles {
            if row.bypassed() && !row.native.meters.is_empty() {
                self.line(2, &format!("{}: {}", row.name(), row.detail), Tone::Muted);
            }
            for hold in &row.holds {
                let label = hold_label(hold, self.now);
                self.line(2, &format!("{}: {label}", row.name()), Tone::Warning);
            }
        }
        true
    }

    fn table_line(&mut self, cells: &[Cell], widths: &[usize], gap: &str) {
        let last = cells
            .iter()
            .rposition(|(text, _)| !text.is_empty())
            .unwrap_or(0);
        for (i, ((text, tone), width)) in cells.iter().zip(widths).enumerate().take(last + 1) {
            if i == last {
                let _ = write!(self.out, "{}", self.style.paint(text, *tone));
            } else {
                let _ = write!(
                    self.out,
                    "{}{gap}",
                    self.style.padded(text, *tone, *width, (3..=5).contains(&i))
                );
            }
        }
        self.out.push('\n');
    }

    fn block(&mut self, tool: &str, row: &ProfileRow, columns: &[MeterColumn]) {
        let [_, (state, state_tone), (home, home_tone), (share, _), (runs, _), (failures, _)] =
            &row.summary;
        let identity = format!("{tool}/{}", row.name());
        let separate = self.width.is_some_and(|width| width < 44);
        if separate {
            self.line(0, &identity, Tone::Strong);
            self.line(2, &format!("state {state}"), *state_tone);
            if state != "removed" {
                self.line(2, &format!("home {home}"), *home_tone);
                self.line(2, &format!("share {share}"), Tone::Normal);
            }
            self.line(2, &format!("runs {runs}"), Tone::Normal);
            self.line(2, &format!("failed {failures}"), Tone::Normal);
        } else {
            self.line(0, &format!("{identity}  {state}"), *state_tone);
            let counts = format!("runs {runs} · failed {failures}");
            let policy = if state == "removed" {
                counts
            } else {
                format!("{home} · share {share} · {counts}")
            };
            self.line(2, &policy, *home_tone);
        }
        if !row.detail.is_empty() {
            self.line(2, &row.detail, Tone::Muted);
        }
        let shared = row.shared_observation();
        if let Some(seen) = shared {
            self.line(2, &format!("seen {}", self.observation(seen)), Tone::Muted);
        }
        for column in columns {
            if let Some((value, tone)) = self.meter_cell(row, column, 5, tool) {
                self.line(2, &format!("{}  {value}", column.label()), tone);
                if let Some(meter) = column.reading(row) {
                    let reset = reset_label(meter);
                    if let Some(seen) = shared.is_none().then(|| self.observation(meter.into())) {
                        if separate {
                            self.line(4, &reset, Tone::Muted);
                            self.line(4, &format!("seen {seen}"), Tone::Muted);
                        } else {
                            self.line(4, &format!("{reset} · {seen}"), Tone::Muted);
                        }
                    } else {
                        self.line(4, &reset, Tone::Muted);
                    }
                }
            }
        }
        for hold in &row.holds {
            self.line(2, &hold_label(hold, self.now), Tone::Warning);
        }
    }

    fn meter_cell(
        &self,
        row: &ProfileRow,
        column: &MeterColumn,
        bar_width: usize,
        tool: &str,
    ) -> Option<Cell> {
        let held = row
            .holds
            .iter()
            .any(|hold| matches!(&hold.applies_to, HoldScope::Meter(key) if *key == column.key));
        let marker = if held { " !" } else { "" };
        let Some(meter) = column.reading(row) else {
            let held_without_meter = held
                && column.slot != Some(CodexSlot::Secondary)
                && !row
                    .native
                    .meters
                    .iter()
                    .any(|meter| meter.key == column.key);
            if held_without_meter || (tool == "claude" && claude_windows().contains(&column.key)) {
                return Some((
                    format!("?{marker}"),
                    if held { Tone::Warning } else { Tone::Muted },
                ));
            }
            return None;
        };
        let used = meter.used_percent.get();
        let full = used == 100.0;
        let expired = meter.reset_at.is_some_and(|at| at <= self.now);
        let cleared = full && row.cleared_at.is_some_and(|at| at >= meter.observed_at);
        let filled = ((used * bar_width as f64 / 100.0).round() as usize).min(if full {
            bar_width
        } else {
            bar_width - 1
        });
        let bar = "█".repeat(filled) + &"░".repeat(bar_width - filled);
        let fraction = if full {
            "FULL".into()
        } else {
            format!("{}%", used.abs())
        };
        let was = if expired || cleared { "was " } else { "" };
        let clear = if cleared { " cleared" } else { "" };
        let text = format!("{bar} {was}{fraction}{clear}{marker}");
        let tone = if expired || cleared {
            Tone::Muted
        } else if full || held {
            Tone::Warning
        } else {
            Tone::Normal
        };
        Some((text, tone))
    }

    fn observation(&self, seen: Observation) -> String {
        let stale = if (self.now - seen.at).num_seconds() > FRESHNESS_MINUTES * 60 {
            " stale"
        } else {
            ""
        };
        format!("{} {}{stale}", age(seen.at, self.now), seen.source)
    }

    /// Wrap plain text by terminal cells before adding ANSI styling.
    fn line(&mut self, indent: usize, text: &str, tone: Tone) {
        let width = self.width.unwrap_or(usize::MAX);
        let indent = indent.min(width.saturating_sub(2));
        let available = width - indent;
        let pad = " ".repeat(indent);
        let text = clean(text);
        if display_width(&text) <= available {
            let _ = writeln!(self.out, "{pad}{}", self.style.paint(&text, tone));
            return;
        }
        let mut line = String::new();
        // Include the separators in the chunks so wrapping cannot collapse
        // distinct profile identities that differ only in internal spaces.
        for mut word in text.split_inclusive(char::is_whitespace) {
            if !line.is_empty() && display_width(&line) + display_width(word) > available {
                let _ = writeln!(self.out, "{pad}{}", self.style.paint(&line, tone));
                line.clear();
            }
            while display_width(word) > available {
                let cut = word
                    .char_indices()
                    .map(|(index, c)| index + c.len_utf8())
                    .take_while(|&end| display_width(&word[..end]) <= available)
                    .last();
                let (chunk, rest) = match cut {
                    Some(end) => word.split_at(end),
                    None => ("…", &word[word.chars().next().unwrap().len_utf8()..]),
                };
                let _ = writeln!(self.out, "{pad}{}", self.style.paint(chunk, tone));
                word = rest;
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            let _ = writeln!(self.out, "{pad}{}", self.style.paint(&line, tone));
        }
    }
}

fn reset_label(meter: &Meter) -> String {
    meter
        .reset_at
        .map(local_time)
        .unwrap_or_else(|| "reset ?".into())
}

fn hold_label(hold: &Hold, now: DateTime<Utc>) -> String {
    let scope = match &hold.applies_to {
        HoldScope::Profile => "profile".into(),
        HoldScope::Meter(key) => meter_label(key),
    };
    let until = local_time(hold.until());
    let release = if hold.reset_at.is_some() {
        "reset"
    } else {
        "retry after"
    };
    format!(
        "HELD {scope} · {release} {until} · {} transcript",
        age(hold.at, now)
    )
}

fn age(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (now - at).num_minutes();
    match minutes {
        ..=0 => "now".into(),
        1..=59 => format!("{minutes}m"),
        60..=1439 => format!("{}h", minutes / 60),
        _ => format!("{}d{}h", minutes / 1440, (minutes / 60) % 24),
    }
}

fn terminal_width() -> Option<usize> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // Query stdout itself. The controlling terminal can have a different
    // width, and redirected stdout should honor COLUMNS or the plain default.
    // SAFETY: TIOCGWINSZ writes one winsize into the initialized buffer.
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_col > 0
    {
        return Some(usize::from(size.ws_col));
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|width| width.parse().ok())
        .filter(|width| *width > 0)
}
