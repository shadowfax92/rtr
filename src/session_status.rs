//! Bounded native transcript inspection for watchdogs and launch routing.
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

/// Conversation settings can come from copied history. Ownership matters
/// only for quota evidence, which routing gathers separately with its index.
pub(crate) fn for_conversation(c: &Conversation) -> Result<SessionStatus> {
    inspect_tail(
        &c.tool,
        &c.profile,
        &c.id,
        &c.transcript_path,
        Ownership::SettingsOnly,
    )
}

pub(crate) fn inspect(
    paths: &Paths,
    tool: &str,
    profile: &str,
    id: &str,
    path: &Path,
) -> Result<SessionStatus> {
    let origin = crate::quota::origin(paths, tool, profile, id)?;
    inspect_with_origin(tool, profile, id, path, origin.as_ref())
}

pub(crate) fn inspect_with_origin(
    tool: &str,
    profile: &str,
    id: &str,
    path: &Path,
    origin: Option<&crate::quota::Origin>,
) -> Result<SessionStatus> {
    inspect_tail(tool, profile, id, path, Ownership::Isolated(origin))
}

/// The child exit can report an account limit without knowing which session to
/// fork. Replay requires a separately correlated native identity.
pub(crate) struct LaunchLimit {
    pub id: Option<String>,
    pub limit: Limit,
    pub can_rerun: bool,
}

/// Exact native identity can authorize a replay. An interactive Codex launch
/// can only attribute newly written limits to its selected home.
pub(crate) enum LaunchTranscript<'a> {
    Known { id: &'a str, check_rerun: bool },
    InteractiveCodex { cwd: Option<&'a Path> },
}

pub(crate) fn learn_launch_exit(
    paths: &Paths,
    tool: &str,
    profile: &str,
    window: std::ops::RangeInclusive<DateTime<Utc>>,
    transcript: LaunchTranscript<'_>,
) -> Result<Option<LaunchLimit>> {
    let home = paths.profile_home_dir(tool, profile);
    let cwd = match transcript {
        LaunchTranscript::InteractiveCodex { cwd } => cwd,
        LaunchTranscript::Known { id, check_rerun } => {
            let key = ConversationKey::new(tool, profile, id);
            let Some(path) = crate::conversations::locate_transcript(&home, &key)? else {
                return Ok(None);
            };
            let origin = crate::quota::origin(paths, tool, profile, id)?;
            let status = inspect_with_origin(tool, profile, id, &path, origin.as_ref())?;
            crate::quota::learn(paths, std::slice::from_ref(&status))?;
            let Some(limit) = status.limit.filter(|limit| window.contains(&limit.at)) else {
                return Ok(None);
            };
            let can_rerun = check_rerun
                && origin.is_none()
                && rejected_first_turn(tool, &path, &limit).unwrap_or(false);
            return Ok(Some(LaunchLimit {
                id: Some(status.id),
                limit,
                can_rerun,
            }));
        }
    };
    // A TUI owns the terminal and has no process-scoped startup ID. All
    // appended rollouts can teach their own home a hold. Only a unique new
    // top-level rollout in the same cwd can supply the interactive hint.
    let histories = crate::native_usage::codex_histories(&home, |_| {});
    let origins = crate::quota::read_origins(paths)?;
    let mut statuses = Vec::new();
    let mut matching = Vec::new();
    for (modified, path) in histories {
        if DateTime::<Utc>::from(modified) < *window.start() {
            break;
        }
        let Some(header) = crate::codex_history::read_header(&path).ok().flatten() else {
            continue;
        };
        let p = &header["payload"];
        let Some(id) = p["id"].as_str() else { continue };
        let origin = origins.for_session(&home, id);
        if let Ok(status) = inspect_with_origin(tool, profile, id, &path, origin) {
            if origin.is_none()
                && p["source"].get("subagent").is_none()
                && cwd.is_some_and(|cwd| p["cwd"].as_str().map(Path::new) == Some(cwd))
                && path
                    .metadata()
                    .and_then(|m| m.created())
                    .is_ok_and(|at| window.contains(&DateTime::<Utc>::from(at)))
            {
                matching.push(statuses.len());
            }
            statuses.push(status);
        }
    }
    crate::quota::learn(paths, &statuses)?;
    let current_limit = |status: &SessionStatus| {
        status
            .limit
            .clone()
            .filter(|limit| window.contains(&limit.at))
    };
    if let [index] = matching.as_slice() {
        let matched = &statuses[*index];
        if let Some(limit) = current_limit(matched) {
            return Ok(Some(LaunchLimit {
                id: Some(matched.id.clone()),
                limit,
                can_rerun: false,
            }));
        }
    }
    Ok(statuses
        .iter()
        .filter_map(current_limit)
        .max_by_key(|limit| limit.at)
        .map(|limit| LaunchLimit {
            id: None,
            limit,
            can_rerun: false,
        }))
}

/// Home inspection credits only locally owned records. Settings-only
/// inspection may inherit arguments without treating the history as evidence.
enum Ownership<'a> {
    SettingsOnly,
    Isolated(Option<&'a crate::quota::Origin>),
}

fn inspect_tail(
    tool: &str,
    profile: &str,
    id: &str,
    path: &Path,
    ownership: Ownership<'_>,
) -> Result<SessionStatus> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let origin = match ownership {
        Ownership::Isolated(origin) => origin,
        Ownership::SettingsOnly => None,
    };
    let inherited = origin.filter(|o| o.path == path).map(|o| o.bytes);
    let now = Utc::now();
    let identified = matches!(ownership, Ownership::Isolated(_))
        && (tool != "codex"
            || crate::codex_history::read_header(path)?
                .is_some_and(|header| header["payload"]["id"] == id));
    let cutoff = origin
        .map(|origin| origin.at)
        .or_else(|| metadata.created().ok().map(DateTime::<Utc>::from));
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
    let recovery_floor = if identified {
        end.saturating_sub(RECOVERY_TAIL_BYTES)
            .max(inherited.unwrap_or_default())
    } else {
        end
    };
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

/// Prove that a bound native session rejected its sole turn before generating
/// output or calling a tool. The caller owns launch/session correlation and quota
/// ownership; this separate full-file check must never infer safety from a tail.
pub(crate) fn rejected_first_turn(tool: &str, path: &Path, limit: &Limit) -> Result<bool> {
    // A large, incomplete, changing or unfamiliar transcript cannot authorize a
    // second execution. Keep the normal status reader's bounded tail independent
    // so declining an automatic rerun does not prevent learning the limit.
    const MAX_PROOF_BYTES: u64 = 8 * 1024 * 1024;
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() == 0 || before.len() > MAX_PROOF_BYTES {
        return Ok(false);
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut file)
        .take(MAX_PROOF_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 != before.len()
        || after.len() != before.len()
        || after.modified().ok() != before.modified().ok()
        || !bytes.ends_with(b"\n")
    {
        return Ok(false);
    }
    let Some(id) = path.file_stem().and_then(|id| id.to_str()) else {
        return Ok(false);
    };
    Ok(match tool {
        "claude" => claude_rejected_first_turn(&bytes, id, limit),
        "codex" => crate::codex_history::file_ids(path).is_some_and(|(thread, rollout)| {
            // A segment may omit an earlier successful turn. Native resumes and
            // forks also carry ancestry; none proves the session's only turn.
            thread == rollout && codex_rejected_first_turn(&bytes, thread, limit)
        }),
        _ => false,
    })
}

fn retry_records(bytes: &[u8], mut inspect: impl FnMut(&Value) -> bool) -> bool {
    for line in bytes[..bytes.len() - 1].split(|byte| *byte == b'\n') {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            return false;
        };
        if !record.is_object() || !inspect(&record) {
            return false;
        }
    }
    true
}

fn retry_limit_matches(tool: &str, record: &Value, limit: &Limit) -> bool {
    classify_limit(tool, record, limit.at).is_some_and(|found| {
        found.at == limit.at && found.kind == limit.kind && found.native_kind == limit.native_kind
    })
}

fn claude_rejected_first_turn(bytes: &[u8], id: &str, limit: &Limit) -> bool {
    let mut users = 0;
    let mut rejected = false;
    let complete = retry_records(bytes, |record| {
        if ["sessionId", "session_id"]
            .into_iter()
            .any(|field| !record[field].is_null() && record[field] != id)
        {
            return false;
        }
        match record["type"].as_str() {
            Some("user") => {
                if rejected
                    || record["sessionId"] != id
                    || record["message"]["role"] != "user"
                    || !record["toolUseResult"].is_null()
                    || (!record["isMeta"].is_null() && !record["isMeta"].is_boolean())
                    || !retry_input_content(&record["message"]["content"], false)
                {
                    return false;
                }
                if record["isMeta"] != true {
                    users += 1;
                }
                users <= 1
            }
            Some("assistant") => {
                if rejected
                    || users != 1
                    || record["sessionId"] != id
                    || record["isApiErrorMessage"] != true
                    || record["message"]["role"] != "assistant"
                    || record["message"]["model"] != "<synthetic>"
                    || (!record["quotaLimits"]["status"].is_null()
                        && record["quotaLimits"]["status"] != "rejected")
                    || (!record["message"]["usage"]["output_tokens"].is_null()
                        && record["message"]["usage"]["output_tokens"].as_u64() != Some(0))
                    || !retry_error_content(&record["message"]["content"])
                    || !retry_limit_matches("claude", record, limit)
                {
                    return false;
                }
                rejected = true;
                true
            }
            Some("attachment") => {
                // Startup context is not another turn. Agent messages, tool
                // progress and new attachment formats need an explicit audit.
                if matches!(
                    record["attachment"]["type"].as_str(),
                    Some(
                        "hook_success"
                            | "hook_additional_context"
                            | "hook_non_blocking_error"
                            | "hook_cancelled"
                    )
                ) {
                    return matches!(
                        record["attachment"]["hookEvent"].as_str(),
                        Some("SessionStart" | "UserPromptSubmit" | "Setup" | "InstructionsLoaded")
                    );
                }
                matches!(
                    record["attachment"]["type"].as_str(),
                    Some(
                        "environment"
                            | "model"
                            | "deferred_tools_delta"
                            | "agent_listing_delta"
                            | "mcp_instructions_delta"
                            | "skill_listing"
                            | "auto_mode"
                            | "instructions"
                            | "session_context"
                            | "date"
                            | "credential_org"
                            | "remote_session_change"
                            | "prompt_snapshot"
                    )
                )
            }
            Some("api-request-blob") => {
                !rejected
                    && matches!(
                        record["message"]["role"].as_str(),
                        Some("user" | "system" | "developer")
                    )
            }
            Some("api-request" | "api-request-shape") => !rejected,
            Some(
                "session-owner"
                | "queue-operation"
                | "atis-latch"
                | "last-prompt"
                | "cost-state"
                | "custom-title"
                | "ai-title"
                | "file-history-snapshot",
            ) => true,
            _ => false,
        }
    });
    complete && users == 1 && rejected
}

fn codex_rejected_first_turn(bytes: &[u8], id: &str, limit: &Limit) -> bool {
    let mut header = false;
    let mut turn = None::<String>;
    let mut rejected = false;
    let mut user_input = false;
    let complete = retry_records(bytes, |record| {
        if rejected {
            return false;
        }
        let payload = &record["payload"];
        let starts_turn = record["type"] == "event_msg" && payload["type"] == "task_started";
        if !payload.is_object()
            || (!payload["thread_id"].is_null() && payload["thread_id"] != id)
            || (!(payload["turn_id"].is_null() || starts_turn)
                && turn
                    .as_deref()
                    .is_none_or(|turn| payload["turn_id"] != turn))
        {
            return false;
        }
        match record["type"].as_str() {
            Some("session_meta") => {
                if header
                    || turn.is_some()
                    || payload["id"] != id
                    || !payload["history_base"].is_null()
                    || !payload["forked_from_id"].is_null()
                    || (!record["ordinal"].is_null() && record["ordinal"].as_u64() != Some(0))
                    || (!payload["history_mode"].is_null()
                        && !matches!(
                            payload["history_mode"].as_str(),
                            Some("legacy" | "paginated")
                        ))
                    || !matches!(payload["source"].as_str(), Some("cli" | "exec"))
                {
                    return false;
                }
                header = true;
                true
            }
            Some("event_msg") if header => match payload["type"].as_str() {
                Some("task_started") => {
                    if turn.is_some() {
                        return false;
                    }
                    turn = payload["turn_id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned);
                    turn.is_some()
                }
                Some("task_complete") => {
                    rejected = turn
                        .as_deref()
                        .is_some_and(|turn| payload["turn_id"] == turn)
                        && payload["last_agent_message"].is_null()
                        && retry_limit_matches("codex", record, limit);
                    rejected
                }
                Some("item_started" | "item_completed") => {
                    let item = &payload["item"];
                    // Native events duplicate response_item user inputs and use
                    // PascalCase item kinds. Reasoning and tool items also arrive
                    // here, so checking response_item alone would miss output.
                    let input = turn
                        .as_deref()
                        .is_some_and(|turn| payload["turn_id"] == turn)
                        && item["type"] == "UserMessage"
                        && retry_input_content(&item["content"], false);
                    user_input |= input;
                    input
                }
                Some("user_message") => {
                    let input = turn.is_some() && payload["message"].is_string();
                    user_input |= input;
                    input
                }
                Some("token_count") => {
                    turn.is_some()
                        && (payload["info"].is_null()
                            || ["last_token_usage", "total_token_usage"]
                                .into_iter()
                                .all(|field| {
                                    let usage = &payload["info"][field];
                                    usage["output_tokens"].as_u64() == Some(0)
                                        && (usage["reasoning_output_tokens"].is_null()
                                            || usage["reasoning_output_tokens"].as_u64() == Some(0))
                                }))
                }
                Some("thread_settings_applied") => turn.is_some(),
                _ => false,
            },
            Some("turn_context") => {
                header
                    && turn
                        .as_deref()
                        .is_some_and(|turn| payload["turn_id"] == turn)
            }
            Some("world_state") => header && turn.is_some(),
            Some("response_item") if header && turn.is_some() => match payload["type"].as_str() {
                Some("message") => {
                    let role = payload["role"].as_str();
                    let input = matches!(role, Some("user" | "developer" | "system"))
                        && retry_input_content(&payload["content"], true);
                    user_input |= input && role == Some("user");
                    input
                }
                Some("additional_tools") => payload["role"] == "developer",
                _ => false,
            },
            // A token_usage_record, reasoning item, tool result or unknown native
            // envelope is not evidence that another execution is safe.
            _ => false,
        }
    });
    complete && header && turn.is_some() && user_input && rejected
}

fn retry_error_content(content: &Value) -> bool {
    content.is_string()
        || content.as_array().is_some_and(|blocks| {
            !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|block| block["type"] == "text" && block["text"].is_string())
        })
}

fn retry_input_content(content: &Value, codex: bool) -> bool {
    if !codex && content.is_string() {
        return true;
    }
    content.as_array().is_some_and(|blocks| {
        !blocks.is_empty()
            && blocks.iter().all(|block| match block["type"].as_str() {
                Some("input_text") if codex => block["text"].is_string(),
                Some("text") if !codex => block["text"].is_string(),
                Some("input_image") if codex => block["image_url"].is_string(),
                Some("image" | "document") if !codex => block["source"].is_object(),
                _ => false,
            })
    })
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

    fn rejected_turn_fixture(tool: &str) -> Vec<Value> {
        if tool == "claude" {
            vec![
                json!({"type":"session-owner","sessionId":"retry"}),
                json!({"type":"attachment","sessionId":"retry","attachment":{"type":"hook_success","hookEvent":"SessionStart"}}),
                json!({"type":"user","sessionId":"retry","message":{"role":"user","content":"hello"}}),
                json!({"type":"assistant","sessionId":"retry","isApiErrorMessage":true,"apiError":"usage_limit_reached","quotaLimits":{"rateLimitType":"seven_day"},"message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"usage limit"}]}}),
            ]
        } else {
            vec![
                json!({"type":"session_meta","payload":{"id":"retry","source":"exec","history_mode":"paginated"}}),
                json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"environment context"}]}}),
                json!({"type":"world_state","payload":{}}),
                json!({"type":"turn_context","payload":{"turn_id":"turn","model":"gpt-test"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":"turn","item":{"type":"UserMessage","content":[{"type":"text","text":"hello"}]}}}),
                json!({"type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{}}}),
                json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn","error":{"codex_error_info":"usage_limit_exceeded","message":"usage limit"}}}),
            ]
        }
    }

    fn prove_fixture(tool: &str, mut rows: Vec<Value>) -> bool {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(if tool == "claude" {
            "retry.jsonl"
        } else {
            "rollout-retry.jsonl"
        });
        let now = Utc::now();
        for row in &mut rows {
            row["timestamp"] = json!(now);
        }
        let limit = rows
            .iter()
            .find_map(|row| classify_limit(tool, row, now))
            .unwrap();
        let bytes = rows
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>();
        std::fs::write(&path, bytes).unwrap();
        rejected_first_turn(tool, &path, &limit).unwrap()
    }

    #[test]
    fn sole_rejection_proof_accepts_native_startup_context_in_both_tools() {
        let mut claude = rejected_turn_fixture("claude");
        claude.insert(3, json!({"type":"api-request-blob","message":{"role":"system","content":[{"type":"tool_addition"}]}}));
        claude.push(json!({"type":"cost-state","sessionId":"retry"}));
        assert!(prove_fixture("claude", claude));
        // Codex persists its environment and actual prompt as separate user
        // records within one native task; those are not two executed turns.
        assert!(prove_fixture("codex", rejected_turn_fixture("codex")));
    }

    #[test]
    fn sole_rejection_proof_declines_claude_output_tools_and_ambiguous_records() {
        for unsafe_row in [
            json!({"type":"assistant","sessionId":"retry","message":{"role":"assistant","model":"claude-test","content":[{"type":"text","text":"partial"}]}}),
            json!({"type":"assistant","sessionId":"retry","message":{"role":"assistant","model":"claude-test","content":[{"type":"tool_use","name":"Bash"}]}}),
            json!({"type":"user","sessionId":"retry","message":{"role":"user","content":[{"type":"tool_result","content":"done"}]}}),
            json!({"type":"user","sessionId":"retry","message":{"role":"user","content":"second turn"}}),
            json!({"type":"user","sessionId":"other","message":{"role":"user","content":"other session"}}),
            json!({"type":"attachment","attachment":{"type":"hook_success","hookEvent":"PreToolUse"}}),
            json!({"type":"future-output-envelope"}),
        ] {
            let mut rows = rejected_turn_fixture("claude");
            rows.insert(rows.len() - 1, unsafe_row.clone());
            assert!(!prove_fixture("claude", rows), "accepted {unsafe_row}");
        }
        let mut rows = rejected_turn_fixture("claude");
        rows.last_mut().unwrap()["message"]["content"] = json!([{"type":"tool_use","name":"Bash"}]);
        assert!(!prove_fixture("claude", rows));
    }

    #[test]
    fn sole_rejection_proof_declines_codex_output_tools_ancestry_and_other_turns() {
        for unsafe_row in [
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"partial"}]}}),
            json!({"type":"response_item","payload":{"type":"reasoning","summary":[]}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","output":"done"}}),
            json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":"turn","item":{"type":"AgentMessage","content":[{"type":"Text","text":"partial"}]}}}),
            json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":"turn","item":{"type":"Reasoning"}}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"second"}}),
            json!({"type":"turn_context","payload":{"turn_id":"other"}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"output_tokens":1},"total_token_usage":{"output_tokens":1}}}}),
            json!({"type":"token_usage_record","payload":{"usage":{"output_tokens":1}}}),
            json!({"type":"future-output-envelope"}),
        ] {
            let mut rows = rejected_turn_fixture("codex");
            rows.insert(rows.len() - 1, unsafe_row.clone());
            assert!(!prove_fixture("codex", rows), "accepted {unsafe_row}");
        }
        for field in ["history_base", "forked_from_id"] {
            let mut rows = rejected_turn_fixture("codex");
            rows[0]["payload"][field] = json!("prior");
            assert!(!prove_fixture("codex", rows));
        }
        let mut rows = rejected_turn_fixture("codex");
        rows.last_mut().unwrap()["payload"]["turn_id"] = json!("other");
        assert!(!prove_fixture("codex", rows));
    }

    #[test]
    fn sole_rejection_proof_requires_complete_bounded_jsonl() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retry.jsonl");
        let now = Utc::now();
        let limit = classify_limit(
            "claude",
            &json!({"timestamp":now,"apiError":"usage_limit_reached"}),
            now,
        )
        .unwrap();
        for bytes in [b"".as_slice(), b"{}", b"{\n", b"{}\n\n", b"[]\n"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(!rejected_first_turn("claude", &path, &limit).unwrap());
        }
        let file = File::create(&path).unwrap();
        file.set_len(8 * 1024 * 1024 + 1).unwrap();
        assert!(!rejected_first_turn("claude", &path, &limit).unwrap());
    }

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
