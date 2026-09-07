//! Copies one native conversation into a different isolated home. Native CLIs
//! still own replay and indexing; these adapters preserve stored records, give
//! the copy a fresh identity, and publish its assets before its transcript.

mod assets;
mod claude;
mod codex;
#[cfg(test)]
mod tests;

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;
use uuid::Uuid;

use crate::conversations::Conversation;
use crate::paths::Paths;

/// Prepare a private copy and publish its transcript last. A completed copy is
/// durable native state; callers must retain it even if the subsequent launch fails.
pub(crate) fn copy(paths: &Paths, source: &Conversation, target_home: &Path) -> Result<String> {
    let source_home = paths.profile_home_dir(&source.tool, &source.profile);
    let id = Uuid::new_v4().to_string();
    let stage = tempfile::Builder::new()
        .prefix(".rtr-transfer-")
        .tempdir_in(target_home)?;
    let mut assets = assets::Assets::new(&source_home, target_home, stage.path(), &id)?;
    let temporary = stage.path().join("transcript.jsonl");
    let mut writer = BufWriter::new(private_file(&temporary)?);
    let relative = match source.tool.as_str() {
        "codex" => codex::copy(source, &source_home, &id, &mut writer, &mut assets)?,
        "claude" => claude::copy(source, &source_home, &id, &mut writer, &mut assets)?,
        _ => bail!("unsupported conversation tool '{}'", source.tool),
    };
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);

    let parent = relative.parent().context("transcript has no parent")?;
    let directory = private_subdir(target_home, parent)?;
    let destination = directory.join(relative.file_name().context("transcript has no filename")?);
    assets.publish()?;
    // A same-filesystem hard link publishes atomically and refuses overwrite.
    // Unlike rename, even an improbable ID collision cannot replace native data.
    std::fs::hard_link(&temporary, &destination).with_context(|| {
        format!(
            "publishing copied conversation at {}",
            destination.display()
        )
    })?;
    assets.commit();
    if source.tool == "codex" {
        if let Some(name) = &source.native_name {
            if let Err(error) = append_codex_name(target_home, &id, name) {
                eprintln!(
                    "rtr: copied conversation {id}, but could not preserve its name: {error:#}"
                );
            }
        }
    }
    Ok(id)
}

fn append_codex_name(home: &Path, id: &str, name: &str) -> Result<()> {
    let path = home.join("session_index.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let mut line = serde_json::to_vec(&serde_json::json!({
        "id": id, "thread_name": name, "updated_at": chrono::Utc::now().to_rfc3339(),
    }))?;
    line.push(b'\n');
    // Native writers also append. One append write keeps the small name record
    // together without assuming the native tool honors an RTR advisory lock.
    if file.write(&line)? != line.len() {
        bail!("short write to {}", path.display());
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))
}

/// Walk native relative paths without following directory symlinks. Both sides
/// contain private account state, so a copied asset cannot escape either home.
fn checked_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            bail!("unsafe native path: {}", relative.display());
        };
        path.push(part);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("reading native path {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("native path is a symlink: {}", path.display());
        }
    }
    Ok(path)
}

fn private_subdir(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            bail!("unsafe native directory: {}", relative.display());
        };
        path.push(part);
        crate::paths::ensure_private_dir(&path)?;
    }
    Ok(path)
}

/// Read a fixed byte prefix from one append-only native file. Native writers do
/// not take RTR locks: allow later appends, reject replacement/truncation, and
/// omit only a final unfinished record from a live source (never an inherited prefix).
struct Snapshot {
    path: PathBuf,
    file: File,
    length: u64,
    inode: (u64, u64),
    exact: bool,
}

impl Snapshot {
    fn open(root: &Path, path: &Path, limit: Option<u64>) -> Result<Self> {
        let relative = path
            .strip_prefix(root)
            .context("transcript is outside the source home")?;
        checked_path(root, relative)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("not a native transcript file: {}", path.display());
        }
        let length = limit.unwrap_or(metadata.len());
        if length > metadata.len() {
            bail!("history prefix is missing from {}", path.display());
        }
        Ok(Self {
            path: path.into(),
            file,
            length,
            inode: (metadata.dev(), metadata.ino()),
            exact: limit.is_some(),
        })
    }

    fn visit(self, mut visit: impl FnMut(Value) -> Result<()>) -> Result<()> {
        let mut reader = BufReader::new((&self.file).take(self.length));
        let mut bytes = Vec::new();
        let mut read = 0;
        loop {
            bytes.clear();
            let count = reader.read_until(b'\n', &mut bytes)?;
            if count == 0 {
                break;
            }
            read += count as u64;
            if !bytes.ends_with(b"\n") {
                if self.exact {
                    bail!("incomplete inherited record in {}", self.path.display());
                }
                eprintln!(
                    "rtr: omitted an unfinished trailing record in {}",
                    self.path.display()
                );
                break;
            }
            let record = serde_json::from_slice(&bytes).with_context(|| {
                format!("invalid conversation record in {}", self.path.display())
            })?;
            visit(record)?;
        }
        let current = std::fs::symlink_metadata(&self.path)?;
        if read != self.length
            || current.file_type().is_symlink()
            || (current.dev(), current.ino()) != self.inode
            || current.len() < self.length
        {
            bail!(
                "source changed while copying {}; retry the fork",
                self.path.display()
            );
        }
        Ok(())
    }
}

fn write_record(writer: &mut impl Write, record: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, record)?;
    writer.write_all(b"\n")?;
    Ok(())
}
