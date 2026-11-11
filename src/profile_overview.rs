//! Joins read-only subscription snapshots, current policy, and launch outcomes
//! for `rtr ls`. Utilization is display-only; learned quota holds take priority.
//! Configured profiles remain visible with zero activity. Usage from removed
//! profiles appears separately, without inventing current enable/bypass state.
use std::fmt::Write as _;

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use std::collections::BTreeMap;

use crate::{
    config::{Config, Profile},
    output::{display_width, Style, Tone},
    paths::Paths,
    usage::{self, Stats},
};

type Row = [(String, Tone); 11];
type Key = (String, String);
const HEADERS: [&str; 11] = [
    "AGENT", "PROFILE", "STATE", "HOME", "SHARE", "RUNS", "FAILED", "LEFT", "RESET", "AGE",
    "DETAIL",
];

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
            // An unavailable history must not hide configured profiles or turn
            // unreadable counts into misleading zeros.
            eprintln!("rtr: usage unavailable: {error:#}");
            None
        }
    };
    let held = crate::quota::held_limits(paths)?;
    let now = Utc::now();
    let jobs: Vec<_> = config
        .tools
        .iter()
        .flat_map(|(tool, settings)| {
            settings
                .profiles
                .iter()
                .filter(|(_, profile)| !profile.bypass)
                .map(move |(name, _)| (tool, name))
        })
        .collect();
    let mut snapshots = BTreeMap::new();
    // Profile homes are independent read-only sources. Bound parallel I/O to
    // four workers so a cold large history does not serialize every profile;
    // scoped joins finish all reads before rendering and retain no background work.
    for batch in jobs.chunks(4) {
        std::thread::scope(|scope| {
            let readers: Vec<_> = batch
                .iter()
                .map(|(tool, name)| {
                    scope.spawn(move || {
                        crate::native_usage::read(&paths.profile_home_dir(tool, name), tool, now)
                            .map(|snapshot| (((*tool).clone(), (*name).clone()), snapshot))
                    })
                })
                .collect();
            for reader in readers {
                if let Some((key, snapshot)) = reader.join().expect("usage reader panicked") {
                    snapshots.insert(key, snapshot);
                }
            }
        });
    }
    print!(
        "{}",
        render_with_limits(
            &config,
            stats.as_ref(),
            if all { "all time" } else { "today" },
            style,
            &held,
            &snapshots,
            now,
            terminal_width()
        )
    );
    if !all {
        println!(
            "\n{}",
            style.paint("Tip: rtr ls -a shows all-time usage.", Tone::Muted)
        );
    }
    Ok(())
}

#[cfg(test)]
pub fn render(config: &Config, stats: Option<&Stats>, label: &str, style: Style) -> String {
    render_with_limits(
        config,
        stats,
        label,
        style,
        &Default::default(),
        &Default::default(),
        Utc::now(),
        usize::MAX,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_with_limits(
    config: &Config,
    stats: Option<&Stats>,
    label: &str,
    style: Style,
    limits: &BTreeMap<Key, DateTime<Utc>>,
    snapshots: &BTreeMap<Key, crate::native_usage::Snapshot>,
    now: DateTime<Utc>,
    width: usize,
) -> String {
    let mut configured = Vec::new();
    let mut allocation_errors = Vec::new();
    for (tool, settings) in &config.tools {
        let allocation = crate::selection::allocation(tool, settings);
        for (name, profile) in &settings.profiles {
            let share = crate::weights::share_label(name, profile, allocation.as_ref().ok());
            let mut row = row(tool, name, Some(profile), stats, share);
            let key = (tool.clone(), name.clone());
            if let Some(until) = limits.get(&key) {
                // A learned hold wins over cached utilization, including when
                // disabled. It affects display here without learning new quota.
                row[2] = (
                    (if profile.enabled {
                        "limited"
                    } else {
                        "disabled; limited"
                    })
                    .into(),
                    Tone::Warning,
                );
                row[7] = ("0%".into(), Tone::Warning);
                row[8] = (crate::output::local_time(*until), Tone::Normal);
            } else if let Some(snapshot) = snapshots.get(&key) {
                row[7] = (
                    snapshot.left(),
                    if snapshot.used >= 90.0 {
                        Tone::Warning
                    } else {
                        Tone::Good
                    },
                );
                row[8] = (crate::output::local_time(snapshot.reset), Tone::Normal);
                row[9] = (snapshot.age(now), Tone::Muted);
                row[10] = (
                    snapshot.detail.clone().unwrap_or_else(|| "-".into()),
                    Tone::Normal,
                );
            }
            configured.push(row);
        }
        if let Err(error) = allocation {
            allocation_errors.push(error.to_string());
        }
    }
    let mut historical = Vec::new();
    if let Some(stats) = stats {
        for (tool, profiles) in stats {
            for name in profiles.keys() {
                if !config
                    .tools
                    .get(tool)
                    .is_some_and(|settings| settings.profiles.contains_key(name))
                {
                    historical.push(row(tool, name, None, Some(stats), "-".into()));
                }
            }
        }
    }
    let mut widths = HEADERS.map(str::len);
    for row in configured.iter().chain(&historical) {
        for (index, (text, _)) in row.iter().enumerate() {
            widths[index] = widths[index].max(display_width(text));
        }
    }
    let mut out = format!(
        "{} {}\n\n",
        style.paint("rtr profiles", Tone::Strong),
        style.paint(&format!("· {label}"), Tone::Muted)
    );
    if configured.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            style.paint("No configured profiles.", Tone::Muted)
        );
    } else {
        table(&mut out, &configured, widths, style, width);
    }
    if !historical.is_empty() {
        let _ = writeln!(
            out,
            "\n{}",
            style.paint("Removed profiles · recorded usage", Tone::Muted)
        );
        table(&mut out, &historical, widths, style, width);
    }
    if stats.is_none() {
        let _ = writeln!(
            out,
            "\n{}",
            style.paint("Usage unavailable; counts shown as -.", Tone::Warning)
        );
    }
    for error in allocation_errors {
        let _ = writeln!(
            out,
            "\n{}",
            style.paint(
                &format!("Automatic selection unavailable: {error}"),
                Tone::Warning
            )
        );
    }
    out
}

fn row(
    tool: &str,
    name: &str,
    profile: Option<&Profile>,
    stats: Option<&Stats>,
    share: String,
) -> Row {
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
    let counts = stats.and_then(|stats| stats.get(tool)?.get(name));
    let runs = counts.map_or(0, |counts| counts.runs);
    let failures = counts.map_or(0, |counts| counts.failures);
    [
        (tool.into(), Tone::Accent),
        (name.into(), Tone::Strong),
        (state.into(), state_tone),
        (home.into(), home_tone),
        (share, Tone::Accent),
        (
            if stats.is_some() {
                runs.to_string()
            } else {
                "-".into()
            },
            if runs == 0 { Tone::Muted } else { Tone::Normal },
        ),
        (
            if stats.is_some() {
                failures.to_string()
            } else {
                "-".into()
            },
            if failures == 0 {
                Tone::Muted
            } else {
                Tone::Bad
            },
        ),
        ("-".into(), Tone::Muted),
        ("-".into(), Tone::Muted),
        ("-".into(), Tone::Muted),
        ("-".into(), Tone::Muted),
    ]
}

fn terminal_width() -> usize {
    crossterm::terminal::size()
        .ok()
        .map(|(w, _)| usize::from(w))
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .unwrap_or(80)
        .max(1)
}

fn table(out: &mut String, rows: &[Row], mut widths: [usize; 11], style: Style, width: usize) {
    let mut columns: Vec<usize> = (0..HEADERS.len()).collect();
    // Preserve utilization, reset, age and scope before secondary policy/count
    // columns. On very narrow panes keep identity and LEFT, truncating cells by
    // terminal width so even hostile or unusually long profile names cannot wrap.
    for drop in [3, 4, 5, 6, 10, 9, 8, 2, 0] {
        let total: usize =
            columns.iter().map(|i| widths[*i]).sum::<usize>() + 2 * (columns.len() - 1);
        if total <= width {
            break;
        }
        columns.retain(|i| *i != drop);
    }
    while columns.iter().map(|i| widths[*i]).sum::<usize>() + 2 * (columns.len() - 1) > width {
        let largest = *columns.iter().max_by_key(|i| widths[**i]).unwrap();
        if widths[largest] == 0 {
            columns.pop();
        } else {
            widths[largest] -= 1;
        }
    }
    let header = columns
        .iter()
        .map(|i| {
            style.padded(
                &truncate(HEADERS[*i], widths[*i]),
                Tone::Muted,
                widths[*i],
                false,
            )
        })
        .collect::<Vec<_>>()
        .join("  ");
    let _ = writeln!(out, "{}", header.trim_end());
    let mut previous_tool = "";
    for row in rows {
        let line = columns
            .iter()
            .map(|i| {
                let (text, tone) = &row[*i];
                let text = if *i == 0 && text == previous_tool {
                    ""
                } else {
                    text
                };
                style.padded(
                    &truncate(text, widths[*i]),
                    *tone,
                    widths[*i],
                    matches!(*i, 4..=7),
                )
            })
            .collect::<Vec<_>>()
            .join("  ");
        let _ = writeln!(out, "{}", line.trim_end());
        previous_tool = &row[0].0;
    }
}

fn truncate(text: &str, width: usize) -> String {
    let text = crate::output::clean(text);
    if display_width(&text) <= width {
        return text;
    }
    let mut result = String::new();
    for c in text.chars() {
        let mut candidate = result.clone();
        candidate.push(c);
        if display_width(&candidate) > width.saturating_sub(1) {
            break;
        }
        result.push(c);
    }
    if width > 0 {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageEvent;
    use chrono::NaiveDate;

    fn config() -> Config {
        Config::parse("[tools.codex]\ncommand=[\"codex\"]\n[tools.codex.profiles.busy]\nbypass=true\n[tools.codex.profiles.idle]\nenabled=false\n").unwrap()
    }

    #[test]
    fn joins_unused_profiles_with_usage_and_keeps_removed_profiles_separate() {
        let events = [("busy", Some(0)), ("busy", None), ("removed", Some(1))].map(
            |(profile, exit_code)| UsageEvent {
                ts: "2026-09-08T12:00:00Z".into(),
                tool: "codex".into(),
                profile: profile.into(),
                exit_code,
                bypass: false,
            },
        );
        let stats = usage::aggregate(&events, None);
        let rendered = render(&config(), Some(&stats), "all time", Style::default());
        let flat = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("busy enabled bypassed 100% 2 1"),
            "{rendered}"
        );
        assert!(flat.contains("idle disabled isolated 0% 0 0"), "{rendered}");
        let history = flat
            .split_once("Removed profiles · recorded usage")
            .unwrap()
            .1;
        assert!(history.contains("removed removed - - 1 1"), "{rendered}");
        assert!(!history.contains("busy"), "{rendered}");

        let empty_day =
            usage::aggregate(&events, Some(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()));
        let today = render(&config(), Some(&empty_day), "today", Style::default());
        let flat = today.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("busy enabled bypassed 100% 0 0"), "{today}");
        assert!(!today.contains("Removed profiles"), "{today}");
    }

    #[test]
    fn unavailable_usage_does_not_claim_zero_runs() {
        let rendered = render(&config(), None, "today", Style::default());
        let flat = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("busy enabled bypassed 100% - -"),
            "{rendered}"
        );
        assert!(rendered.contains("Usage unavailable"));
    }
    #[test]
    fn held_limit_overrides_stale_snapshot_and_narrow_rows_keep_remaining() {
        let now = Utc::now();
        let config = Config::parse("[tools.claude]\ncommand=['claude']\n[tools.claude.profiles.work]\n[tools.claude.profiles.unknown]\n").unwrap();
        let snapshots = [(
            ("claude".into(), "work".into()),
            crate::native_usage::Snapshot {
                used: 63.0,
                observed: now - chrono::Duration::hours(39),
                reset: now + chrono::Duration::days(2),
                detail: Some("Fable".into()),
            },
        )]
        .into();
        let held = [(
            ("claude".into(), "work".into()),
            now + chrono::Duration::hours(1),
        )]
        .into();
        let stats = Stats::default();
        let full = render_with_limits(
            &config,
            Some(&stats),
            "today",
            Style::default(),
            &held,
            &snapshots,
            now,
            80,
        );
        let row = full
            .lines()
            .find(|l| l.split_whitespace().any(|s| s == "work"))
            .unwrap();
        assert!(row.contains("limited") && row.contains("0%"), "{full}");
        assert!(
            row.contains(&crate::output::local_time(now + chrono::Duration::hours(1))),
            "{full}"
        );
        assert!(!row.contains("37%") && !row.contains("Fable"), "{full}");
        assert!(full
            .lines()
            .find(|l| l.split_whitespace().any(|s| s == "unknown"))
            .unwrap()
            .ends_with('-'));
        let fresh = render_with_limits(
            &config,
            Some(&stats),
            "today",
            Style::default(),
            &BTreeMap::new(),
            &snapshots,
            now,
            80,
        );
        assert!(
            fresh.contains("37%") && fresh.contains("39h") && fresh.contains("Fable"),
            "{fresh}"
        );
        for width in [20, 32, 44, 60, 80] {
            let narrow = render_with_limits(
                &config,
                Some(&stats),
                "today",
                Style::default(),
                &held,
                &snapshots,
                now,
                width,
            );
            assert!(
                narrow.lines().all(|l| display_width(l) <= width),
                "{width}: {narrow}"
            );
            assert!(narrow.contains("0%"), "{narrow}");
        }
    }
}
