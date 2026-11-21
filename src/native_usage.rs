//! Read native subscription evidence from bounded local caches and rollout
//! tails. A profile owns its cache and only the observations it wrote itself.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    limits::{
        CodexSlot, CreditInfo, Meter, MeterKey, MeterScope, MeterSource, MeterWindow, ModelFamily,
        Percent, FRESHNESS_MINUTES,
    },
    quota::OriginIndex,
};

/// A home-local, non-persisted snapshot for listing and routing.
#[derive(Debug, Default, PartialEq)]
pub struct NativeRead {
    pub meters: Vec<Meter>,
    pub credit_info: Option<CreditInfo>,
}

pub fn read_owned(
    home: &Path,
    tool: &str,
    origin_index: &OriginIndex,
    now: DateTime<Utc>,
) -> NativeRead {
    match tool {
        "claude" => claude(home, now).unwrap_or_default(),
        "codex" => codex(home, origin_index, now).unwrap_or_default(),
        _ => NativeRead::default(),
    }
}

/// Projection for the existing one-window listing. Expired and reset-less
/// evidence remains in NativeRead for callers that can render it.
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
            m if m < FRESHNESS_MINUTES => "-".into(),
            15..=59 => format!("{minutes}m"),
            60..=2879 => format!("{}h", minutes / 60),
            _ => format!("{}d", minutes / 1440),
        }
    }
}

impl NativeRead {
    pub(crate) fn tightest(&self, now: DateTime<Utc>) -> Option<Snapshot> {
        let meter = self
            .meters
            .iter()
            .filter(|meter| {
                meter.reset_at.is_some_and(|reset| reset > now)
                    && (meter.source == MeterSource::ClaudeCache
                        || meter.key.scope == MeterScope::AllModels)
            })
            .max_by(|a, b| {
                a.used_percent
                    .get()
                    .total_cmp(&b.used_percent.get())
                    .then_with(|| {
                        (a.key.scope == MeterScope::AllModels)
                            .cmp(&(b.key.scope == MeterScope::AllModels))
                    })
            })?;
        let used = meter.used_percent.get();
        let detail = match (&meter.source, &meter.key.scope) {
            (MeterSource::ClaudeCache, MeterScope::Model(family)) => Some(format!("{family:?}")),
            (MeterSource::ClaudeCache, MeterScope::Unclassified(label)) => {
                Some(crate::output::clean(label))
            }
            (MeterSource::CodexRollout { .. }, _) if used == 100.0 => self
                .credit_info
                .as_ref()
                .filter(|credit| credit.has_credits)
                .and_then(|credit| {
                    if credit.unlimited {
                        Some("credits unlimited".into())
                    } else {
                        credit
                            .balance
                            .map(|amount| format!("cr {}", grouped(amount)))
                    }
                }),
            _ => None,
        };
        Some(Snapshot {
            used,
            observed: meter.observed_at,
            reset: meter.reset_at?,
            detail,
        })
    }
}

// Only the usage block enters the model. Serde skips unrelated home fields.
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
    #[serde(default)]
    five_hour: Value,
    #[serde(default)]
    seven_day: Value,
    #[serde(default)]
    limits: Value,
}

#[derive(Default)]
struct ClaudeFields {
    used: Option<Percent>,
    reset: Option<DateTime<Utc>>,
}

fn claude(home: &Path, now: DateTime<Utc>) -> Option<NativeRead> {
    let file = File::open(home.join(".claude.json")).ok()?;
    let mut bytes = Vec::new();
    file.take(2 * 1024 * 1024).read_to_end(&mut bytes).ok()?;
    let cache = serde_json::from_slice::<ClaudeHome>(&bytes).ok()?.cache?;
    let at = DateTime::from_timestamp_millis(cache.fetched_at)?;
    if at > now {
        return None;
    }
    let usage = cache.utilization;
    // Merge representations by window and scope within this fetch. Native
    // resets can differ at sub-millisecond precision for the same meter.
    let mut fields = BTreeMap::<MeterKey, ClaudeFields>::new();
    for row in usage.limits.as_array().into_iter().flatten() {
        let Some(key) = claude_key(row) else { continue };
        let meter = fields.entry(key).or_default();
        meter.used = meter.used.or_else(|| percent(&row["percent"]));
        meter.reset = meter.reset.or_else(|| claude_reset(row, at));
    }
    for (window, row) in [
        (MeterWindow::FiveHour, &usage.five_hour),
        (MeterWindow::Week, &usage.seven_day),
    ] {
        let meter = fields
            .entry(MeterKey {
                window,
                scope: MeterScope::AllModels,
            })
            .or_default();
        meter.used = meter.used.or_else(|| percent(&row["utilization"]));
        meter.reset = meter.reset.or_else(|| claude_reset(row, at));
    }
    let meters = fields
        .into_iter()
        .filter_map(|(key, fields)| {
            Some(Meter {
                key,
                used_percent: fields.used?,
                reset_at: fields.reset,
                observed_at: at,
                source: MeterSource::ClaudeCache,
            })
        })
        .collect();
    Some(NativeRead {
        meters,
        credit_info: None,
    })
}

fn percent(value: &Value) -> Option<Percent> {
    Percent::new(value.as_f64()?)
}

fn claude_reset(row: &Value, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    row["resets_at"]
        .as_str()?
        .parse()
        .ok()
        .filter(|reset| *reset > at)
}

fn claude_key(row: &Value) -> Option<MeterKey> {
    let kind = row["kind"].as_str();
    let group = row["group"].as_str();
    let scope = &row["scope"];
    let model = &scope["model"];
    let identity = group.or(kind);
    if identity.is_none() && scope.is_null() {
        return None;
    }
    let labels = [kind, group];
    let week = labels
        .iter()
        .flatten()
        .any(|label| matches!(*label, "weekly" | "weekly_all" | "weekly_scoped"));
    let window = if week {
        MeterWindow::Week
    } else if labels.contains(&Some("session")) {
        MeterWindow::FiveHour
    } else {
        MeterWindow::Native(identity.unwrap_or("limit").into())
    };
    let id = model["id"].as_str();
    let display = model["display_name"].as_str();
    let surface = &scope["surface"];
    let model_only = scope.as_object().is_some_and(|values| {
        values
            .iter()
            .all(|(key, value)| key == "model" || value.is_null())
    });
    let no_scope = scope.is_null() || (model_only && model.is_null());
    let family = id.map(ModelFamily::from_claude_model).or_else(|| {
        (model["id"].is_null() && display == Some("Fable")).then_some(ModelFamily::Fable)
    });
    let scope =
        if no_scope && (labels.contains(&Some("weekly_all")) || window == MeterWindow::FiveHour) {
            MeterScope::AllModels
        } else if let Some(family) =
            family.filter(|family| model_only && *family != ModelFamily::Unknown)
        {
            MeterScope::Model(family)
        } else {
            let label = [
                id.or(display),
                surface
                    .as_str()
                    .or_else(|| surface["id"].as_str())
                    .or_else(|| surface["display_name"].as_str()),
                scope.as_str().or_else(|| scope["type"].as_str()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" / ");
            MeterScope::Unclassified(if label.is_empty() {
                identity.unwrap_or("unknown").into()
            } else {
                label
            })
        };
    Some(MeterKey { window, scope })
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
    rate_limits: Value,
}

const CODEX_SUBSCRIPTION: &str = "codex";

/// Each limit ID keeps its newest whole observation. Slots and credits from
/// different events can describe different windows or subscription states.
type CodexReads = BTreeMap<Option<String>, (DateTime<Utc>, NativeRead)>;

fn codex(home: &Path, origins: &OriginIndex, now: DateTime<Utc>) -> Option<NativeRead> {
    let mut files = Vec::new();
    for root in [home.join("sessions"), home.join("archived_sessions")] {
        collect_histories(&root, &mut files).ok()?;
    }
    files.sort_unstable_by(|a, b| b.cmp(a));
    let mut latest = CodexReads::new();
    let subscription = Some(CODEX_SUBSCRIPTION.to_owned());
    // Recent writers win timestamp ties. Each limit ID has its own latest
    // observation, so a subscription reading cannot prune other rollouts.
    for (_, path) in files {
        let _ = codex_tail(&path, home, origins, now, &mut latest);
    }
    let mut result = latest
        .remove(&subscription)
        .map(|(_, read)| read)
        .unwrap_or_default();
    result
        .meters
        .extend(latest.into_values().flat_map(|(_, read)| read.meters));
    Some(result)
}

fn codex_observation(row: &Value, at: DateTime<Utc>, scope: MeterScope) -> NativeRead {
    let meters = [
        (CodexSlot::Primary, "primary"),
        (CodexSlot::Secondary, "secondary"),
    ]
    .into_iter()
    .filter_map(|(slot, label)| {
        let window = &row[label];
        let duration = match window["window_minutes"].as_u64() {
            Some(300) => MeterWindow::FiveHour,
            Some(10_080) => MeterWindow::Week,
            Some(minutes) => MeterWindow::Minutes(minutes),
            None => MeterWindow::Native(label.into()),
        };
        Some(Meter {
            key: MeterKey {
                window: duration,
                scope: scope.clone(),
            },
            used_percent: percent(&window["used_percent"])?,
            reset_at: window["resets_at"]
                .as_i64()
                .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
                .filter(|reset| *reset > at),
            observed_at: at,
            source: MeterSource::CodexRollout { slot },
        })
    })
    .collect();
    let credit = &row["credits"];
    let credit_info = credit.as_object().map(|_| CreditInfo {
        has_credits: credit["has_credits"].as_bool().unwrap_or(false),
        unlimited: credit["unlimited"].as_bool().unwrap_or(false),
        balance: credit["balance"]
            .as_f64()
            .or_else(|| credit["balance"].as_str()?.parse().ok())
            .filter(|amount| amount.is_finite() && *amount >= 0.0),
    });
    NativeRead {
        meters,
        credit_info,
    }
}

fn observe_codex(event: Event, latest: &mut CodexReads) {
    let at = event.timestamp;
    let limits = &event.payload.rate_limits;
    let rows = limits
        .as_array()
        .map_or(std::slice::from_ref(limits), Vec::as_slice);
    for row in rows {
        let id = row["limit_id"].as_str().map(str::to_owned);
        if latest.get(&id).is_some_and(|(seen, _)| *seen >= at) {
            continue;
        }
        let scope = if id.as_deref() == Some(CODEX_SUBSCRIPTION) {
            MeterScope::AllModels
        } else {
            MeterScope::Unclassified(id.clone().unwrap_or_else(|| "unknown limit ID".into()))
        };
        let read = codex_observation(row, at, scope);
        // A rejection token_count can carry null slots. Holds come from the
        // rejection itself, so this event does not erase the last measurement.
        if !read.meters.is_empty() {
            latest.insert(id, (at, read));
        }
    }
}

fn codex_tail(
    path: &Path,
    home: &Path,
    origins: &OriginIndex,
    now: DateTime<Utc>,
    latest: &mut CodexReads,
) -> Option<()> {
    let mut file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let header = crate::codex_history::read_header(path).ok()??;
    let payload = &header["payload"];
    let id = payload["id"].as_str().filter(|id| !id.is_empty())?;
    let origin = origins.for_session(home, id);
    let copied_bytes = origins.copy_boundary(path);
    let copied = origin.is_some() || !payload["forked_from_id"].is_null();
    let cutoff = origin.map(|origin| origin.at).or_else(|| {
        copied
            .then(|| metadata.created().ok().map(DateTime::<Utc>::from))
            .flatten()
    });
    // Exact offsets also suppress copied records with newer timestamps. A
    // moved or native copy has only time-based provenance. Read neither a
    // parent rollout nor a file whose bounded header cannot identify its owner.
    let owned = |offset, at| match copied_bytes {
        Some(bytes) => offset >= bytes,
        None => !copied || cutoff.is_some_and(|cutoff| at >= cutoff),
    };
    let mut end = metadata.len();
    let floor = end.saturating_sub(1024 * 1024);
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
        let mut line_end = bytes.len();
        for line in bytes[first..].rsplit(|b| *b == b'\n') {
            let line_start = line_end - line.len();
            line_end = line_start.saturating_sub(1);
            if copied_bytes.is_some_and(|inherited| start + (line_start as u64) < inherited) {
                return Some(());
            }
            if memchr::memmem::find(line, b"\"rate_limits\"").is_none() {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<Event>(line) else {
                continue;
            };
            if event.timestamp <= now
                && owned(start + line_start as u64, event.timestamp)
                && event.kind == "event_msg"
                && event.payload.kind == "token_count"
            {
                observe_codex(event, latest);
            }
        }
        carry = bytes[..first].to_vec();
        end = start;
    }
    Some(())
}

/// Collect modification times in one pass. Ignore symlink entries as the
/// conversation catalog does.
fn collect_histories(root: &Path, files: &mut Vec<(SystemTime, PathBuf)>) -> std::io::Result<()> {
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
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::json;
    use std::io::Write;

    fn read(home: &Path, tool: &str, now: DateTime<Utc>) -> Option<Snapshot> {
        read_owned(home, tool, &OriginIndex::default(), now).tightest(now)
    }

    fn now() -> DateTime<Utc> {
        "2026-11-11T19:00:00Z".parse().unwrap()
    }
    fn put(home: &Path, relative: &str, value: serde_json::Value) {
        let path = home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let prefix =
            if relative.starts_with("sessions/") || relative.starts_with("archived_sessions/") {
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"fixture\"}}\n"
            } else {
                ""
            };
        std::fs::write(path, format!("{prefix}{value}\n")).unwrap();
    }
    fn event(at: DateTime<Utc>, used: f64, reset: DateTime<Utc>) -> serde_json::Value {
        json!({"timestamp":at,"type":"event_msg","payload":{"type":"token_count","rate_limits":{
            "limit_id":"codex","primary":{"used_percent":used,"resets_at":reset.timestamp()},"secondary":null,
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
        writeln!(
            file,
            "{}",
            json!({"type":"session_meta","payload":{"id":"fixture"}})
        )
        .unwrap();
        writeln!(file, "{}", event(now(), 10.0, now() + Duration::days(1))).unwrap();
        file.write_all(&vec![b'x'; 300_000]).unwrap();
        let mut latest = CodexReads::new();
        codex_tail(&path, t.path(), &OriginIndex::default(), now(), &mut latest);
        assert!(!latest.is_empty());
        file.write_all(&vec![b'x'; 1024 * 1024]).unwrap();
        latest.clear();
        codex_tail(&path, t.path(), &OriginIndex::default(), now(), &mut latest);
        assert!(latest.is_empty());
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
            json!({"type":"session_meta","payload":{"id":"fork","forked_from_id":"source"}})
        )
        .unwrap();
        let at = Utc::now() - Duration::hours(1);
        writeln!(file, "{}", event(at, 100.0, at + Duration::days(1))).unwrap();
        let mut latest = CodexReads::new();
        codex_tail(
            &path,
            t.path(),
            &OriginIndex::default(),
            Utc::now(),
            &mut latest,
        );
        assert!(latest.is_empty());
        writeln!(
            file,
            "{}",
            event(Utc::now(), 10.0, Utc::now() + Duration::days(1))
        )
        .unwrap();
        codex_tail(
            &path,
            t.path(),
            &OriginIndex::default(),
            Utc::now(),
            &mut latest,
        );
        let (_, read) = latest.get(&Some(CODEX_SUBSCRIPTION.to_owned())).unwrap();
        assert_eq!(read.meters[0].used_percent.get(), 10.0);
    }
    #[test]
    fn oversized_or_missing_headers_do_not_claim_foreign_usage() {
        let t = tempfile::tempdir().unwrap();
        let path = "sessions/rollout-owned.jsonl";
        put(
            t.path(),
            path,
            event(now() - Duration::hours(1), 30.0, now() + Duration::days(1)),
        );
        let foreign = t.path().join("sessions/rollout-foreign.jsonl");
        let mut file = File::create(&foreign).unwrap();
        writeln!(file, "{}", json!({"type":"session_meta","payload":{"forked_from_id":"source","base_instructions":"x".repeat(300_000)}})).unwrap();
        writeln!(
            file,
            "{}",
            event(
                now() - Duration::minutes(5),
                95.0,
                now() + Duration::days(1)
            )
        )
        .unwrap();
        assert_eq!(read(t.path(), "codex", now()).unwrap().left(), "70%");
        // A malformed/missing metadata envelope is equally unable to establish
        // whether the token counts belong to this native home.
        std::fs::write(
            &foreign,
            event(now(), 95.0, now() + Duration::days(1)).to_string(),
        )
        .unwrap();
        assert_eq!(read(t.path(), "codex", now()).unwrap().left(), "70%");
    }
}
