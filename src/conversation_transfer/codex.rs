//! Makes a Codex rollout independent by copying its inherited byte prefixes in
//! order. Only envelope identity/ordinals change; native replay owns all message,
//! tool-result, world-state, and compaction semantics.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::json;

use super::{assets::Assets, write_record, Snapshot};
use crate::{codex_history, conversations::Conversation};

pub(super) fn copy(
    source: &Conversation,
    home: &Path,
    id: &str,
    writer: &mut impl Write,
    assets: &mut Assets,
) -> Result<PathBuf> {
    let fragments = codex_history::lineage(home, &source.transcript_path)?;
    let mut header = fragments
        .last()
        .context("empty Codex history")?
        .header
        .clone();
    if header["payload"]["id"] != source.id {
        bail!("Codex source identity changed before copying");
    }
    let paginated = header["payload"]["history_mode"] == "paginated";
    let metadata = header["payload"]
        .as_object_mut()
        .context("invalid Codex metadata")?;
    metadata.insert("id".into(), json!(id));
    metadata.insert("session_id".into(), json!(id));
    metadata.insert("forked_from_id".into(), json!(source.id));
    metadata.insert("source".into(), json!("cli"));
    // Like a native CLI fork, this root owns its complete visible history.
    // Retaining a subagent projection bound after renumbering can hide rows.
    metadata.remove("subagent_history_start_ordinal");
    metadata.remove("history_base");
    metadata.remove("forked_from_ordinal_exclusive");
    if paginated {
        header["ordinal"] = json!(0);
    }
    assets.rewrite(&mut header)?;
    write_record(writer, &header)?;
    let mut ordinal = 1u64;
    let mut active_turn = None;
    for fragment in fragments {
        let mut previous = None;
        let snapshot =
            Snapshot::open(home, &fragment.path, fragment.limit.map(|(bytes, _)| bytes))?;
        let mut first = true;
        snapshot.visit(|mut record| {
            // Revalidate the header on the descriptor being copied: discovery
            // must not authorize a different file substituted before snapshotting.
            if first {
                if record != fragment.header {
                    bail!("Codex source metadata changed before copying");
                }
                first = false;
            }
            if paginated {
                let current = record["ordinal"]
                    .as_u64()
                    .context("paginated Codex record has no ordinal")?;
                if previous.is_some_and(|last| current != last + 1) {
                    bail!("non-contiguous Codex history ordinals");
                }
                previous = Some(current);
            }
            if record["type"] == "session_meta" {
                return Ok(());
            }
            if record["type"] == "event_msg" {
                match record["payload"]["type"].as_str() {
                    Some("task_started") => {
                        active_turn = record["payload"]["turn_id"].as_str().map(str::to_string)
                    }
                    Some("task_complete" | "turn_aborted") => active_turn = None,
                    _ => {}
                }
            }
            if paginated {
                record["ordinal"] = json!(ordinal);
                ordinal += 1;
            }
            assets.rewrite(&mut record)?;
            write_record(writer, &record)
        })?;
        if let Some((_, end)) = fragment.limit {
            if previous.and_then(|last| last.checked_add(1)) != Some(end) {
                bail!("Codex inherited byte and ordinal boundaries disagree");
            }
        }
    }
    // The source keeps running. Mark its unfinished turn interrupted in this
    // copy so native resume does not mistake a partial snapshot for live work.
    if let Some(turn) = active_turn {
        let mut record = json!({"timestamp":chrono::Utc::now().to_rfc3339(),"type":"event_msg","payload":{"type":"turn_aborted","turn_id":turn,"reason":"interrupted"}});
        if paginated {
            record["ordinal"] = json!(ordinal);
        }
        write_record(writer, &record)?;
    }
    let now = chrono::Local::now();
    Ok(PathBuf::from(format!(
        "sessions/{}/rollout-{}-{id}.jsonl",
        now.format("%Y/%m/%d"),
        now.format("%Y-%m-%dT%H-%M-%S")
    )))
}
