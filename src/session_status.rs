//! Bounded native transcript inspection for watchdogs and launch preflight.
//! The watchdog sees the newest turn's limit. Quota also learns recovery from
//! owned successes in the recent tail. Copied bytes can supply settings only.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::Config,
    conversations::{Conversation, ConversationKey},
    limits::{ModelFamily, SuccessWatermarks},
    paths::Paths,
};

const CHUNK: u64 = 64 * 1024;
const RECOVERY_TAIL_BYTES: u64 = 1024 * 1024;
pub(crate) const UNKNOWN_HOLD_SECONDS: i64 = 15 * 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limit {
    pub kind: String,
    pub at: DateTime<Utc>,
    pub resets_at: Option<DateTime<Utc>>,
    pub active: bool,
    pub message: String,
    /// Preserve the native classification without making it part of the API.
    #[serde(skip)]
    pub native_kind: Option<String>,
    #[serde(skip)]
    pub(crate) family: Option<ModelFamily>,
}

/// Machine-facing session identity and settings; facts used only by scheduling
/// stay private so v1 clients never need to understand native transcript shapes.
#[derive(Clone, Debug, Serialize)]
pub struct SessionStatus {
    pub schema_version: u8,
    pub tool: String,
    pub profile: String,
    pub id: String,
    pub cwd: std::path::PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub limit: Option<Limit>,
    #[serde(skip)]
    pub(crate) successes: SuccessWatermarks,
}

/// Filename-only discovery avoids loading catalogs and multi-megabyte transcripts.
pub fn lookup(
    paths: &Paths,
    id: &str,
    tool_filter: Option<&str>,
    profile_filter: Option<&str>,
) -> Result<SessionStatus> {
    if id.is_empty() || id.contains(['/', '\\']) || matches!(id, "." | "..") {
        bail!("unknown session ID");
    }
    let config = Config::load(&paths.config_file())?;
    let mut found = None;
    for (tool, settings) in &config.tools {
        if !matches!(tool.as_str(), "claude" | "codex") || tool_filter.is_some_and(|t| t != tool) {
            continue;
        }
        for profile in settings.profiles.keys() {
            if profile_filter.is_some_and(|p| p != profile) {
                continue;
            }
            let home = paths.profile_home_dir(tool, profile);
            let key = ConversationKey::new(tool, profile, id);
            let Some(path) = crate::conversations::locate_transcript(&home, &key)? else {
                continue;
            };
            if found.is_some() {
                bail!("session ID is present in multiple profiles; pass -t and -p");
            }
            found = Some(inspect(paths, tool, profile, id, &path)?);
        }
    }
    found.context("unknown session ID or session started outside rtr")
}

pub(crate) fn for_conversation(paths: &Paths, c: &Conversation) -> Result<SessionStatus> {
    inspect(paths, &c.tool, &c.profile, &c.id, &c.transcript_path)
}

pub(crate) fn inspect(
    paths: &Paths,
    tool: &str,
    profile: &str,
    id: &str,
    path: &Path,
) -> Result<SessionStatus> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let born = metadata.created().ok().map(DateTime::<Utc>::from);
    let origin = crate::quota::origin(paths, tool, profile, id)?;
    let inherited = origin.as_ref().filter(|o| o.path == path).map(|o| o.bytes);
    let now = Utc::now();
    let identified = tool != "codex"
        || crate::codex_history::read_header(path)?
            .is_some_and(|header| header["payload"]["id"] == id);
    // Old RTR copies have no byte boundary. Birth time is the available native
    // evidence: a historical error predating this file cannot belong to its home.
    // New copies use exact byte offsets, including records with clock skew.
    let owned = |offset: u64, at: DateTime<Utc>| -> bool {
        if !identified || at > now {
            return false;
        }
        if let Some(bytes) = inherited {
            return offset >= bytes;
        }
        let cutoff = origin.as_ref().map(|o| o.at).or(born);
        cutoff.is_some_and(|cutoff| at >= cutoff)
    };
    let mut result = SessionStatus {
        schema_version: 1,
        tool: tool.into(),
        profile: profile.into(),
        id: id.into(),
        cwd: Default::default(),
        model: None,
        effort: None,
        limit: None,
        successes: SuccessWatermarks::default(),
    };
    let mut head = vec![0; metadata.len().min(CHUNK) as usize];
    file.read_exact(&mut head)?;
    for line in head.split(|b| *b == b'\n') {
        let Ok(r) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let cwd = if tool == "codex" {
            &r["payload"]["cwd"]
        } else {
            &r["cwd"]
        };
        if let Some(cwd) = cwd.as_str() {
            result.cwd = cwd.into();
            break;
        }
    }
    let mut end = metadata.len();
    // Resolve settings as far back as before. Look beyond the last model only
    // in the recent owned tail, where each family can supply its own recovery.
    let recovery_floor = end
        .saturating_sub(RECOVERY_TAIL_BYTES)
        .max(inherited.unwrap_or_default());
    let mut carry = Vec::new();
    let mut newest_decided = false;
    let mut failing_turn = None::<String>;
    let mut searching_window = false;
    'tail: while end > 0 {
        let start = end.saturating_sub(CHUNK);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; (end - start) as usize];
        file.read_exact(&mut bytes)?;
        bytes.append(&mut carry);
        let first = if start > 0 {
            bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |p| p + 1)
        } else {
            0
        };
        let mut line_end = start + bytes.len() as u64;
        for line in bytes[first..].rsplit(|b| *b == b'\n') {
            let offset = line_end - line.len() as u64;
            line_end = offset.saturating_sub(1);
            if offset < recovery_floor
                && newest_decided
                && result.model.is_some()
                && !searching_window
            {
                break 'tail;
            }
            if line.is_empty() {
                continue;
            }
            // Tool payloads can be enormous. Only deserialize native envelopes
            // relevant to settings/turn completion; DOM and command output do
            // not need to become a JSON tree during watchdog checks.
            if ![
                b"\"assistant\"".as_slice(),
                b"\"user\"",
                b"\"turn_context\"",
                b"\"event_msg\"",
            ]
            .iter()
            .any(|s| memchr::memmem::find(line, s).is_some())
            {
                continue;
            }
            let Ok(r) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if result.cwd.as_os_str().is_empty() {
                if let Some(cwd) = r["cwd"].as_str().or_else(|| r["payload"]["cwd"].as_str()) {
                    result.cwd = cwd.into();
                }
            }
            let at = r
                .get("timestamp")
                .and_then(crate::conversations::parse_timestamp);
            let owned_at = at.filter(|at| owned(offset, *at));
            if tool == "claude" {
                let real = r["type"] == "assistant"
                    && r["message"]["model"]
                        .as_str()
                        .is_some_and(|m| m != "<synthetic>")
                    && r["isApiErrorMessage"] != true;
                if real {
                    if result.model.is_none() {
                        // requestedModel preserves explicit context variants when
                        // native Claude records them; message.model is canonical.
                        result.model = r["requestedModel"]
                            .as_str()
                            .filter(|m| m.contains("[1m]"))
                            .or_else(|| r["message"]["model"].as_str())
                            .map(str::to_string);
                        result.effort = r["effort"]
                            .as_str()
                            .or_else(|| r["perTurnEffort"].as_str())
                            .map(str::to_string);
                    }
                    if let Some(at) = owned_at.filter(|_| r["apiError"].is_null()) {
                        let family = r["message"]["model"]
                            .as_str()
                            .map(ModelFamily::from_claude_model);
                        if let Some(family) =
                            family.filter(|family| *family != ModelFamily::Unknown)
                        {
                            let latest = result.successes.models.entry(family).or_insert(at);
                            *latest = (*latest).max(at);
                        } else {
                            result.successes.common = result.successes.common.max(Some(at));
                        }
                    }
                }
                if !newest_decided && matches!(r["type"].as_str(), Some("user" | "assistant")) {
                    newest_decided = true;
                    if owned_at.is_some() {
                        result.limit = classify_limit(tool, &r, now);
                    }
                }
            } else {
                let p = &r["payload"];
                if r["type"] == "turn_context" && result.model.is_none() {
                    result.model = p["model"].as_str().map(str::to_string);
                    result.effort = p["effort"].as_str().map(str::to_string);
                }
                if r["type"] == "event_msg" {
                    if p["type"] == "task_complete" && p["error"].is_null() {
                        result.successes.common = result.successes.common.max(owned_at);
                    }
                    if matches!(
                        p["type"].as_str(),
                        Some("task_complete" | "task_started" | "turn_aborted")
                    ) {
                        if newest_decided {
                            searching_window = false;
                        } else {
                            newest_decided = true;
                            if owned_at.is_some() {
                                result.limit = classify_limit(tool, &r, now);
                                searching_window = result.limit.is_some();
                                failing_turn = p["turn_id"].as_str().map(str::to_string);
                            }
                        }
                    } else if searching_window && p["type"] == "token_count" {
                        let limit = result
                            .limit
                            .as_mut()
                            .filter(|limit| owned_at.is_some_and(|at| at <= limit.at));
                        if let (Some(limit), Some(reset)) =
                            (limit, saturated_reset(&p["rate_limits"]))
                        {
                            limit.resets_at = Some(reset);
                            searching_window = false;
                        }
                    }
                }
                if searching_window
                    && r["type"] == "turn_context"
                    && (failing_turn.is_none() || p["turn_id"].as_str() == failing_turn.as_deref())
                {
                    searching_window = false;
                }
            }
        }
        carry = bytes[..first].to_vec();
        end = start;
        if end <= recovery_floor && newest_decided && result.model.is_some() && !searching_window {
            break;
        }
    }
    if let Some(limit) = &mut result.limit {
        limit.active = limit.resets_at.is_none_or(|at| at > now);
    }
    Ok(result)
}

/// Recognize only structured terminal usage errors. Message text is display-only
/// for Claude; Codex needs it solely as a fallback for missing reset windows.
pub fn classify_limit(tool: &str, record: &Value, now: DateTime<Utc>) -> Option<Limit> {
    let (kind, native_kind, reset, message) = if tool == "claude" {
        if record["apiError"] != "usage_limit_reached" {
            return None;
        }
        let native = record["quotaLimits"]["rateLimitType"].as_str();
        let kind = match native {
            Some("seven_day") => "weekly",
            Some("five_hour") => "five_hour",
            Some(s) => s,
            None => "usage",
        };
        (
            kind.to_string(),
            native.map(str::to_string),
            record["quotaLimits"]["resetsAt"]
                .as_i64()
                .and_then(|s| DateTime::from_timestamp(s, 0)),
            content_text(&record["message"]["content"]),
        )
    } else {
        let p = &record["payload"];
        if record["type"] != "event_msg"
            || p["type"] != "task_complete"
            || p["error"]["codex_error_info"] != "usage_limit_exceeded"
        {
            return None;
        }
        let message = p["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let at = record
            .get("timestamp")
            .and_then(crate::conversations::parse_timestamp)
            .unwrap_or(now);
        (
            "usage".into(),
            None,
            parse_message_reset(&message, at),
            message,
        )
    };
    let at = record
        .get("timestamp")
        .and_then(crate::conversations::parse_timestamp)?;
    let mut limit = Limit {
        kind,
        native_kind,
        at,
        resets_at: reset,
        active: true,
        message,
        family: (tool == "claude")
            .then(|| claude_limit_family(&record["quotaLimits"]))
            .flatten(),
    };
    limit.active = limit.resets_at.is_none_or(|at| at > now);
    Some(limit)
}

fn claude_limit_family(quota: &Value) -> Option<ModelFamily> {
    // A scoped kind must carry its own model identity. Extra scope dimensions
    // make it ambiguous, even if the record also identifies a Claude family.
    if quota["rateLimitType"] != "weekly_scoped" {
        return None;
    }
    let scope = quota["scope"].as_object()?;
    if scope
        .iter()
        .any(|(key, value)| key != "model" && !value.is_null())
    {
        return None;
    }
    let model = scope.get("model")?;
    match model["id"].as_str() {
        Some(id) => Some(ModelFamily::from_claude_model(id))
            .filter(|family| *family != ModelFamily::Unknown),
        None if model["id"].is_null() && model["display_name"] == "Fable" => {
            Some(ModelFamily::Fable)
        }
        None => None,
    }
}

fn content_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    content
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn saturated_reset(limits: &Value) -> Option<DateTime<Utc>> {
    if let Some(a) = limits.as_array() {
        return a.iter().filter_map(saturated_reset).max();
    }
    if limits["limit_id"] != "codex" {
        return None;
    }
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| {
            let w = &limits[key];
            (w["used_percent"].as_f64()? >= 100.0)
                .then(|| {
                    w["resets_at"]
                        .as_i64()
                        .and_then(|s| DateTime::from_timestamp(s, 0))
                })
                .flatten()
        })
        .max()
}

fn parse_message_reset(message: &str, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let text = message
        .split_once("try again at ")?
        .1
        .trim()
        .trim_end_matches('.');
    let cleaned = text
        .split_whitespace()
        .map(|part| {
            if part.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                part.replace("st,", ",")
                    .replace("nd,", ",")
                    .replace("rd,", ",")
                    .replace("th,", ",")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let local = at.with_timezone(&Local);
    let naive = NaiveDateTime::parse_from_str(&cleaned, "%b %d, %Y %I:%M %p")
        .ok()
        .or_else(|| {
            let time = NaiveTime::parse_from_str(&cleaned, "%I:%M %p").ok()?;
            let mut date = local.date_naive();
            if time < local.time() {
                date = date.succ_opt()?;
            }
            Some(date.and_time(time))
        })?;
    // A message has no UTC offset. For a DST fold choose the first occurrence
    // still after the failing turn; a nonexistent local time stays unknown.
    let mapped = Local.from_local_datetime(&naive);
    mapped
        .earliest()
        .filter(|t| t.with_timezone(&Utc) >= at)
        .or_else(|| mapped.latest())
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_structured_usage_markers_count() {
        let now = Utc::now();
        for native in ["seven_day", "five_hour", "future_window"] {
            let r = json!({"timestamp":now,"apiError":"usage_limit_reached","quotaLimits":{"rateLimitType":native,"resetsAt":now.timestamp()+60},"message":{"content":[{"text":"display"}]}});
            let limit = classify_limit("claude", &r, now).unwrap();
            assert!(limit.active);
            assert_eq!(limit.native_kind.as_deref(), Some(native));
        }
        for error in ["server_error", "authentication_failed", "invalid_request"] {
            assert!(classify_limit("claude", &json!({"timestamp":now,"apiError":error,"isApiErrorMessage":true,"error":"rate_limit"}), now).is_none());
        }
        assert!(classify_limit("codex", &json!({"timestamp":now,"type":"event_msg","payload":{"type":"error","error":{"codex_error_info":"usage_limit_exceeded"}}}), now).is_none());
    }

    #[test]
    fn codex_message_dates_time_only_and_unknown_hold() {
        let now = Local
            .with_ymd_and_hms(2026, 11, 11, 8, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let expected = Local
            .with_ymd_and_hms(2026, 11, 11, 8, 53, 0)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            parse_message_reset("try again at Nov 11th, 2026 8:53 AM.", now),
            Some(expected)
        );
        assert_eq!(
            parse_message_reset("try again at 8:53 AM.", now),
            Some(expected)
        );
        assert!(parse_message_reset("missing", now).is_none());
        let r = json!({"timestamp":now,"type":"event_msg","payload":{"type":"task_complete","error":{"codex_error_info":"usage_limit_exceeded","message":"unknown"}}});
        assert!(classify_limit("codex", &r, now).unwrap().active);
        let unknown = classify_limit("codex", &r, now + chrono::Duration::minutes(16)).unwrap();
        // The watchdog cannot know an unknown reset has passed. Only the
        // profile scheduler's hold expires; the session signal stays active.
        assert!(unknown.active);
        assert_eq!(unknown.resets_at, None);
    }

    #[test]
    fn saturated_codex_window_only() {
        let reset = Utc::now().timestamp() + 600;
        assert_eq!(
            saturated_reset(
                &json!({"limit_id":"codex","secondary":{"used_percent":100,"resets_at":reset}})
            )
            .unwrap()
            .timestamp(),
            reset
        );
        for limits in [
            json!({"limit_id":"premium","secondary":{"used_percent":100,"resets_at":reset}}),
            json!({"limit_id":"codex","primary":{"used_percent":99,"resets_at":reset}}),
        ] {
            assert!(saturated_reset(&limits).is_none());
        }
    }
}
