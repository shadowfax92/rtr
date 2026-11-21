//! Renders routing's read-only limit evidence alongside profile policy and
//! recorded runs. Every layout preserves measurements and scoped holds.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    io::{self, IsTerminal},
};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};

use crate::{
    config::{Config, Profile},
    limits::{
        CodexSlot, Hold, HoldScope, Meter, MeterKey, MeterScope, MeterSource, MeterState,
        MeterWindow, ModelFamily, ProfileLimits, FRESHNESS_MINUTES,
    },
    output::{clean, display_width, local_time, Style, Tone},
    paths::Paths,
    routing,
    usage::{self, ProfileStats, Stats},
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
    let limits = routing::listing_limits(paths, &config)?;
    let mut listing = Listing::new(
        style,
        terminal_width(),
        io::stdout().is_terminal(),
        Utc::now(),
    );
    listing.render(&config, stats.as_ref(), all, limits);
    print!("{}", listing.out);
    Ok(())
}

#[cfg(test)]
pub fn render(config: &Config, stats: Option<&Stats>, label: &str, style: Style) -> String {
    let mut listing = Listing::new(style, None, false, Utc::now());
    listing.render(config, stats, label == "all time", Default::default());
    listing.out
}

/// One profile's policy, measured usage, and active rejections. Bypassed
/// launches use the native default home. A disabled profile can still serve
/// exact resumes from its isolated home, which also owns any persisted holds.
struct ProfileRow {
    summary: [Cell; 4],
    counts: Option<ProfileStats>,
    readings: Vec<Reading>,
    holds: Vec<Hold>,
    detail: String,
}

impl ProfileRow {
    fn name(&self) -> &str {
        &self.summary[0].0
    }

    fn bypassed(&self) -> bool {
        self.summary[2].0 == "bypassed"
    }

    fn run_cells(&self) -> [Cell; 2] {
        let Some(counts) = &self.counts else {
            return [("-".into(), Tone::Muted), ("-".into(), Tone::Muted)];
        };
        [
            count_cell(counts.runs, Tone::Normal),
            count_cell(counts.failures, Tone::Bad),
        ]
    }

    fn compact_runs(&self) -> Cell {
        let Some(counts) = &self.counts else {
            return ("-".into(), Tone::Muted);
        };
        let ProfileStats { runs, failures } = *counts;
        let tone = if failures > 0 {
            Tone::Bad
        } else if runs == 0 {
            Tone::Muted
        } else {
            Tone::Normal
        };
        // The aggregate's total includes failures. RUNS shows ok/failed.
        (format!("{}/{failures}", runs - failures), tone)
    }

    fn shared_observation(&self) -> Option<Observation> {
        shared_observation(self.readings.iter())
    }
}

/// Retain the native measurement alongside its routing interpretation.
struct Reading {
    meter: Meter,
    state: MeterState,
}

fn shared_observation<'a>(readings: impl Iterator<Item = &'a Reading>) -> Option<Observation> {
    let mut observations = readings.map(|reading| Observation::from(&reading.meter));
    let first = observations.next()?;
    observations.all(|seen| seen == first).then_some(first)
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

    fn reading<'a>(&self, row: &'a ProfileRow) -> Option<&'a Reading> {
        row.readings.iter().find(|reading| {
            reading.meter.key == self.key
                && self
                    .slot
                    .is_none_or(|slot| reading.meter.source == MeterSource::CodexRollout { slot })
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
        for Reading { meter, .. } in &row.readings {
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

fn summary(name: &str, profile: Option<&Profile>, share: String, limited: bool) -> [Cell; 4] {
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
    [
        (name.into(), Tone::Strong),
        (state, if limited { Tone::Warning } else { state_tone }),
        (home.into(), home_tone),
        (share, Tone::Accent),
    ]
}

fn count_cell(count: usize, tone: Tone) -> Cell {
    (
        count.to_string(),
        if count == 0 { Tone::Muted } else { tone },
    )
}

fn detail(tool: &str, profile: &Profile, limits: &ProfileLimits) -> String {
    let mut labels = Vec::new();
    if profile.bypass {
        labels.push("native default / unknown".into());
        if !limits.meters.is_empty() {
            labels.push("meters from isolated home".into());
        }
    } else if tool == "codex" && limits.meters.is_empty() {
        labels.push("no window reported".into());
    }
    if let Some(credits) = limits
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
    compact: bool,
    now: DateTime<Utc>,
}

impl Listing {
    fn new(style: Style, width: Option<usize>, compact: bool, now: DateTime<Utc>) -> Self {
        Self {
            out: String::new(),
            style,
            width,
            compact,
            now,
        }
    }

    fn render(
        &mut self,
        config: &Config,
        stats: Option<&Stats>,
        all: bool,
        mut evidence: BTreeMap<Key, ProfileLimits>,
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
                    let limits = evidence
                        .remove(&(tool.clone(), name.clone()))
                        .unwrap_or_default();
                    let holds: Vec<_> = limits.active_holds(self.now).cloned().collect();
                    let readings = limits
                        .meters
                        .iter()
                        .map(|meter| {
                            let family = match (&meter.key.scope, tool.as_str()) {
                                (MeterScope::Model(family), _) => *family,
                                (_, "codex") => ModelFamily::Codex,
                                (_, _) => ModelFamily::Unknown,
                            };
                            Reading {
                                state: limits.meter_state(meter, family, self.now),
                                meter: meter.clone(),
                            }
                        })
                        .collect();
                    ProfileRow {
                        summary: summary(
                            name,
                            Some(profile),
                            crate::weights::share_label(name, profile, allocation.as_ref().ok()),
                            !holds.is_empty(),
                        ),
                        counts: stats.map(|stats| {
                            stats
                                .get(tool)
                                .and_then(|counts| counts.get(name))
                                .cloned()
                                .unwrap_or_default()
                        }),
                        detail: detail(tool, profile, &limits),
                        readings,
                        holds,
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
                        summary: summary(name, None, "-".into(), false),
                        counts: Some(profiles[name].clone()),
                        readings: Vec::new(),
                        holds: Vec::new(),
                        detail: String::new(),
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
        if self.width.is_none_or(|width| width >= MIN_TABLE_WIDTH) {
            // Codex subscriptions compare across homes. Other limit IDs can
            // vary per home and would make the whole table depend on one rollout.
            let (main, extras): (Vec<_>, Vec<_>) = columns.iter().partition(|column| {
                !self.compact || tool != "codex" || column.key.scope == MeterScope::AllModels
            });
            let bar_width = if self.compact { 3 } else { 10 };
            if self.table(tool, rows, &main, &extras, true, bar_width)
                || (self.compact
                    && [3, 2]
                        .into_iter()
                        .any(|bar_width| self.table(tool, rows, &main, &extras, false, bar_width)))
            {
                return;
            }
        }
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                self.out.push('\n');
            }
            self.block(tool, row, &columns);
        }
    }

    fn table(
        &mut self,
        tool: &str,
        profiles: &[ProfileRow],
        columns: &[&MeterColumn],
        extras: &[&MeterColumn],
        allow_seen: bool,
        bar_width: usize,
    ) -> bool {
        let shared: Vec<_> = profiles
            .iter()
            .map(|row| shared_observation(columns.iter().filter_map(|column| column.reading(row))))
            .collect();
        let show_seen = allow_seen && shared.iter().any(Option::is_some);
        let compact = self.compact;
        // Redirected output without a requested width keeps every detail on the
        // profile row. Terminal tables reserve that space for comparisons.
        let inline_detail = !compact && self.width.is_none();
        let show_detail = !compact
            && profiles.iter().any(|row| {
                (!row.detail.is_empty() && (inline_detail || !row.bypassed()))
                    || (inline_detail && !row.holds.is_empty())
            });
        let show_home = !compact || profiles.iter().any(ProfileRow::bypassed);
        let headers: Vec<_> = PROFILE_HEADERS
            .into_iter()
            .filter(|label| (*label != "HOME" || show_home) && (*label != "FAILED" || !compact))
            .map(str::to_owned)
            .chain(columns.iter().map(|column| column.label()))
            .chain(show_seen.then(|| "SEEN".into()))
            .chain(show_detail.then(|| "DETAIL".into()))
            .collect();
        let mut widths: Vec<_> = headers.iter().map(|label| display_width(label)).collect();
        let rows: Vec<_> = profiles
            .iter()
            .zip(&shared)
            .map(|(row, shared)| {
                let mut cells = row.summary.to_vec();
                if !show_home {
                    cells.remove(2);
                }
                if compact {
                    cells.push(row.compact_runs());
                } else {
                    cells.extend(row.run_cells());
                }
                for column in columns {
                    let cell =
                        self.meter_cell(row, column, bar_width, tool)
                            .map(|(mut text, tone)| {
                                if let Some(reading) = column.reading(row) {
                                    let _ = write!(text, " {}", reading.timing(compact));
                                    if !compact && shared.is_none() {
                                        let _ = write!(
                                            text,
                                            " · {}",
                                            self.observation((&reading.meter).into())
                                        );
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
        // Size actual labels and values before printing. A retry can move the
        // observation metadata below the comparison when the pane is tight.
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
            .iter()
            .map(|label| (label.clone(), Tone::Muted))
            .collect();
        self.table_line(&header, &headers, &widths, gap);
        for ((cells, row), shared) in rows.into_iter().zip(profiles).zip(shared) {
            self.table_line(&cells, &headers, &widths, gap);
            if compact {
                self.continuation(tool, row, columns, extras, shared, show_seen);
            }
        }
        if !inline_detail && !compact {
            if profiles.iter().any(ProfileRow::bypassed) {
                self.line(
                    2,
                    "Bypassed homes use the native default / unknown usage.",
                    Tone::Muted,
                );
            }
            for row in profiles {
                if row.bypassed() && !row.readings.is_empty() {
                    self.line(2, &format!("{}: {}", row.name(), row.detail), Tone::Muted);
                }
                for hold in &row.holds {
                    let label = hold_label(hold, self.now);
                    self.line(2, &format!("{}: {label}", row.name()), Tone::Warning);
                }
            }
        }
        true
    }

    fn continuation(
        &mut self,
        tool: &str,
        row: &ProfileRow,
        columns: &[&MeterColumn],
        extras: &[&MeterColumn],
        shared: Option<Observation>,
        show_seen: bool,
    ) {
        let mut notes = Vec::new();
        if !row.detail.is_empty() {
            notes.push(row.detail.clone());
        }
        if let Some(seen) = shared.filter(|_| !show_seen) {
            notes.push(format!("seen {}", self.observation(seen)));
        }
        for column in columns {
            if let Some(reading) = column.reading(row) {
                if shared.is_none() {
                    notes.push(format!(
                        "{} seen {}",
                        column.label(),
                        self.observation((&reading.meter).into())
                    ));
                }
                if matches!(reading.state, MeterState::Superseded(_)) {
                    notes.push(format!(
                        "{} prior sample FULL · reset {}",
                        column.label(),
                        reset_label(&reading.meter, true)
                    ));
                }
            }
        }
        if !notes.is_empty() {
            self.line(
                2,
                &format!("{} · {}", row.name(), notes.join(" · ")),
                Tone::Muted,
            );
        }
        // Each extra has its own label and timing so line wrapping cannot
        // visually attach one scope's reset to the next scope's bar.
        for column in extras {
            if let Some((value, tone)) = self.meter_cell(row, column, 3, tool) {
                let mut text = format!("{} · {} {value}", row.name(), column.label());
                if let Some(reading) = column.reading(row) {
                    let _ = write!(text, " {}", reading.timing(false));
                    let seen = (&reading.meter).into();
                    if Some(seen) != shared {
                        let _ = write!(text, " · {}", self.observation(seen));
                    }
                }
                self.line(2, &text, tone);
            }
        }
        for hold in &row.holds {
            self.line(
                2,
                &format!("{} · {}", row.name(), hold_label(hold, self.now)),
                Tone::Warning,
            );
        }
    }

    fn table_line(&mut self, cells: &[Cell], headers: &[String], widths: &[usize], gap: &str) {
        let last = cells
            .iter()
            .rposition(|(text, _)| !text.is_empty())
            .unwrap_or(0);
        for (i, ((text, tone), width)) in cells.iter().zip(widths).enumerate().take(last + 1) {
            if i == last {
                let _ = write!(self.out, "{}", self.style.paint(text, *tone));
            } else {
                let numeric = matches!(headers[i].as_str(), "SHARE" | "RUNS" | "FAILED");
                let _ = write!(
                    self.out,
                    "{}{gap}",
                    self.style.padded(text, *tone, *width, numeric)
                );
            }
        }
        self.out.push('\n');
    }

    fn block(&mut self, tool: &str, row: &ProfileRow, columns: &[MeterColumn]) {
        let [_, (state, state_tone), (home, home_tone), (share, _)] = &row.summary;
        let [(runs, _), (failures, _)] = row.run_cells();
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
                if let Some(reading) = column.reading(row) {
                    let reset = reading.timing(false);
                    if let Some(seen) = shared
                        .is_none()
                        .then(|| self.observation((&reading.meter).into()))
                    {
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
        let Some(reading) = column.reading(row) else {
            let held_without_meter = held
                && column.slot != Some(CodexSlot::Secondary)
                && !row
                    .readings
                    .iter()
                    .any(|reading| reading.meter.key == column.key);
            if held_without_meter || (tool == "claude" && claude_windows().contains(&column.key)) {
                return Some((
                    format!("?{marker}"),
                    if held { Tone::Warning } else { Tone::Muted },
                ));
            }
            return None;
        };
        let used = reading.meter.used_percent.get();
        let full = used == 100.0;
        let filled = ((used * bar_width as f64 / 100.0).round() as usize).min(if full {
            bar_width
        } else {
            bar_width - 1
        });
        let bar = "█".repeat(filled) + &"░".repeat(bar_width - filled);
        let value = match reading.state {
            MeterState::Full => "FULL".into(),
            MeterState::Superseded(_) => "superseded".into(),
            MeterState::Cleared => "was FULL cleared".into(),
            MeterState::Expired if full => "was FULL".into(),
            MeterState::Expired => format!("was {used}%"),
            MeterState::Fresh | MeterState::Unknown => format!("{used}%"),
        };
        let text = format!("{bar} {value}{marker}");
        let tone = match reading.state {
            MeterState::Expired | MeterState::Cleared | MeterState::Superseded(_) => Tone::Muted,
            MeterState::Full => Tone::Warning,
            _ if held => Tone::Warning,
            _ => Tone::Normal,
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

impl Reading {
    fn timing(&self, compact: bool) -> String {
        match self.state {
            MeterState::Superseded(at) if compact => compact_time(at),
            MeterState::Superseded(at) => {
                format!(
                    "by success {} · prior sample FULL · reset {}",
                    local_time(at),
                    reset_label(&self.meter, false)
                )
            }
            _ => reset_label(&self.meter, compact),
        }
    }
}

fn compact_time(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%a %H:%M").to_string()
}

fn reset_label(meter: &Meter, compact: bool) -> String {
    meter
        .reset_at
        .map(if compact { compact_time } else { local_time })
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
