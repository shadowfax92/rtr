//! Filesystem view of Codex threads and their immutable rollout ancestry.
//!
//! A thread ID selects its newest segment (or its original file); a history_base ID selects one exact
//! rollout (the suffix after `_`, or the original thread ID). Mixing these two
//! lookups replays abandoned branches or invents cycles after a native revert.
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// One physical prefix in replay order. Limits refer to this file, not to the
/// flattened history. Consumers preserve these boundaries even as writers append.
pub(crate) struct Fragment {
    pub path: PathBuf,
    pub header: Value,
    pub limit: Option<(u64, u64)>,
}

pub(crate) fn file_ids(path: &Path) -> Option<(&str, &str)> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    let rest = stem.strip_prefix("rollout-").unwrap_or(stem);
    // Keep historical/test opaque IDs supported; native filenames have a
    // 19-character local timestamp followed by a dash.
    let ids = if rest.as_bytes().get(19) == Some(&b'-') && rest.as_bytes().get(10) == Some(&b'T') {
        &rest[20..]
    } else {
        rest
    };
    if let Some((thread, rollout)) = ids.split_once('_') {
        // Native segment aliases contain two complete UUIDs. Accepting a
        // truncated owner could bind another conversation's history to it.
        if thread.len() != 36
            || rollout.len() != 36
            || uuid::Uuid::parse_str(thread).is_err()
            || uuid::Uuid::parse_str(rollout).is_err()
        {
            return None;
        }
        Some((thread, rollout))
    } else {
        Some((ids, ids))
    }
}

/// Active storage takes precedence over archive duplicates. Within each store,
/// segments supersede their retired original, then newer filenames win. The
/// segment preference repairs older RTR copies that used UTC filenames while
/// Codex used local time; otherwise the retired original can sort last.
pub(crate) fn live_files(files: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    let mut live = BTreeMap::new();
    for path in files {
        let Some((thread, _)) = file_ids(path) else {
            continue;
        };
        live.entry(thread.to_string())
            .and_modify(|previous: &mut PathBuf| {
                let archived =
                    |p: &Path| p.components().any(|c| c.as_os_str() == "archived_sessions");
                if (archived(previous) && !archived(path))
                    || (archived(previous) == archived(path)
                        && (
                            file_ids(path).is_some_and(|(thread, rollout)| thread != rollout),
                            path.file_name(),
                        ) > (
                            file_ids(previous).is_some_and(|(thread, rollout)| thread != rollout),
                            previous.file_name(),
                        ))
                {
                    *previous = path.clone();
                }
            })
            .or_insert_with(|| path.clone());
    }
    live
}

pub(crate) fn read_header(path: &Path) -> Result<Option<Value>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    for line in BufReader::new(file).take(256 * 1024).lines().take(16) {
        let Ok(line) = line else { continue };
        let Ok(record): Result<Value, _> = serde_json::from_str(&line) else {
            continue;
        };
        if record["type"] == "session_meta" {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

pub(crate) fn home_for(path: &Path) -> Result<&Path> {
    path.ancestors()
        .find_map(|p| {
            matches!(
                p.file_name().and_then(|s| s.to_str()),
                Some("sessions" | "archived_sessions")
            )
            .then(|| p.parent())
            .flatten()
        })
        .context("Codex rollout is outside sessions or archived_sessions")
}

pub(crate) fn lineage(home: &Path, path: &Path) -> Result<Vec<Fragment>> {
    let mut path = path.to_path_buf();
    let mut limit = None;
    let mut seen = HashSet::new();
    let mut fragments = Vec::new();
    // Only discover siblings/ancestors if a header actually references one.
    let mut files = None;
    loop {
        let identity = std::fs::metadata(&path)?;
        if !seen.insert((identity.dev(), identity.ino())) {
            bail!("cyclic Codex history ancestry at {}", path.display());
        }
        if seen.len() > 128 {
            bail!("Codex history ancestry exceeds 128 levels");
        }
        let header = read_header(&path)?.context("Codex transcript has no session metadata")?;
        let metadata = &header["payload"];
        if file_ids(&path).is_none_or(|(thread, _)| metadata["id"] != thread) {
            bail!(
                "Codex history filename and session identity disagree: {}",
                path.display()
            );
        }
        let mode = metadata["history_mode"].as_str().unwrap_or("legacy");
        if !matches!(mode, "legacy" | "paginated") {
            bail!("unsupported Codex history mode '{mode}'");
        }
        let base = &metadata["history_base"];
        let parent = if base.is_null() {
            None
        } else {
            if mode != "paginated" {
                bail!("unsupported inherited legacy Codex history");
            }
            let id = base["thread_id"]
                .as_str()
                .context("Codex history base has no rollout ID")?;
            let bytes = base["end_byte_offset"]
                .as_u64()
                .context("Codex history base has no byte boundary")?;
            let end = base["end_ordinal_exclusive"]
                .as_u64()
                .context("Codex history base has no ordinal boundary")?;
            if header["ordinal"].as_u64() != Some(end) {
                bail!("Codex inherited history boundary disagrees with its header");
            }
            if files.is_none() {
                files = Some(crate::conversations::codex_transcript_files(home)?);
            }
            let files = files.as_ref().expect("initialized above");
            let parent = files
                .iter()
                .find(|p| file_ids(p).is_some_and(|(_, rollout)| rollout == id))
                .with_context(|| format!("missing Codex history parent {id}"))?;
            Some((parent.clone(), (bytes, end)))
        };
        fragments.push(Fragment {
            path,
            header,
            limit,
        });
        let Some((parent, boundary)) = parent else {
            break;
        };
        path = parent;
        limit = Some(boundary);
    }
    fragments.reverse();
    Ok(fragments)
}
