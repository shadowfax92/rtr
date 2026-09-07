//! Makes a Codex rollout independent by copying its inherited byte prefixes in
//! order. Only envelope identity/ordinals change; native replay owns all message,
//! tool-result, world-state, and compaction semantics.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::{assets::Assets, write_record, Snapshot};
use crate::conversations::{locate_transcript, Conversation, ConversationKey};

struct Fragment {
    snapshot: Snapshot,
    metadata: Value,
    expected_end: Option<u64>,
}

fn collect(
    home: &Path,
    path: &Path,
    expected_id: &str,
    limit: Option<(u64, u64)>,
    seen: &mut HashSet<String>,
    out: &mut Vec<Fragment>,
) -> Result<()> {
    if seen.len() >= 128 {
        bail!("Codex history ancestry exceeds 128 levels");
    }
    let snapshot = Snapshot::open(home, path, limit.map(|(bytes, _)| bytes))?;
    let mut first = String::new();
    // Header reads use the same open descriptor as the fixed snapshot.
    BufReader::new(&snapshot.file).read_line(&mut first)?;
    use std::io::{Seek, SeekFrom};
    (&snapshot.file).seek(SeekFrom::Start(0))?;
    let header: Value = serde_json::from_str(&first).context("reading Codex session metadata")?;
    if header["type"] != "session_meta" {
        bail!(
            "Codex transcript has no initial session metadata: {}",
            path.display()
        );
    }
    let metadata = header["payload"].clone();
    let id = metadata["id"]
        .as_str()
        .context("Codex session metadata has no ID")?;
    if id != expected_id {
        bail!(
            "Codex history filename and session identity disagree: {}",
            path.display()
        );
    }
    if !seen.insert(id.to_string()) {
        bail!("cyclic Codex history ancestry at {id}");
    }
    let mode = metadata["history_mode"].as_str().unwrap_or("legacy");
    if !matches!(mode, "legacy" | "paginated") {
        bail!("unsupported Codex history mode '{mode}'");
    }
    if !metadata["history_base"].is_null() {
        if mode != "paginated" {
            bail!("unsupported inherited legacy Codex history");
        }
        let base = &metadata["history_base"];
        let parent = base["thread_id"]
            .as_str()
            .context("Codex history base has no parent ID")?;
        let bytes = base["end_byte_offset"]
            .as_u64()
            .context("Codex history base has no byte boundary")?;
        let end = base["end_ordinal_exclusive"]
            .as_u64()
            .context("Codex history base has no ordinal boundary")?;
        if header["ordinal"].as_u64() != Some(end) {
            bail!("Codex inherited history boundary disagrees with its header");
        }
        let key = ConversationKey::new("codex", "", parent);
        let path = locate_transcript(home, &key)?
            .with_context(|| format!("missing Codex history parent {parent}"))?;
        collect(home, &path, parent, Some((bytes, end)), seen, out)?;
    }
    out.push(Fragment {
        snapshot,
        metadata: header,
        expected_end: limit.map(|(_, end)| end),
    });
    Ok(())
}

pub(super) fn copy(
    source: &Conversation,
    home: &Path,
    id: &str,
    writer: &mut impl Write,
    assets: &mut Assets,
) -> Result<PathBuf> {
    let mut fragments = Vec::new();
    collect(
        home,
        &source.transcript_path,
        &source.id,
        None,
        &mut HashSet::new(),
        &mut fragments,
    )?;
    let mut header = fragments
        .last()
        .context("empty Codex history")?
        .metadata
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
        fragment.snapshot.visit(|mut record| {
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
        if let Some(end) = fragment.expected_end {
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
    let now = chrono::Utc::now();
    Ok(PathBuf::from(format!(
        "sessions/{}/rollout-{}-{id}.jsonl",
        now.format("%Y/%m/%d"),
        now.format("%Y-%m-%dT%H-%M-%S")
    )))
}
