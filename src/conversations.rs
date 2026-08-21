//! Cross-profile conversation discovery and launch planning.
//!
//! This module owns the translation from Claude/Codex native history into one
//! stable RTR catalog. Callers should not need to know profile-home layouts or
//! either tool's resume/fork dialect.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::config::Config;
use crate::paths::Paths;

/// One resumable top-level native conversation and the profile home that owns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub tool: String,
    pub profile: String,
    pub id: String,
    /// Exact mutable name recorded by the native tool, before display cleanup.
    pub native_name: Option<String>,
    pub title: String,
    pub first_prompt: Option<String>,
    pub cwd: PathBuf,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub enabled: bool,
    pub bypass: bool,
    pub transcript_path: PathBuf,
}

/// Filters applied while building a catalog; all filters are exact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConversationQuery {
    tool: Option<String>,
    profile: Option<String>,
    cwd: Option<PathBuf>,
    limit: Option<usize>,
}

impl ConversationQuery {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn with_tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }

    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// Stable, display-independent identity passed between the picker and RTR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationKey {
    pub tool: String,
    pub profile: String,
    pub id: String,
}

impl ConversationKey {
    pub fn new(tool: impl Into<String>, profile: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            profile: profile.into(),
            id: id.into(),
        }
    }

    pub fn encode(&self) -> String {
        format!(
            "v1:{}:{}:{}",
            self.tool,
            hex_encode(self.profile.as_bytes()),
            hex_encode(self.id.as_bytes())
        )
    }

    pub fn decode(value: &str) -> Result<Self> {
        let mut parts = value.split(':');
        let (Some("v1"), Some(tool), Some(profile), Some(id), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            anyhow::bail!("invalid conversation key");
        };
        if !matches!(tool, "claude" | "codex") {
            anyhow::bail!("unsupported conversation tool '{tool}'");
        }
        let profile = String::from_utf8(hex_decode(profile)?)
            .context("conversation key profile is not UTF-8")?;
        let id = String::from_utf8(hex_decode(id)?).context("conversation key ID is not UTF-8")?;
        if profile.is_empty() || id.is_empty() {
            anyhow::bail!("conversation key fields must not be empty");
        }
        Ok(Self::new(tool, profile, id))
    }
}

impl From<&Conversation> for ConversationKey {
    fn from(conversation: &Conversation) -> Self {
        Self::new(
            conversation.tool.clone(),
            conversation.profile.clone(),
            conversation.id.clone(),
        )
    }
}

/// Healthy conversations plus isolated scanner failures for observability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    pub conversations: Vec<Conversation>,
    pub diagnostics: Vec<String>,
}

/// Native operation requested after a conversation has been resolved exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    Resume,
    Fork,
}

impl OpenMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Fork => "fork",
        }
    }
}

/// Build a point-in-time catalog from every configured isolated profile home.
///
/// Codex summaries intentionally use only a bounded rollout prefix/tail plus
/// its small history/name indexes. Full Codex message bodies are read only by
/// [`inspect`] for one selected entry; Claude's much smaller transcripts are
/// streamed once because their names and metadata can occur anywhere.
pub fn query(paths: &Paths, query: &ConversationQuery) -> Result<Catalog> {
    let config = Config::load(&paths.config_file())?;
    let mut catalog = Catalog {
        conversations: Vec::new(),
        diagnostics: Vec::new(),
    };

    for (tool_name, tool) in &config.tools {
        for (profile_name, profile) in &tool.profiles {
            let home = paths.profile_home_dir(tool_name, profile_name);
            let result = match tool_name.as_str() {
                "codex" => scan_codex_home(
                    &home,
                    profile_name,
                    profile.enabled,
                    profile.bypass,
                    &mut catalog,
                ),
                "claude" => scan_claude_home(
                    &home,
                    profile_name,
                    profile.enabled,
                    profile.bypass,
                    &mut catalog,
                ),
                _ => continue,
            };
            if let Err(error) = result {
                catalog.diagnostics.push(format!(
                    "could not scan {tool_name}/{profile_name} at {}: {error:#}",
                    home.display()
                ));
            }
        }
    }

    catalog.conversations.retain(|conversation| {
        query
            .tool
            .as_deref()
            .is_none_or(|tool| conversation.tool == tool)
            && query
                .profile
                .as_deref()
                .is_none_or(|profile| conversation.profile == profile)
            && query
                .cwd
                .as_deref()
                .is_none_or(|cwd| paths_match(&conversation.cwd, cwd))
    });
    catalog.conversations.sort_by_key(|conversation| {
        (
            Reverse(conversation.updated_at),
            conversation.tool.clone(),
            conversation.profile.clone(),
            conversation.id.clone(),
        )
    });
    if let Some(limit) = query.limit {
        catalog.conversations.truncate(limit);
    }
    Ok(catalog)
}

/// Render a bounded transcript preview for one exact native conversation.
///
/// The picker invokes this repeatedly while selection changes, so lookup walks
/// directory metadata but reads only the tail of the selected transcript.
pub fn inspect(paths: &Paths, key: &ConversationKey) -> Result<String> {
    let config = Config::load(&paths.config_file())?;
    let tool = config.tool(&key.tool)?;
    if !tool.profiles.contains_key(&key.profile) {
        anyhow::bail!("profile '{}/{}' does not exist", key.tool, key.profile);
    }
    let home = paths.profile_home_dir(&key.tool, &key.profile);
    let path = locate_transcript(&home, key)?.with_context(|| {
        format!(
            "conversation '{}' was not found in {}/{}",
            key.id, key.tool, key.profile
        )
    })?;
    let messages = transcript_tail(&path, &key.tool)?;
    let mut output = format!(
        "{}/{}  {}\n{}\n",
        key.tool,
        key.profile,
        key.id,
        path.display()
    );
    if messages.is_empty() {
        output.push_str("\nNo text messages found near the end of this transcript.\n");
    } else {
        for (role, text) in messages {
            output.push('\n');
            output.push_str(&role);
            output.push_str(":\n");
            output.push_str(&text);
            output.push('\n');
        }
    }
    Ok(output)
}

/// Return exact identity/name matches, with native IDs taking precedence.
pub fn matches_selector<'a>(catalog: &'a Catalog, selector: &str) -> Result<Vec<&'a Conversation>> {
    if let Ok(key) = ConversationKey::decode(selector) {
        return Ok(catalog
            .conversations
            .iter()
            .filter(|conversation| {
                conversation.tool == key.tool
                    && conversation.profile == key.profile
                    && conversation.id == key.id
            })
            .collect());
    }
    let id_matches = catalog
        .conversations
        .iter()
        .filter(|conversation| conversation.id == selector)
        .collect::<Vec<_>>();
    if !id_matches.is_empty() {
        return Ok(id_matches);
    }
    Ok(catalog
        .conversations
        .iter()
        .filter(|conversation| conversation.native_name.as_deref() == Some(selector))
        .collect())
}

/// Resume or fork an exact native conversation in the profile that owns it.
pub async fn open(
    paths: &Paths,
    conversation: &Conversation,
    mode: OpenMode,
    extra_args: &[String],
) -> Result<i32> {
    if !conversation.transcript_path.is_file() {
        anyhow::bail!(
            "conversation transcript disappeared: {}",
            conversation.transcript_path.display()
        );
    }
    let mut args = native_open_args(&conversation.tool, &conversation.id, mode)?;
    args.extend_from_slice(extra_args);
    crate::runner::run_isolated_profile_tool(
        paths,
        &conversation.tool,
        &conversation.profile,
        &args,
        Some(&conversation.cwd),
    )
    .await
}

fn native_open_args(tool: &str, id: &str, mode: OpenMode) -> Result<Vec<String>> {
    let args = match (tool, mode) {
        ("codex", OpenMode::Resume) => vec!["resume", id],
        ("codex", OpenMode::Fork) => vec!["fork", id],
        ("claude", OpenMode::Resume) => vec!["--resume", id],
        ("claude", OpenMode::Fork) => vec!["--resume", id, "--fork-session"],
        _ => anyhow::bail!("unsupported conversation tool '{tool}'"),
    };
    Ok(args.into_iter().map(str::to_string).collect())
}

fn scan_codex_home(
    home: &Path,
    profile: &str,
    enabled: bool,
    bypass: bool,
    catalog: &mut Catalog,
) -> Result<()> {
    let prompts = codex_prompts(&home.join("history.jsonl"), &mut catalog.diagnostics)?;
    let names = codex_names(&home.join("session_index.jsonl"), &mut catalog.diagnostics)?;
    let mut seen_ids = HashSet::new();
    for path in codex_transcript_files(home)? {
        match codex_conversation(&path, profile, enabled, bypass, &prompts, &names) {
            Ok(Some(conversation)) if seen_ids.insert(conversation.id.clone()) => {
                catalog.conversations.push(conversation)
            }
            Ok(Some(_)) => {}
            Ok(None) => catalog
                .diagnostics
                .push(format!("{} has no usable session metadata", path.display())),
            Err(error) => catalog
                .diagnostics
                .push(format!("{}: {error:#}", path.display())),
        }
    }
    Ok(())
}

fn codex_prompts(path: &Path, diagnostics: &mut Vec<String>) -> Result<HashMap<String, String>> {
    let mut prompts = HashMap::new();
    visit_json_lines_if_exists(path, diagnostics, |record| {
        let Some(id) = record.get("session_id").and_then(Value::as_str) else {
            return;
        };
        let Some(prompt) = record.get("text").and_then(Value::as_str) else {
            return;
        };
        let prompt = display_text(prompt, 240);
        if !prompt.is_empty() {
            prompts.entry(id.to_string()).or_insert(prompt);
        }
    })?;
    Ok(prompts)
}

fn codex_names(path: &Path, diagnostics: &mut Vec<String>) -> Result<HashMap<String, String>> {
    let mut names = HashMap::new();
    visit_json_lines_if_exists(path, diagnostics, |record| {
        let Some(id) = record.get("id").and_then(Value::as_str) else {
            return;
        };
        match record.get("thread_name").and_then(Value::as_str) {
            Some(name) if !name.trim().is_empty() => {
                names.insert(id.to_string(), name.to_string());
            }
            _ => {
                names.remove(id);
            }
        }
    })?;
    Ok(names)
}

fn codex_conversation(
    path: &Path,
    profile: &str,
    enabled: bool,
    bypass: bool,
    prompts: &HashMap<String, String>,
    names: &HashMap<String, String>,
) -> Result<Option<Conversation>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut metadata = None;
    // Session metadata is the first rollout record today. The small bound also
    // tolerates historical preambles without ever turning cataloging into a
    // scan of a multi-gigabyte transcript corpus.
    for line in BufReader::new(file)
        .take(CODEX_METADATA_PREFIX_BYTES)
        .lines()
        .take(CODEX_METADATA_PREFIX_LINES)
    {
        let Ok(line) = line else { continue };
        let Ok(record): Result<Value, _> = serde_json::from_str(&line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) == Some("session_meta") {
            metadata = record.get("payload").cloned();
            break;
        }
    }
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let Some(id) = metadata.get("id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(cwd) = metadata.get("cwd").and_then(Value::as_str) else {
        return Ok(None);
    };
    let first_prompt = prompts.get(id).cloned();
    let native_name = names.get(id).cloned();
    let title = native_name
        .as_deref()
        .map(|name| display_text(name, 160))
        .filter(|name| !name.is_empty())
        .or_else(|| first_prompt.clone())
        .unwrap_or_else(|| id.to_string());
    Ok(Some(Conversation {
        tool: "codex".into(),
        profile: profile.into(),
        id: id.into(),
        native_name,
        title,
        first_prompt,
        cwd: PathBuf::from(cwd),
        started_at: metadata.get("timestamp").and_then(parse_timestamp),
        updated_at: latest_timestamp_in_tail(path, 64 * 1024)?.unwrap_or(modified_at(path)?),
        enabled,
        bypass,
        transcript_path: path.to_path_buf(),
    }))
}

fn scan_claude_home(
    home: &Path,
    profile: &str,
    enabled: bool,
    bypass: bool,
    catalog: &mut Catalog,
) -> Result<()> {
    for project in directories_if_exists(&home.join("projects"))? {
        for path in jsonl_files(&project)? {
            match claude_conversation(&path, profile, enabled, bypass) {
                Ok(Some(conversation)) => catalog.conversations.push(conversation),
                Ok(None) => catalog
                    .diagnostics
                    .push(format!("{} has no usable session metadata", path.display())),
                Err(error) => catalog
                    .diagnostics
                    .push(format!("{}: {error:#}", path.display())),
            }
        }
    }
    Ok(())
}

fn claude_conversation(
    path: &Path,
    profile: &str,
    enabled: bool,
    bypass: bool,
) -> Result<Option<Conversation>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut id = None;
    let mut cwd = None;
    let mut started_at = None;
    let mut updated_at = None;
    let mut first_prompt = None;
    let mut generated_title = None;
    let mut explicit_name = None;

    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(record): Result<Value, _> = serde_json::from_str(&line) else {
            continue;
        };
        if let Some(value) = record
            .get("sessionId")
            .or_else(|| record.get("session_id"))
            .and_then(Value::as_str)
        {
            id = Some(value.to_string());
        }
        if let Some(value) = record.get("cwd").and_then(Value::as_str) {
            cwd = Some(PathBuf::from(value));
        }
        if started_at.is_none() {
            started_at = record.get("timestamp").and_then(parse_timestamp);
        }
        if let Some(timestamp) = record.get("timestamp").and_then(parse_timestamp) {
            if updated_at.is_none_or(|current| timestamp > current) {
                updated_at = Some(timestamp);
            }
        }
        match record.get("type").and_then(Value::as_str) {
            Some("agent-name") => {
                explicit_name = record
                    .get("agentName")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .map(str::to_string);
            }
            Some("ai-title") => {
                generated_title = record
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .map(|name| display_text(name, 160))
                    .filter(|name| !name.is_empty());
            }
            Some("user")
                if first_prompt.is_none() && record.get("isMeta") != Some(&Value::Bool(true)) =>
            {
                first_prompt = claude_user_text(&record).map(|text| display_text(&text, 240));
            }
            _ => {}
        }
    }

    let (Some(id), Some(cwd)) = (id, cwd) else {
        return Ok(None);
    };
    let native_name = explicit_name.clone();
    let title = explicit_name
        .as_deref()
        .map(|name| display_text(name, 160))
        .filter(|name| !name.is_empty())
        .or(generated_title)
        .or_else(|| first_prompt.clone())
        .unwrap_or_else(|| id.clone());
    Ok(Some(Conversation {
        tool: "claude".into(),
        profile: profile.into(),
        id,
        native_name,
        title,
        first_prompt,
        cwd,
        started_at,
        updated_at: updated_at.unwrap_or(modified_at(path)?),
        enabled,
        bypass,
        transcript_path: path.to_path_buf(),
    }))
}

fn claude_user_text(record: &Value) -> Option<String> {
    let content = record.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let text = content
        .as_array()?
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn visit_json_lines_if_exists(
    path: &Path,
    diagnostics: &mut Vec<String>,
    mut visit: impl FnMut(&Value),
) -> Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let Ok(line) = line else {
            diagnostics.push(format!(
                "{}:{} could not be read",
                path.display(),
                index + 1
            ));
            continue;
        };
        match serde_json::from_str(&line) {
            Ok(record) => visit(&record),
            Err(_) => diagnostics.push(format!(
                "{}:{} is not valid JSON",
                path.display(),
                index + 1
            )),
        }
    }
    Ok(())
}

fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn modified_at(path: &Path) -> Result<DateTime<Utc>> {
    let modified = path
        .metadata()
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .modified()
        .with_context(|| format!("reading modification time for {}", path.display()))?;
    Ok(modified.into())
}

fn latest_timestamp_in_tail(path: &Path, bytes: u64) -> Result<Option<DateTime<Utc>>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let length = file
        .metadata()
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .len();
    let offset = length.saturating_sub(bytes);
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("seeking {}", path.display()))?;
    let mut reader = BufReader::new(file).take(bytes);
    if offset > 0 {
        let mut partial = Vec::new();
        reader.read_until(b'\n', &mut partial)?;
    }
    let mut latest = None;
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        for value in [
            record.get("timestamp"),
            record
                .get("payload")
                .and_then(|payload| payload.get("timestamp")),
        ]
        .into_iter()
        .flatten()
        {
            let Some(timestamp) = parse_timestamp(value) else {
                continue;
            };
            if latest.is_none_or(|current| timestamp > current) {
                latest = Some(timestamp);
            }
        }
    }
    Ok(latest)
}

fn display_text(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    let mut pending_space = false;
    let mut characters_written = 0;
    for character in value
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
    {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space && characters_written < max_chars {
            output.push(' ');
            characters_written += 1;
        }
        pending_space = false;
        if characters_written == max_chars {
            break;
        }
        output.push(character);
        characters_written += 1;
    }
    output
}

fn paths_match(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn directories_if_exists(path: &Path) -> Result<Vec<PathBuf>> {
    entries_if_exists(path, true)
}

fn jsonl_files(path: &Path) -> Result<Vec<PathBuf>> {
    entries_if_exists(path, false)
}

fn entries_if_exists(path: &Path, directories: bool) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", path.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("reading file type for {}", entry.path().display()))?;
        let matches = if directories {
            file_type.is_dir()
        } else {
            file_type.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        };
        if matches {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn jsonl_files_recursive(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for path in entries_if_exists(&directory, true)? {
            pending.push(path);
        }
        files.extend(jsonl_files(&directory)?);
    }
    files.sort();
    Ok(files)
}

fn codex_transcript_files(home: &Path) -> Result<Vec<PathBuf>> {
    // Codex moves archived rollouts out of `sessions/` without changing their
    // native IDs. Active files come first so a transient duplicate resolves to
    // the live copy while both stores remain searchable.
    let mut files = jsonl_files_recursive(&home.join("sessions"))?;
    files.extend(jsonl_files_recursive(&home.join("archived_sessions"))?);
    Ok(files)
}

fn locate_transcript(home: &Path, key: &ConversationKey) -> Result<Option<PathBuf>> {
    match key.tool.as_str() {
        "claude" => {
            for project in directories_if_exists(&home.join("projects"))? {
                let candidate = project.join(format!("{}.jsonl", key.id));
                if candidate.is_file() {
                    return Ok(Some(candidate));
                }
            }
            Ok(None)
        }
        "codex" => Ok(codex_transcript_files(home)?.into_iter().find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".jsonl"))
                .is_some_and(|stem| {
                    stem == key.id
                        || stem
                            .strip_suffix(&key.id)
                            .is_some_and(|prefix| prefix.ends_with('-'))
                })
        })),
        _ => anyhow::bail!("unsupported conversation tool '{}'", key.tool),
    }
}

const PREVIEW_BYTES: u64 = 512 * 1024;
const PREVIEW_MESSAGES: usize = 24;
const PREVIEW_MESSAGE_CHARS: usize = 1_200;
const CODEX_METADATA_PREFIX_BYTES: u64 = 256 * 1024;
const CODEX_METADATA_PREFIX_LINES: usize = 16;

fn transcript_tail(path: &Path, tool: &str) -> Result<Vec<(String, String)>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let length = file
        .metadata()
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .len();
    let offset = length.saturating_sub(PREVIEW_BYTES);
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("seeking {}", path.display()))?;

    // A seek into the tail normally lands in the middle of a JSON record. The
    // first partial line is discarded so previews never interpret fragments.
    let mut reader = BufReader::new(file).take(PREVIEW_BYTES);
    if offset > 0 {
        let mut partial = Vec::new();
        reader.read_until(b'\n', &mut partial)?;
    }
    let mut messages = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let extracted = match tool {
            "claude" => claude_preview_message(&record),
            "codex" => codex_preview_message(&record),
            _ => None,
        };
        let Some((role, text)) = extracted else {
            continue;
        };
        let text = display_text(&text, PREVIEW_MESSAGE_CHARS);
        if text.is_empty() {
            continue;
        }
        // Codex commonly records the same user input as both an event and a
        // response item. Consecutive de-duplication keeps the preview readable.
        if messages
            .last()
            .is_some_and(|previous| previous == &(role.clone(), text.clone()))
        {
            continue;
        }
        messages.push((role, text));
        if messages.len() > PREVIEW_MESSAGES {
            messages.remove(0);
        }
    }
    Ok(messages)
}

fn claude_preview_message(record: &Value) -> Option<(String, String)> {
    let role = match record.get("type").and_then(Value::as_str)? {
        "user" => "user",
        "assistant" => "assistant",
        _ => return None,
    };
    let text = message_content_text(record.get("message")?.get("content")?)?;
    Some((role.to_string(), text))
}

fn codex_preview_message(record: &Value) -> Option<(String, String)> {
    let record_type = record.get("type").and_then(Value::as_str)?;
    let payload = record.get("payload")?;
    if record_type == "event_msg"
        && payload.get("type").and_then(Value::as_str) == Some("user_message")
    {
        return payload
            .get("message")
            .and_then(Value::as_str)
            .map(|text| ("user".to_string(), text.to_string()));
    }
    if record_type != "response_item"
        || payload.get("type").and_then(Value::as_str) != Some("message")
    {
        return None;
    }
    let role = payload.get("role").and_then(Value::as_str)?.to_string();
    let text = message_content_text(payload.get("content")?)?;
    Some((role, text))
}

fn message_content_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let text = content
        .as_array()?
        .iter()
        .filter_map(|part| {
            part.get("text")
                .or_else(|| part.get("input_text"))
                .or_else(|| part.get("output_text"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        anyhow::bail!("invalid hex field in conversation key");
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => anyhow::bail!("invalid hex field in conversation key"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn test_paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
        }
    }

    #[test]
    fn catalog_discovers_native_names_across_claude_and_codex_profiles() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            paths.config_file(),
            r#"
[tools.codex]
command = ["codex"]
[tools.codex.profiles.eng]

[tools.claude]
command = ["claude"]
[tools.claude.profiles.personal]
"#,
        )
        .unwrap();

        let codex_id = "019f0000-0000-7000-8000-000000000001";
        let codex_home = paths.profile_home_dir("codex", "eng");
        let codex_session = codex_home
            .join("sessions/2026/08/20")
            .join(format!("rollout-{codex_id}.jsonl"));
        fs::create_dir_all(codex_session.parent().unwrap()).unwrap();
        fs::write(
            &codex_session,
            format!(
                "{}\n",
                serde_json::json!({
                    "timestamp": "2026-08-20T18:00:00Z",
                    "type": "session_meta",
                    "payload": {
                        "id": codex_id,
                        "cwd": "/work/codex",
                        "timestamp": "2026-08-20T18:00:00Z"
                    }
                })
            ),
        )
        .unwrap();
        fs::write(
            codex_home.join("history.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"session_id": codex_id, "ts": 1, "text": "original codex prompt"})
            ),
        )
        .unwrap();
        fs::write(
            codex_home.join("session_index.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "id": codex_id,
                    "thread_name": "renamed  codex",
                    "updated_at": "2026-08-20T18:01:00Z"
                })
            ),
        )
        .unwrap();

        let claude_id = "10000000-0000-4000-8000-000000000002";
        let claude_session = paths
            .profile_home_dir("claude", "personal")
            .join("projects/-work-claude")
            .join(format!("{claude_id}.jsonl"));
        fs::create_dir_all(claude_session.parent().unwrap()).unwrap();
        let claude_records = [
            serde_json::json!({
                "type": "user",
                "sessionId": claude_id,
                "cwd": "/work/claude",
                "timestamp": "2026-08-20T17:00:00Z",
                "message": {"role": "user", "content": "original claude prompt"}
            }),
            serde_json::json!({
                "type": "ai-title",
                "sessionId": claude_id,
                "aiTitle": "Renamed Claude"
            }),
            serde_json::json!({
                "type": "agent-name",
                "sessionId": claude_id,
                "agentName": "Renamed   Claude"
            }),
        ];
        fs::write(
            &claude_session,
            claude_records
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let catalog = query(&paths, &ConversationQuery::all()).unwrap();

        assert_eq!(catalog.conversations.len(), 2);
        let codex = catalog
            .conversations
            .iter()
            .find(|conversation| conversation.tool == "codex")
            .unwrap();
        assert_eq!(codex.profile, "eng");
        assert_eq!(codex.native_name.as_deref(), Some("renamed  codex"));
        assert_eq!(codex.title, "renamed codex");
        assert_eq!(codex.first_prompt.as_deref(), Some("original codex prompt"));
        let claude = catalog
            .conversations
            .iter()
            .find(|conversation| conversation.tool == "claude")
            .unwrap();
        assert_eq!(claude.profile, "personal");
        assert_eq!(claude.title, "Renamed Claude");
        assert_eq!(claude.native_name.as_deref(), Some("Renamed   Claude"));
        assert_eq!(
            claude.first_prompt.as_deref(),
            Some("original claude prompt")
        );
    }

    #[test]
    fn conversation_keys_round_trip_without_exposing_display_fields() {
        let key = ConversationKey::new("claude", "work team", "session:id/with spaces");
        let encoded = key.encode();

        assert_eq!(ConversationKey::decode(&encoded).unwrap(), key);
        assert!(!encoded.contains("work team"));
        assert!(!encoded.contains("session:id/with spaces"));
        assert!(ConversationKey::decode("v2:claude:00:00").is_err());
        assert!(ConversationKey::decode("v1:other:00:00").is_err());
    }

    #[test]
    fn native_open_arguments_preserve_each_tools_resume_and_fork_contract() {
        assert_eq!(
            native_open_args("codex", "thread", OpenMode::Resume).unwrap(),
            ["resume", "thread"]
        );
        assert_eq!(
            native_open_args("codex", "thread", OpenMode::Fork).unwrap(),
            ["fork", "thread"]
        );
        assert_eq!(
            native_open_args("claude", "session", OpenMode::Resume).unwrap(),
            ["--resume", "session"]
        );
        assert_eq!(
            native_open_args("claude", "session", OpenMode::Fork).unwrap(),
            ["--resume", "session", "--fork-session"]
        );
    }

    #[test]
    fn selector_matches_exact_native_names_without_confusing_them_for_identity() {
        let mut first = test_conversation("codex", "eng", "id-one", "same name");
        let mut second = test_conversation("claude", "personal", "id-two", "same name");
        let catalog = Catalog {
            conversations: vec![first.clone(), second.clone()],
            diagnostics: Vec::new(),
        };

        assert_eq!(matches_selector(&catalog, "id-one").unwrap(), vec![&first]);
        assert_eq!(matches_selector(&catalog, "same name").unwrap().len(), 2);
        second.native_name = Some("id-one".into());
        let catalog = Catalog {
            conversations: vec![first.clone(), second],
            diagnostics: Vec::new(),
        };
        assert_eq!(matches_selector(&catalog, "id-one").unwrap(), vec![&first]);
        first.title = "renamed".into();
        assert_eq!(ConversationKey::from(&first).id, "id-one");

        first.native_name = Some("v1:not-an-opaque-key".into());
        let catalog = Catalog {
            conversations: vec![first.clone()],
            diagnostics: Vec::new(),
        };
        assert_eq!(
            matches_selector(&catalog, "v1:not-an-opaque-key").unwrap(),
            vec![&first]
        );
    }

    #[test]
    fn catalog_and_preview_include_codex_archived_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            paths.config_file(),
            r#"
[tools.codex]
command = ["codex"]
[tools.codex.profiles.eng]
"#,
        )
        .unwrap();
        let id = "archived-codex-id";
        let transcript = paths
            .profile_home_dir("codex", "eng")
            .join("archived_sessions/rollout-archived-codex-id.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let records = [
            serde_json::json!({
                "type": "session_meta",
                "payload": {"id": id, "cwd": "/work/archive", "timestamp": "2026-08-20T18:00:00Z"}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "archived answer"}]}
            }),
        ];
        fs::write(
            &transcript,
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let catalog = query(&paths, &ConversationQuery::all()).unwrap();

        assert_eq!(catalog.conversations.len(), 1);
        assert_eq!(catalog.conversations[0].transcript_path, transcript);
        let preview = inspect(&paths, &ConversationKey::new("codex", "eng", id)).unwrap();
        assert!(preview.contains("assistant:\narchived answer"), "{preview}");
    }

    #[test]
    fn codex_metadata_scan_is_bounded_by_bytes_as_well_as_records() {
        let temp = tempfile::tempdir().unwrap();
        let transcript = temp.path().join("rollout-bounded.jsonl");
        let oversized_preamble = "x".repeat(CODEX_METADATA_PREFIX_BYTES as usize + 1);
        let metadata = serde_json::json!({
            "type": "session_meta",
            "payload": {"id": "too-late", "cwd": "/work"}
        });
        fs::write(&transcript, format!("{oversized_preamble}\n{metadata}\n")).unwrap();

        let conversation = codex_conversation(
            &transcript,
            "eng",
            true,
            false,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();

        assert!(conversation.is_none());
    }

    #[test]
    fn catalog_filters_by_profile_tool_and_canonical_current_directory() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            paths.config_file(),
            r#"
[tools.codex]
command = ["codex"]
[tools.codex.profiles.work]
[tools.codex.profiles.personal]
"#,
        )
        .unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let alias = temp.path().join("project-alias");
        std::os::unix::fs::symlink(&project, &alias).unwrap();

        for (profile, id, cwd) in [
            ("work", "work-id", alias.as_path()),
            ("personal", "personal-id", temp.path()),
        ] {
            let transcript = paths
                .profile_home_dir("codex", profile)
                .join("sessions/2026/08/20")
                .join(format!("rollout-{id}.jsonl"));
            fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            fs::write(
                transcript,
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "session_meta",
                        "payload": {"id": id, "cwd": cwd, "timestamp": "2026-08-20T18:00:00Z"}
                    })
                ),
            )
            .unwrap();
        }

        let catalog = query(
            &paths,
            &ConversationQuery::all()
                .with_tool("codex")
                .with_profile("work")
                .with_cwd(&project)
                .with_limit(1),
        )
        .unwrap();

        assert_eq!(catalog.conversations.len(), 1);
        assert_eq!(catalog.conversations[0].id, "work-id");
    }

    #[test]
    fn inspect_reads_text_from_only_the_selected_transcript_tail() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            paths.config_file(),
            r#"
[tools.claude]
command = ["claude"]
[tools.claude.profiles.eng]
"#,
        )
        .unwrap();
        let id = "preview-id";
        let transcript = paths
            .profile_home_dir("claude", "eng")
            .join("projects/-work")
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let records = [
            serde_json::json!({
                "type": "user", "sessionId": id, "cwd": "/work",
                "message": {"content": "question"}
            }),
            serde_json::json!({
                "type": "assistant", "sessionId": id,
                "message": {"content": [{"type": "text", "text": "answer"}]}
            }),
        ];
        fs::write(
            transcript,
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let preview = inspect(&paths, &ConversationKey::new("claude", "eng", id)).unwrap();

        assert!(preview.contains("claude/eng  preview-id"), "{preview}");
        assert!(preview.contains("user:\nquestion"), "{preview}");
        assert!(preview.contains("assistant:\nanswer"), "{preview}");
    }

    fn test_conversation(tool: &str, profile: &str, id: &str, title: &str) -> Conversation {
        Conversation {
            tool: tool.into(),
            profile: profile.into(),
            id: id.into(),
            native_name: Some(title.into()),
            title: title.into(),
            first_prompt: None,
            cwd: PathBuf::from("/work"),
            started_at: None,
            updated_at: DateTime::parse_from_rfc3339("2026-08-20T18:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            enabled: true,
            bypass: false,
            transcript_path: PathBuf::from("/transcript"),
        }
    }
}
