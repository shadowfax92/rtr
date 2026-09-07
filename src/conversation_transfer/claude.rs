//! Relocates a Claude transcript and its session-owned companion files. Message
//! UUID chains remain intact; only root session ownership changes. Account bridge
//! and queued runtime records are not conversation history to execute elsewhere.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::json;

use super::{assets::Assets, write_record, Snapshot};
use crate::conversations::Conversation;

pub(super) fn copy(
    source: &Conversation,
    home: &Path,
    id: &str,
    writer: &mut impl Write,
    assets: &mut Assets,
) -> Result<PathBuf> {
    let project = source
        .transcript_path
        .parent()
        .context("Claude transcript has no project directory")?
        .strip_prefix(home)
        .context("Claude project is outside its profile")?;
    let relative = project.join(format!("{id}.jsonl"));
    assets.session_directory(
        &source.transcript_path.with_extension(""),
        &project.join(id),
    )?;
    assets.session_directory(
        &home.join("image-cache").join(&source.id),
        &PathBuf::from("image-cache").join(id),
    )?;
    assets.session_directory(
        &home.join("uploads").join(&source.id),
        &PathBuf::from("uploads").join(id),
    )?;
    assets.claude_project(project);
    let mut messages = 0;
    let mut parent_uuids = HashSet::new();
    Snapshot::open(home, &source.transcript_path, None)?.visit(|mut record| {
        if is_runtime_record(&record) {
            return Ok(());
        }
        if record["type"] == "fork-context-ref" {
            bail!(
                "Claude main transcript uses external parent context; cannot copy it independently"
            );
        }
        if let Some(uuid) = record["uuid"].as_str() {
            parent_uuids.insert(uuid.to_string());
        }
        if matches!(record["type"].as_str(), Some("user" | "assistant")) {
            messages += 1;
        }
        for field in ["sessionId", "session_id"] {
            // Native forks can retain ancestor session IDs on historical rows.
            // Rebind the main transcript's structural ownership, not UUID links
            // or literal IDs mentioned in message/tool-result content.
            if record[field].is_string() {
                record[field] = json!(id);
            }
        }
        assets.rewrite(&mut record)?;
        write_record(writer, &record)
    })?;
    assets.rewrite_companion_transcripts(&source.id, id, &parent_uuids)?;
    if messages == 0 {
        bail!("Claude source has no conversation messages");
    }
    Ok(relative)
}

/// These records restore account-bound jobs, permissions, filesystem ownership,
/// or pending actions. Neither the root copy nor historical subagent assets may
/// re-arm them in a different account merely by rebinding a session ID.
pub(super) fn is_runtime_record(record: &serde_json::Value) -> bool {
    matches!(
        record["type"].as_str(),
        Some(
            "bridge-session"
                | "queue-operation"
                | "atis-latch"
                | "permission-mode"
                | "mode"
                | "file-history-snapshot"
                | "file-history-delta"
                | "artifact-comment-monitor"
                | "artifact-autoreact-ledger"
                | "worktree-state"
                | "isolation-latch"
                | "continued-in"
                | "observer-ref"
        )
    )
}
