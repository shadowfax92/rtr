//! Relocates references to known native asset stores, without rewriting general
//! home paths or IDs inside dialogue. Each transfer owns fresh destination roots;
//! unpublished roots are removed on error, published session assets are retained.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::{checked_path, private_file, private_subdir};

struct Rule {
    source: PathBuf,
    destination: PathBuf,
    publication_root: PathBuf,
}

pub(super) struct Assets {
    source_home: PathBuf,
    target_home: PathBuf,
    stage: PathBuf,
    asset_root: PathBuf,
    claude_project: Option<PathBuf>,
    aliases: Vec<PathBuf>,
    rules: Vec<Rule>,
    roots: HashSet<PathBuf>,
    owned: Vec<PathBuf>,
    committed: bool,
}

impl Assets {
    pub(super) fn new(source: &Path, target: &Path, stage: &Path, id: &str) -> Result<Self> {
        if !std::fs::symlink_metadata(source)?.file_type().is_dir() {
            bail!("source profile home must be a real directory");
        }
        let canonical = source.canonicalize()?;
        let mut aliases = vec![source.to_path_buf()];
        if canonical != source {
            aliases.push(canonical);
        }
        let asset_root = PathBuf::from(".rtr-fork-assets").join(id);
        let mut rules = Vec::new();
        // Only session content stores are eligible. Config, credentials, caches
        // of account state, and arbitrary files mentioned in dialogue stay outside.
        for directory in [
            "attachments",
            "generated_images",
            "paste-cache",
            ".rtr-fork-assets",
        ] {
            rules.push(Rule {
                source: directory.into(),
                destination: asset_root.join(directory),
                publication_root: asset_root.clone(),
            });
        }
        Ok(Self {
            source_home: source.into(),
            target_home: target.into(),
            stage: stage.join("assets"),
            asset_root,
            claude_project: None,
            aliases,
            rules,
            roots: HashSet::new(),
            owned: Vec::new(),
            committed: false,
        })
    }

    pub(super) fn claude_project(&mut self, project: &Path) {
        self.claude_project = Some(project.into());
    }

    /// Native Claude forks retain ancestor session paths. Register stores from
    /// actual references (including missing ones, which must fail validation),
    /// without walking or importing every other conversation in the project.
    fn register_claude_references(&mut self, text: &str) {
        let Some(project) = &self.claude_project else {
            return;
        };
        for base in [
            project.clone(),
            PathBuf::from("image-cache"),
            PathBuf::from("uploads"),
        ] {
            for alias in &self.aliases {
                let prefix = format!("{}/", alias.join(&base).display());
                for (_, rest) in text
                    .match_indices(&prefix)
                    .map(|(index, matched)| (index, &text[index + matched.len()..]))
                {
                    let mut components = rest.split('/');
                    let Some(session) = components.next().filter(|id| !id.is_empty()) else {
                        continue;
                    };
                    let mut source = base.join(session);
                    if &base == project {
                        let Some(store @ ("tool-results" | "subagents")) = components.next() else {
                            continue;
                        };
                        source.push(store);
                    }
                    if !self
                        .rules
                        .iter()
                        .any(|rule| source.starts_with(&rule.source))
                    {
                        self.rules.push(Rule {
                            destination: self.asset_root.join(&source),
                            source,
                            publication_root: self.asset_root.clone(),
                        });
                    }
                }
            }
        }
    }

    pub(super) fn session_directory(&mut self, source: &Path, destination: &Path) -> Result<()> {
        let relative = source.strip_prefix(&self.source_home)?.to_path_buf();
        self.rules.push(Rule {
            source: relative.clone(),
            destination: destination.into(),
            publication_root: destination.into(),
        });
        match std::fs::symlink_metadata(source) {
            Ok(_) => {
                self.stage_path(&relative, destination)?;
                self.roots.insert(destination.into());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    pub(super) fn rewrite(&mut self, value: &mut Value) -> Result<()> {
        match value {
            // Ordinary dialogue is not a file manifest. Only native persisted
            // output notices embed asset paths in otherwise unstructured text.
            Value::String(text) => *text = self.rewrite_notices(text)?,
            Value::Array(items) => {
                for item in items {
                    self.rewrite(item)?;
                }
            }
            Value::Object(fields) => {
                for (key, item) in fields {
                    if matches!(
                        key.as_str(),
                        "path"
                            | "file_path"
                            | "filePath"
                            | "image_path"
                            | "imagePath"
                            | "image_url"
                            | "local_images"
                            | "attachment_path"
                            | "attachmentPath"
                            | "persistedOutputPath"
                            | "url"
                    ) {
                        self.rewrite_paths(item)?;
                    } else {
                        self.rewrite(item)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn rewrite_paths(&mut self, value: &mut Value) -> Result<()> {
        match value {
            Value::String(text) => *text = self.rewrite_text(text)?,
            Value::Array(items) => {
                for item in items {
                    self.rewrite_paths(item)?;
                }
            }
            _ => self.rewrite(value)?,
        }
        Ok(())
    }

    fn rewrite_notices(&mut self, text: &str) -> Result<String> {
        let mut output = String::new();
        let mut rest = text;
        while let Some(start) = rest.find("<persisted-output>") {
            output.push_str(&rest[..start]);
            rest = &rest[start..];
            let Some(end) = rest.find("</persisted-output>") else {
                break;
            };
            let end = end + "</persisted-output>".len();
            // Limit matching to the path notice, never the output preview below.
            for line in rest[..end].split_inclusive('\n') {
                if line.trim_start().starts_with("Full output saved to:") {
                    output.push_str(&self.rewrite_text(line)?);
                } else {
                    output.push_str(line);
                }
            }
            rest = &rest[end..];
        }
        output.push_str(rest);
        let mut images = String::new();
        let mut rest = output.as_str();
        while let Some(start) = rest.find("[Image: source: ") {
            let path_start = start + "[Image: source: ".len();
            let Some(length) = rest[path_start..].find(']') else {
                break;
            };
            images.push_str(&rest[..path_start]);
            images.push_str(&self.rewrite_text(&rest[path_start..path_start + length])?);
            rest = &rest[path_start + length..];
        }
        images.push_str(rest);
        Ok(images)
    }

    pub(super) fn rewrite_companion_transcripts(
        &mut self,
        old_id: &str,
        new_id: &str,
        parent_uuids: &HashSet<String>,
    ) -> Result<()> {
        let mut visited = HashSet::new();
        loop {
            let mut pending = vec![self.stage.clone()];
            let mut transcripts = Vec::new();
            while let Some(directory) = pending.pop() {
                if !directory.exists() {
                    continue;
                }
                for entry in std::fs::read_dir(directory)? {
                    let entry = entry?;
                    let path = entry.path();
                    if entry.file_type()?.is_dir() {
                        pending.push(path);
                    } else if path.extension().is_some_and(|ext| ext == "jsonl")
                        && path.components().any(|c| c.as_os_str() == "subagents")
                        && visited.insert(path.clone())
                    {
                        transcripts.push(path);
                    }
                }
            }
            if transcripts.is_empty() {
                break;
            }
            for path in transcripts {
                // Historical subagent transcripts are passive assets. Resolve
                // parent hydration against the copied root's actual UUID boundary
                // and remove runtime ownership just as for its main transcript.
                use std::io::{BufWriter, Write};
                let directory = path.parent().context("subagent transcript has no parent")?;
                let snapshot = super::Snapshot::open(&self.stage, &path, None)?;
                let mut temp = tempfile::NamedTempFile::new_in(directory)?;
                let mut writer = BufWriter::new(temp.as_file_mut());
                snapshot.visit(|mut row| {
                    if super::claude::is_runtime_record(&row) { return Ok(()); }
                    if row["type"] == "fork-context-ref" {
                        let boundary = row["parentLastUuid"].as_str().context("Claude subagent has no parent history boundary")?;
                        if !parent_uuids.contains(boundary) {
                            bail!("Claude subagent parent history is outside the selected conversation; cannot copy independently");
                        }
                        row["parentSessionId"] = Value::String(new_id.into());
                    }
                    for key in ["sessionId", "session_id"] {
                        if row[key].as_str() == Some(old_id) { row[key] = Value::String(new_id.into()); }
                    }
                    self.rewrite(&mut row)?;
                    super::write_record(&mut writer, &row)
                })?;
                writer.flush()?;
                drop(writer);
                temp.as_file().sync_all()?;
                temp.persist(&path)?;
            }
            // A companion can reference another companion. Discover newly staged
            // files on the next pass, while the visited set breaks dependency cycles.
        }
        Ok(())
    }

    fn rewrite_text(&mut self, text: &str) -> Result<String> {
        self.register_claude_references(text);
        let mut output = text.to_string();
        for index in 0..self.rules.len() {
            for alias in self.aliases.clone() {
                let prefix = alias
                    .join(&self.rules[index].source)
                    .to_string_lossy()
                    .into_owned();
                let mut offset = 0;
                while let Some(found) = output[offset..].find(&prefix) {
                    let start = offset + found;
                    let remainder = &output[start..];
                    if remainder.len() > prefix.len() && !remainder[prefix.len()..].starts_with('/')
                    {
                        offset = start + prefix.len();
                        continue;
                    }
                    // Native tool-output notices embed paths in text, while image
                    // records can contain a path as the entire string. Prefer the
                    // longest existing path, preserving spaces and trailing prose.
                    let end = remainder
                        .find(['\n', '\r', '\t', '<', '>', '"', '\'', '`', '\0'])
                        .unwrap_or(remainder.len());
                    let candidate = remainder[..end].trim_end();
                    let mut length = candidate.len();
                    let source_relative = loop {
                        let path = Path::new(&candidate[..length]);
                        let suffix = path.strip_prefix(&alias)?;
                        if self.source_home.join(suffix).try_exists()? {
                            break suffix.to_path_buf();
                        }
                        let last = candidate[..length].chars().next_back();
                        if last
                            .is_some_and(|c| matches!(c, '.' | ',' | ';' | ':' | ')' | ']' | '}'))
                        {
                            length -= last.unwrap().len_utf8();
                        } else if let Some(space) = candidate[..length].rfind(char::is_whitespace) {
                            length = space;
                        } else {
                            bail!("missing conversation asset: {}", path.display());
                        }
                        if length < prefix.len() {
                            bail!("cannot resolve conversation asset in {candidate}");
                        }
                    };
                    let rule = &self.rules[index];
                    let suffix = source_relative.strip_prefix(&rule.source)?;
                    let target_relative = rule.destination.join(suffix);
                    let publication_root = rule.publication_root.clone();
                    self.stage_path(&source_relative, &target_relative)?;
                    self.roots.insert(publication_root);
                    let replacement = self
                        .target_home
                        .join(&target_relative)
                        .to_string_lossy()
                        .into_owned();
                    output.replace_range(start..start + length, &replacement);
                    offset = start + replacement.len();
                }
            }
        }
        Ok(output)
    }

    fn stage_path(&self, source: &Path, target: &Path) -> Result<()> {
        let source = checked_path(&self.source_home, source)?;
        let destination = self.stage.join(target);
        if destination.exists() {
            return Ok(());
        }
        copy_tree(&source, &destination)
    }

    pub(super) fn publish(&mut self) -> Result<()> {
        for relative in &self.roots {
            let parent = relative.parent().context("asset root has no parent")?;
            private_subdir(&self.target_home, parent)?;
            let destination = self.target_home.join(relative);
            // Reserve each root exclusively before writing into it. Its fresh ID
            // makes it independent of existing sessions and concurrent transfers.
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&destination)?;
            self.owned.push(destination.clone());
            publish_tree(&self.stage.join(relative), &destination)?;
        }
        Ok(())
    }

    pub(super) fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for Assets {
    fn drop(&mut self) {
        if !self.committed {
            for path in &self.owned {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        bail!("conversation asset is a symlink: {}", source.display());
    }
    if metadata.is_dir() {
        crate::paths::ensure_private_dir(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        crate::paths::ensure_private_dir(destination.parent().context("asset has no parent")?)?;
        let mut reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)?;
        let before = reader.metadata()?;
        let mut writer = private_file(destination)?;
        let count = std::io::copy(&mut (&mut reader).take(before.len()), &mut writer)?;
        let after = std::fs::symlink_metadata(source)?;
        if count != before.len()
            || (before.dev(), before.ino()) != (after.dev(), after.ino())
            || after.len() < before.len()
        {
            bail!(
                "conversation asset changed while copying: {}",
                source.display()
            );
        }
        writer.sync_all()?;
    } else {
        bail!("unsupported conversation asset: {}", source.display());
    }
    Ok(())
}

fn publish_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::DirBuilder::new().mode(0o700).create(&target)?;
            publish_tree(&entry.path(), &target)?;
        } else {
            std::fs::hard_link(entry.path(), target)?;
        }
    }
    Ok(())
}
