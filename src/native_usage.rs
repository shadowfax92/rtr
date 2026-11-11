//! Read-only subscription snapshots for `ls`; these figures never feed routing.
//! Native caches can lag or outlive a reset, so every window carries observation
//! and reset times. Missing, malformed, or bounded-out input remains unknown.
use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// The tightest current window, with optional information explaining its scope
/// or why a saturated Codex subscription can still serve paid turns.
#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub used: f64,
    pub observed: DateTime<Utc>,
    pub reset: DateTime<Utc>,
    pub detail: Option<String>,
}

impl Snapshot {
    pub fn left(&self) -> String {
        format!("{:.0}%", (100.0 - self.used).floor())
    }
    pub fn age(&self, now: DateTime<Utc>) -> String {
        let minutes = (now - self.observed).num_minutes().max(0);
        match minutes {
            0..=14 => "-".into(),
            15..=59 => format!("{minutes}m"),
            60..=2879 => format!("{}h", minutes / 60),
            _ => format!("{}d", minutes / 1440),
        }
    }
}

fn window(
    used: f64,
    observed: DateTime<Utc>,
    reset: DateTime<Utc>,
    now: DateTime<Utc>,
    detail: Option<String>,
) -> Option<Snapshot> {
    (used.is_finite()
        && (0.0..=100.0).contains(&used)
        && observed <= now
        && observed < reset
        && reset > now)
        .then_some(Snapshot {
            used,
            observed,
            reset,
            detail,
        })
}

pub(crate) fn read(home: &Path, tool: &str, now: DateTime<Utc>) -> Option<Snapshot> {
    match tool {
        "claude" => claude(home, now),
        "codex" => codex(home, now),
        _ => None,
    }
}

// Serde ignores every other top-level field: account identity, project paths,
// and auth metadata never enter the usage model or any persisted RTR output.
#[derive(Deserialize)]
struct ClaudeHome {
    #[serde(rename = "cachedUsageUtilization")]
    cache: Option<ClaudeCache>,
}
#[derive(Deserialize)]
struct ClaudeCache {
    #[serde(rename = "fetchedAtMs")]
    fetched_at: i64,
    utilization: ClaudeUsage,
}
#[derive(Deserialize)]
struct ClaudeUsage {
    seven_day: Option<ClaudeWindow>,
    five_hour: Option<ClaudeWindow>,
    #[serde(default)]
    limits: Vec<ClaudeLimit>,
}
#[derive(Deserialize)]
struct ClaudeWindow {
    utilization: f64,
    resets_at: DateTime<Utc>,
}
#[derive(Deserialize)]
struct ClaudeLimit {
    percent: f64,
    resets_at: DateTime<Utc>,
    scope: Option<Scope>,
}
#[derive(Deserialize)]
struct Scope {
    model: Option<Model>,
}
#[derive(Deserialize)]
struct Model {
    display_name: String,
}

fn claude(home: &Path, now: DateTime<Utc>) -> Option<Snapshot> {
    let file = File::open(home.join(".claude.json")).ok()?;
    let cache = serde_json::from_reader::<_, ClaudeHome>(BufReader::new(file))
        .ok()?
        .cache?;
    let at = DateTime::from_timestamp_millis(cache.fetched_at)?;
    let usage = cache.utilization;
    let mut windows: Vec<_> = [usage.seven_day, usage.five_hour]
        .into_iter()
        .flatten()
        .filter_map(|w| window(w.utilization, at, w.resets_at, now, None))
        .collect();
    windows.extend(usage.limits.into_iter().filter_map(|w| {
        let model = w
            .scope
            .and_then(|s| s.model)
            .map(|m| crate::output::clean(&m.display_name));
        window(w.percent, at, w.resets_at, now, model)
    }));
    // Prefer the unscoped figure on a tie; call out a model only when tighter.
    windows.into_iter().max_by(|a, b| {
        a.used
            .total_cmp(&b.used)
            .then_with(|| a.detail.is_none().cmp(&b.detail.is_none()))
    })
}

#[derive(Deserialize)]
struct Event {
    timestamp: DateTime<Utc>,
    #[serde(rename = "type")]
    kind: String,
    payload: TokenCount,
}
#[derive(Deserialize)]
struct TokenCount {
    #[serde(rename = "type")]
    kind: String,
    rate_limits: Option<RateLimits>,
}
#[derive(Deserialize)]
struct RateLimits {
    primary: Option<CodexWindow>,
    secondary: Option<CodexWindow>,
    credits: Option<Credits>,
}
#[derive(Deserialize)]
struct CodexWindow {
    used_percent: f64,
    resets_at: i64,
}
#[derive(Deserialize)]
struct Credits {
    has_credits: bool,
    unlimited: bool,
    balance: Option<String>,
}

fn codex(home: &Path, now: DateTime<Utc>) -> Option<Snapshot> {
    let mut files = Vec::new();
    for root in [home.join("sessions"), home.join("archived_sessions")] {
        collect_histories(&root, &mut files).ok()?;
    }
    files.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut latest: Option<Event> = None;
    // Inspect recent writers, not newest filenames: a long-running conversation
    // can be older than today's directory. Metadata prunes histories that could
    // not have recorded a newer observation. No catalog/cache is written.
    for (modified, path) in files {
        if latest
            .as_ref()
            .is_some_and(|e| DateTime::<Utc>::from(modified) < e.timestamp)
        {
            break;
        }
        if let Some(event) = codex_tail(&path) {
            if latest
                .as_ref()
                .is_none_or(|e| event.timestamp > e.timestamp)
            {
                latest = Some(event);
            }
        }
    }
    let event = latest?;
    let limits = event.payload.rate_limits?;
    let detail = limits.credits.filter(|c| c.has_credits).and_then(|c| {
        if c.unlimited {
            Some("credits unlimited".into())
        } else {
            let amount = c.balance?.parse::<f64>().ok()?;
            (amount.is_finite() && amount >= 0.0).then(|| format!("cr {}", grouped(amount)))
        }
    });
    [limits.primary, limits.secondary]
        .into_iter()
        .flatten()
        .filter_map(|w| {
            window(
                w.used_percent,
                event.timestamp,
                DateTime::from_timestamp(w.resets_at, 0)?,
                now,
                (w.used_percent >= 100.0).then(|| detail.clone()).flatten(),
            )
        })
        .max_by(|a, b| a.used.total_cmp(&b.used))
}

/// Collect metadata in one directory pass. The catalog helper separately walks
/// directories and files, sorts every level, and stats files afterward; `ls`
/// needs only modification order. Refuse symlink entries as the catalog does.
fn collect_histories(
    root: &Path,
    files: &mut Vec<(std::time::SystemTime, std::path::PathBuf)>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            collect_histories(&path, files)?;
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                files.push((modified, path));
            }
        }
    }
    Ok(())
}

fn grouped(amount: f64) -> String {
    let digits = format!("{amount:.0}");
    let mut result = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result
}

fn codex_tail(path: &Path) -> Option<Event> {
    let mut file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let length = metadata.len();
    // Forks retain the source's token-count records. Their old percentages do
    // not describe the destination account. Birth time is conservative native
    // evidence until the new home writes a token-count observation of its own.
    let inherited = crate::codex_history::read_header(path)
        .ok()
        .flatten()
        .is_some_and(|header| !header["payload"]["forked_from_id"].is_null());
    let born = inherited
        .then(|| metadata.created().ok().map(DateTime::<Utc>::from))
        .flatten();
    // Native tool payloads may be huge. Bound each read to 256 KiB and each
    // history to 1 MiB; a figure beyond that budget is unknown, never guessed.
    let mut end = length;
    let floor = length.saturating_sub(1024 * 1024);
    let mut carry = Vec::new();
    while end > floor {
        let start = end.saturating_sub(256 * 1024).max(floor);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = vec![0; (end - start) as usize];
        file.read_exact(&mut bytes).ok()?;
        bytes.append(&mut carry);
        let first = if start > 0 {
            memchr::memchr(b'\n', &bytes).map_or(bytes.len(), |i| i + 1)
        } else {
            0
        };
        for line in bytes[first..].rsplit(|b| *b == b'\n') {
            if memchr::memmem::find(line, b"\"rate_limits\"").is_none() {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<Event>(line) else {
                continue;
            };
            if (!inherited || born.is_some_and(|at| event.timestamp >= at))
                && event.kind == "event_msg"
                && event.payload.kind == "token_count"
                && event
                    .payload
                    .rate_limits
                    .as_ref()
                    .is_some_and(|r| r.primary.is_some() || r.secondary.is_some())
            {
                return Some(event);
            }
        }
        carry = bytes[..first].to_vec();
        end = start;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::json;
    use std::io::Write;

    fn now() -> DateTime<Utc> {
        "2026-11-11T19:00:00Z".parse().unwrap()
    }
    fn put(home: &Path, relative: &str, value: serde_json::Value) {
        let path = home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string() + "\n").unwrap();
    }
    fn event(at: DateTime<Utc>, used: f64, reset: DateTime<Utc>) -> serde_json::Value {
        json!({"timestamp":at,"type":"event_msg","payload":{"type":"token_count","rate_limits":{
            "primary":{"used_percent":used,"resets_at":reset.timestamp()},"secondary":null,
            "credits":{"has_credits":true,"unlimited":false,"balance":"46741.241474"}}}})
    }

    #[test]
    fn claude_cache_selects_tighter_model_and_keeps_age_and_reset() {
        let temp = tempfile::tempdir().unwrap();
        let at = now() - Duration::hours(26);
        let reset = now() + Duration::days(2);
        put(
            temp.path(),
            ".claude.json",
            json!({"ignored_account":"do not retain", "cachedUsageUtilization":{
            "fetchedAtMs":at.timestamp_millis(),"utilization":{
            "seven_day":{"utilization":58,"resets_at":reset},"five_hour":null,
            "limits":[{"percent":64,"resets_at":reset,"is_active":true,"scope":{"model":{"display_name":"Fable"}}}]}}}),
        );
        let s = read(temp.path(), "claude", now()).unwrap();
        assert_eq!(s.left(), "36%");
        assert_eq!(s.detail.as_deref(), Some("Fable"));
        assert_eq!(s.observed, at);
        assert_eq!(s.reset, reset);
        assert_eq!(s.age(now()), "26h");
        assert_eq!(s.age(at + Duration::minutes(14)), "-");
        assert_eq!(s.age(at + Duration::minutes(15)), "15m");
        assert!(read(temp.path(), "claude", reset).is_none());
    }

    #[test]
    fn claude_short_window_can_be_tighter_and_equal_scoped_limit_is_not_highlighted() {
        let t = tempfile::tempdir().unwrap();
        let reset = now() + Duration::hours(1);
        put(
            t.path(),
            ".claude.json",
            json!({"cachedUsageUtilization":{"fetchedAtMs":now().timestamp_millis(),"utilization":{
            "seven_day":{"utilization":10,"resets_at":reset},
            "five_hour":{"utilization":80,"resets_at":reset},
            "limits":[{"percent":80,"resets_at":reset,"scope":{"model":{"display_name":"Fable"}}}]}}}),
        );
        let s = read(t.path(), "claude", now()).unwrap();
        assert_eq!(s.left(), "20%");
        assert!(s.detail.is_none());
    }

    #[test]
    fn codex_skips_null_rejection_and_shows_credits_only_when_saturated() {
        let t = tempfile::tempdir().unwrap();
        let path = "sessions/2026/11/11/rollout-fixture.jsonl";
        let at = now() - Duration::hours(1);
        let reset = now() + Duration::hours(2);
        put(t.path(), path, event(at, 100.0, reset));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(t.path().join(path))
            .unwrap();
        writeln!(file,"{}", json!({"timestamp":now(),"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"premium","primary":null,"secondary":null}}})).unwrap();
        // Include a partial native write; the preceding complete snapshot wins.
        write!(file, "{{\"rate_limits\":").unwrap();
        let s = read(t.path(), "codex", now()).unwrap();
        assert_eq!(s.left(), "0%");
        assert_eq!(s.detail.as_deref(), Some("cr 46,741"));
        assert_eq!(s.observed, at);
        assert_eq!(s.age(now()), "1h");
        assert_eq!(s.reset, reset);
        assert!(read(t.path(), "codex", reset).is_none());
        put(t.path(), path, event(at, 20.0, reset));
        let s = read(t.path(), "codex", now()).unwrap();
        assert_eq!(s.left(), "80%");
        assert!(s.detail.is_none());
        let mut e = event(at, 100.0, reset);
        e["payload"]["rate_limits"]["credits"]["has_credits"] = json!(false);
        put(t.path(), path, e);
        assert!(read(t.path(), "codex", now()).unwrap().detail.is_none());
    }

    #[test]
    fn codex_secondary_window_and_archived_histories_are_considered() {
        let t = tempfile::tempdir().unwrap();
        let mut e = event(now(), 10.0, now() + Duration::hours(1));
        e["payload"]["rate_limits"]["secondary"] =
            json!({"used_percent":90,"resets_at":(now()+Duration::days(1)).timestamp()});
        put(t.path(), "archived_sessions/rollout-fixture.jsonl", e);
        let s = read(t.path(), "codex", now()).unwrap();
        assert_eq!(s.left(), "10%");
        assert_eq!(s.reset, now() + Duration::days(1));
    }

    #[test]
    fn tail_boundary_and_bounded_unknown_do_not_invent_figures() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("history");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "{}", event(now(), 10.0, now() + Duration::days(1))).unwrap();
        file.write_all(&vec![b'x'; 300_000]).unwrap();
        assert!(codex_tail(&path).is_some());
        file.write_all(&vec![b'x'; 1024 * 1024]).unwrap();
        assert!(codex_tail(&path).is_none());
        assert!(read(t.path(), "claude", now()).is_none());
        put(
            t.path(),
            ".claude.json",
            json!({"cachedUsageUtilization":{"fetchedAtMs":0,"utilization":{"seven_day":{"utilization":80,"resets_at":now()-Duration::seconds(1)},"five_hour":null}}}),
        );
        assert!(read(t.path(), "claude", now()).is_none());
    }
    #[test]
    fn copied_codex_usage_is_not_attributed_to_the_destination() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("fork.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"session_meta","payload":{"forked_from_id":"source"}})
        )
        .unwrap();
        let at = Utc::now() - Duration::hours(1);
        writeln!(file, "{}", event(at, 100.0, at + Duration::days(1))).unwrap();
        assert!(codex_tail(&path).is_none());
        writeln!(
            file,
            "{}",
            event(Utc::now(), 10.0, Utc::now() + Duration::days(1))
        )
        .unwrap();
        assert_eq!(
            codex_tail(&path)
                .unwrap()
                .payload
                .rate_limits
                .unwrap()
                .primary
                .unwrap()
                .used_percent,
            10.0
        );
    }
}
