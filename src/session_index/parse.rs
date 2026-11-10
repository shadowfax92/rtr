//! Streaming transcript projection. Offsets always stop after a complete JSONL
//! record, including discarded tool records. Saved metadata and message endpoints
//! allow append continuation and exact Codex history-prefix composition.
use super::{
    store::{Stamp, Store, VERSION},
    Source,
};
use crate::conversations::{self, ClaudeMetadata, Conversation};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::PathBuf,
};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Message {
    pub end: u64,
    pub role: String,
    pub text: String,
    pub timestamp: Option<DateTime<Utc>>,
}
/// The native metadata fold continues alongside the byte cursor on append.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct NativeMetadata {
    claude: ClaudeMetadata,
    id: Option<String>,
    cwd: Option<PathBuf>,
    started: Option<DateTime<Utc>>,
    updated: Option<DateTime<Utc>>,
    hidden: bool,
    inherited: bool,
    header_seen: bool,
}
/// One atomic checkpoint of a physical transcript, never a flattened Codex
/// thread. Message endpoints retain inherited byte cuts without tool payloads.
#[derive(Serialize, Deserialize)]
pub(crate) struct Record {
    pub(super) version: u32,
    pub(super) path: PathBuf,
    pub(super) stamp: Stamp,
    pub(super) offset: u64,
    pub(super) complete: bool,
    pub(super) metadata: NativeMetadata,
    pub(super) messages: Vec<Message>,
    // Verify both ends of the consumed prefix before assuming a growth is an
    // append. Inode changes, truncation, and prefix/tail rewrites rebuild it.
    prefix: Fingerprint,
    anchor: Fingerprint,
    terminated: bool,
    #[serde(skip)]
    pub(super) parsed_bytes: u64,
    #[serde(skip)]
    pub(super) cache_error: Option<String>,
}

#[derive(Default, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    length: usize,
    hash: u64,
}
impl Record {
    pub(super) fn summary(&self, source: &Source) -> Result<super::store::Summary> {
        Ok(super::store::Summary {
            stamp: self.stamp.clone(),
            conversation: self.metadata.conversation(source)?,
            hidden: self.metadata.hidden,
            complete: self.complete,
        })
    }
}
impl NativeMetadata {
    fn observe(&mut self, record: &Value, tool: &str) {
        if tool == "claude" {
            self.claude.observe(record);
            return;
        }
        if let Some(time) = record
            .get("timestamp")
            .and_then(conversations::parse_timestamp)
        {
            self.updated = self.updated.max(Some(time));
        }
        if record["type"] == "session_meta" && !self.header_seen {
            let payload = &record["payload"];
            // The first physical header owns this file. Forked histories can
            // replay ancestor session_meta records later in the same stream;
            // those describe inherited content, not a change of native identity,
            // cwd, visibility, or ancestry. Match read_header's first-record rule.
            self.header_seen = true;
            self.inherited = !payload["history_base"].is_null();
            self.id = payload["id"].as_str().map(str::to_string);
            self.cwd = payload["cwd"].as_str().map(PathBuf::from);
            self.started = payload
                .get("timestamp")
                .and_then(conversations::parse_timestamp);
            self.hidden =
                payload["source"] == "exec" || payload["source"].get("subagent").is_some();
        }
    }
    fn conversation(&self, source: &Source) -> Result<Option<Conversation>> {
        if source.tool == "claude" {
            return self.claude.clone().conversation(
                &source.path,
                &source.profile,
                source.enabled,
                source.bypass,
            );
        }
        let (Some(id), Some(cwd)) = (&self.id, &self.cwd) else {
            return Ok(None);
        };
        let first_prompt = source.prompts.get(id).cloned();
        let native_name = source.names.get(id).cloned();
        Ok(Some(Conversation {
            tool: source.tool.clone(),
            profile: source.profile.clone(),
            id: id.clone(),
            title: native_name
                .as_deref()
                .map(|s| conversations::display_text(s, 160))
                .filter(|s| !s.is_empty())
                .or_else(|| first_prompt.clone())
                .unwrap_or_else(|| id.clone()),
            native_name,
            first_prompt,
            cwd: cwd.clone(),
            started_at: self.started,
            updated_at: if self.inherited {
                conversations::codex_updated_at(&source.path)?
            } else {
                self.updated.or(self.started).unwrap_or(
                    DateTime::from_timestamp(source.stamp.seconds, source.stamp.nanos as u32)
                        .unwrap_or_default(),
                )
            },
            enabled: source.enabled,
            bypass: source.bypass,
            transcript_path: source.path.clone(),
        }))
    }
}

// Deserialize only small top-level metadata. Serde skips tool/request payloads
// without constructing a Value tree; dialogue candidates get the full adapter.
#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<Value>,
    cwd: Option<Value>,
    #[serde(rename = "sessionId")]
    native_session_id: Option<Value>,
    session_id: Option<Value>,
    #[serde(rename = "agentName")]
    agent_name: Option<Value>,
    #[serde(rename = "customTitle")]
    custom_title: Option<Value>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<Value>,
}

fn contains(line: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(line, needle).is_some()
}
/// A conservative raw-line gate: false positives only cost parsing; escaped
/// JSON spellings take the adapter path so valid unusual encodings cannot hide.
pub(super) fn dialogue_candidate(line: &[u8], tool: &str) -> bool {
    if contains(line, b"\\u00") {
        return true;
    }
    if tool == "claude" {
        contains(line, b"\"user\"")
            || contains(line, b"\"assistant\"")
            || contains(line, b"\"queued_command\"")
    } else {
        contains(line, b"\"message\"")
            || contains(line, b"\"user_message\"")
            || contains(line, b"\"session_meta\"")
    }
}

pub(super) fn load(
    source: &Source,
    store: Option<&Store>,
    cancelled: &(impl Fn() -> bool + Sync),
) -> Result<Record> {
    load_mode(source, store, cancelled, true)
}
pub(super) fn load_mode(
    source: &Source,
    store: Option<&Store>,
    cancelled: &(impl Fn() -> bool + Sync),
    use_gate: bool,
) -> Result<Record> {
    let mut file =
        File::open(&source.path).with_context(|| format!("opening {}", source.path.display()))?;
    let stamp = Stamp::from_metadata(&file.metadata()?)?;
    let previous = store.and_then(|s| s.read(&source.path));
    let mut record = match previous {
        Some(r) if r.stamp == stamp && r.complete => return Ok(r),
        Some(r)
            if r.stamp.same_file(&stamp)
                && (r.stamp.size < stamp.size || (r.stamp == stamp && !r.complete))
                && (r.offset == 0 || r.terminated)
                && anchors_match(&mut file, &r)? =>
        {
            r
        }
        _ => Record {
            version: VERSION,
            path: source.path.clone(),
            stamp: stamp.clone(),
            offset: 0,
            complete: false,
            metadata: NativeMetadata::default(),
            messages: Vec::new(),
            prefix: Fingerprint::default(),
            anchor: Fingerprint::default(),
            terminated: true,
            parsed_bytes: 0,
            cache_error: None,
        },
    };
    record.stamp = stamp;
    record.parsed_bytes = 0;
    record.complete = false;
    file.seek(SeekFrom::Start(record.offset))?;
    let mut reader =
        BufReader::with_capacity(256 * 1024, file.take(record.stamp.size - record.offset));
    let mut line = Vec::new();
    let mut checkpoint = record.offset;
    loop {
        if cancelled() {
            break;
        }
        line.clear();
        let length = reader.read_until(b'\n', &mut line)?;
        if length == 0 {
            record.complete = true;
            break;
        }
        // Native writers may be halfway through a record. Revisit its bytes on
        // the next open instead of losing that dialogue forever.
        if line.last() != Some(&b'\n') && serde_json::from_slice::<Value>(&line).is_err() {
            break;
        }
        record.terminated = line.last() == Some(&b'\n');
        record.offset += length as u64;
        record.parsed_bytes += length as u64;
        if let Ok(envelope) = serde_json::from_slice::<Envelope>(&line) {
            // Codex ownership is observed only from the full physical header,
            // never from this payload-free projection used to collect timestamps.
            let kind = if source.tool == "claude" {
                envelope.kind
            } else {
                None
            };
            let mut metadata = serde_json::json!({"type":kind,"timestamp":envelope.timestamp,"cwd":envelope.cwd,
                "sessionId":envelope.native_session_id,"session_id":envelope.session_id,
                "agentName":envelope.agent_name,"customTitle":envelope.custom_title,"aiTitle":envelope.ai_title});
            if metadata["sessionId"].is_null() {
                metadata.as_object_mut().unwrap().remove("sessionId");
            }
            record.metadata.observe(&metadata, &source.tool);
        }
        if !use_gate || dialogue_candidate(&line, &source.tool) {
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                record.metadata.observe(&value, &source.tool);
                if let Some((role, text)) = conversations::transcript_message(&value, &source.tool)
                {
                    let text = conversations::dialogue_text(&text, usize::MAX);
                    if !text.is_empty()
                        && (source.tool != "codex"
                            || !record
                                .messages
                                .last()
                                .is_some_and(|m| m.role == role && m.text == text))
                    {
                        record.messages.push(Message {
                            end: record.offset,
                            role,
                            text,
                            timestamp: conversations::message_timestamp(&value, &source.tool),
                        });
                    }
                }
            }
        }
        if record.offset - checkpoint >= 32 * 1024 * 1024 {
            set_anchors(&mut record, reader.get_mut().get_mut())?;
            if let Some(store) = store {
                if let Err(e) = store.save(&record) {
                    record.cache_error = Some(format!("saving transcript cache: {e:#}"));
                }
            }
            checkpoint = record.offset;
        }
    }
    set_anchors(&mut record, reader.get_mut().get_mut())?;
    if let Some(store) = store {
        if let Err(e) = store.save(&record) {
            record.cache_error = Some(format!("saving transcript cache: {e:#}"));
        }
    }
    Ok(record)
}
fn read_at(file: &mut File, start: u64, length: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn sample(file: &mut File, start: u64, length: usize) -> Result<Fingerprint> {
    let bytes = read_at(file, start, length)?;
    Ok(Fingerprint {
        length,
        hash: super::store::fingerprint(&bytes),
    })
}
fn anchors_match(file: &mut File, record: &Record) -> Result<bool> {
    Ok(sample(file, 0, record.prefix.length)? == record.prefix
        && sample(
            file,
            record.offset.saturating_sub(record.anchor.length as u64),
            record.anchor.length,
        )? == record.anchor)
}
fn set_anchors(record: &mut Record, file: &mut File) -> Result<()> {
    // Use the already opened handle. Reopening the pathname could sample a
    // replacement while the projection still describes the retired inode.
    let position = file.stream_position()?;
    let length = record.offset.min(256) as usize;
    record.prefix = sample(file, 0, length)?;
    record.anchor = sample(file, record.offset - length as u64, length)?;
    file.seek(SeekFrom::Start(position))?;
    Ok(())
}
