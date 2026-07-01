use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::capture;
use crate::paths::Paths;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct UsageEvent {
    pub ts: String,
    pub tool: String,
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileStats {
    pub runs: usize,
    pub failures: usize,
}

pub type Stats = BTreeMap<String, BTreeMap<String, ProfileStats>>;

pub fn new_event(
    tool: &str,
    profile: &str,
    preset: Option<&str>,
    exit_code: Option<i32>,
) -> UsageEvent {
    UsageEvent {
        ts: capture::now_rfc3339(),
        tool: tool.to_string(),
        profile: profile.to_string(),
        preset: preset.map(str::to_string),
        exit_code,
    }
}

pub fn append_event(path: &Path, event: &UsageEvent) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        crate::paths::create_private_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(event).context("serializing usage event")?;
    line.push(b'\n');
    crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(path), || {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.write_all(&line).context("writing usage event")?;
        file.flush().context("flushing usage event")?;
        Ok(())
    })
}

pub fn read_events(path: &Path) -> Result<Vec<UsageEvent>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut events = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(event) => events.push(event),
            Err(e) => eprintln!(
                "rtr: ignoring malformed usage line {} in {}: {e}",
                idx + 1,
                path.display()
            ),
        }
    }
    Ok(events)
}

pub fn aggregate(events: &[UsageEvent], local_day: Option<NaiveDate>) -> Stats {
    let mut stats = Stats::new();
    for event in events {
        if let Some(day) = local_day {
            if event_local_day(&event.ts) != Some(day) {
                continue;
            }
        }
        let profile = stats
            .entry(event.tool.clone())
            .or_default()
            .entry(event.profile.clone())
            .or_default();
        profile.runs += 1;
        if event.exit_code != Some(0) {
            profile.failures += 1;
        }
    }
    stats
}

pub fn render_stats(stats: &Stats, label: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "rtr stats ({label})");
    if stats.is_empty() {
        let _ = writeln!(out, "  no usage recorded");
        return out;
    }
    for (tool, profiles) in stats {
        let _ = writeln!(out, "{tool}");
        for (profile, stats) in profiles {
            let pct = if stats.runs == 0 {
                0.0
            } else {
                (stats.failures as f64 / stats.runs as f64) * 100.0
            };
            let _ = writeln!(
                out,
                "  {profile}: {} runs, {} failed ({pct:.1}%)",
                stats.runs, stats.failures
            );
        }
    }
    out
}

pub fn print_stats(paths: &Paths, today: bool) -> Result<()> {
    let events = read_events(&paths.usage_file())?;
    let (day, label) = if today {
        (Some(Local::now().date_naive()), "today")
    } else {
        (None, "all time")
    };
    print!("{}", render_stats(&aggregate(&events, day), label));
    Ok(())
}

fn event_local_day(ts: &str) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.with_timezone(&Local).date_naive())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn event(ts: &str, tool: &str, profile: &str, exit_code: Option<i32>) -> UsageEvent {
        UsageEvent {
            ts: ts.to_string(),
            tool: tool.to_string(),
            profile: profile.to_string(),
            preset: None,
            exit_code,
        }
    }

    #[test]
    fn aggregate_counts_successes_and_failures_for_day() {
        let day = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let events = vec![
            event("2026-07-01T12:00:00-07:00", "codex", "work", Some(0)),
            event("2026-07-01T13:00:00-07:00", "codex", "work", Some(2)),
            event("2026-07-01T14:00:00-07:00", "codex", "personal", None),
            event("2026-07-02T12:00:00-07:00", "codex", "work", Some(0)),
        ];
        let stats = aggregate(&events, Some(day));
        assert_eq!(stats["codex"]["work"].runs, 2);
        assert_eq!(stats["codex"]["work"].failures, 1);
        assert_eq!(stats["codex"]["personal"].failures, 1);
    }

    #[test]
    fn render_includes_failed_percentage() {
        let stats = aggregate(
            &[
                event("2026-07-01T12:00:00-07:00", "claude", "work", Some(0)),
                event("2026-07-01T13:00:00-07:00", "claude", "work", Some(1)),
            ],
            None,
        );
        let out = render_stats(&stats, "all time");
        assert!(out.contains("work: 2 runs, 1 failed (50.0%)"), "{out}");
    }

    #[test]
    fn append_and_read_usage_jsonl_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        append_event(
            &path,
            &event("2026-07-01T12:00:00Z", "codex", "work", Some(0)),
        )
        .unwrap();
        append_event(
            &path,
            &event("2026-07-01T12:01:00Z", "codex", "personal", Some(1)),
        )
        .unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].profile, "personal");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn read_events_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        std::fs::write(
            &path,
            [
                serde_json::to_string(&event("2026-07-01T12:00:00Z", "codex", "work", Some(0)))
                    .unwrap(),
                "not-json".to_string(),
                serde_json::to_string(&event("2026-07-01T12:01:00Z", "codex", "personal", Some(1)))
                    .unwrap(),
            ]
            .join("\n"),
        )
        .unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].profile, "personal");
    }
}
