//! Atomic picker snapshot: a small JSON catalog followed by byte-addressable
//! dialogue blocks. One rename commits metadata and offsets together; readers
//! keep the old mapped inode while other processes publish a new generation.
use super::{
    dialogue::Dialogue,
    store::{Stamp, VERSION},
    SessionIndex,
};
use crate::{
    config::Config,
    conversations::{Catalog, Conversation},
    file_lock,
    paths::{self, Paths},
};
use anyhow::{ensure, Result};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const MAGIC: &[u8; 8] = b"RTRPK005";
const NAME: &str = "picker-v5.bin";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Entry {
    pub conversation: Conversation,
    pub hidden: bool,
    pub dependencies: Vec<(PathBuf, Stamp)>,
    pub canonical_dir: PathBuf,
    offset: usize,
    length: usize,
    ready: bool,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    created: i64,
    entries: Vec<Entry>,
}

/// A validated catalog and cheap views into its immutable mapped dialogue.
/// No transcript discovery or corpus decoding occurs when opening this cache.
pub(crate) struct Packed {
    manifest: Manifest,
    dialogues: Vec<Dialogue>,
    by_path: HashMap<PathBuf, usize>,
    damaged: AtomicBool,
}

pub(crate) struct View {
    pub catalog: Catalog,
    pub dialogues: Vec<(Conversation, Dialogue)>,
    pub directories: HashMap<PathBuf, PathBuf>,
}

impl Packed {
    pub fn open(paths: &Paths) -> Option<Self> {
        Self::read(paths).ok()
    }
    fn read(paths: &Paths) -> Result<Self> {
        let file = File::open(paths.state_dir.join("session-index").join(NAME))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "snapshot is not private"
        );
        // Safety: RTR never mutates a published inode. Writers sync a private
        // staging file and rename it, and pruning only unlinks old pathnames.
        // Arc keeps this mapping alive for every dialogue view after replacement.
        let bytes = Arc::new(unsafe { Mmap::map(&file)? });
        ensure!(bytes.get(..8) == Some(MAGIC.as_slice()), "snapshot magic");
        let length = usize::try_from(u64::from_le_bytes(
            bytes
                .get(8..16)
                .ok_or_else(|| anyhow::anyhow!("snapshot header"))?
                .try_into()?,
        ))?;
        let start = 16usize
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("snapshot overflow"))?;
        let manifest: Manifest = serde_json::from_slice(
            bytes
                .get(16..start)
                .ok_or_else(|| anyhow::anyhow!("snapshot catalog"))?,
        )?;
        ensure!(manifest.version == VERSION, "snapshot version");
        let dialogues = manifest
            .entries
            .iter()
            .map(|entry| {
                let begin = start
                    .checked_add(entry.offset)
                    .ok_or_else(|| anyhow::anyhow!("snapshot offset"))?;
                let end = begin
                    .checked_add(entry.length)
                    .ok_or_else(|| anyhow::anyhow!("snapshot length"))?;
                Dialogue::mapped(bytes.clone(), begin..end)
            })
            .collect::<Result<Vec<_>>>()?;
        let by_path = manifest
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.conversation.transcript_path.clone(), i))
            .collect();
        Ok(Self {
            manifest,
            dialogues,
            by_path,
            damaged: AtomicBool::new(false),
        })
    }

    pub fn view(&self, config: &Config, include_hidden: bool) -> View {
        let mut catalog = Catalog {
            conversations: Vec::new(),
            diagnostics: Vec::new(),
        };
        let mut dialogues = Vec::new();
        let mut directories = HashMap::new();
        for (entry, dialogue) in self.manifest.entries.iter().zip(&self.dialogues) {
            let c = &entry.conversation;
            let Some(profile) = config
                .tools
                .get(&c.tool)
                .and_then(|t| t.profiles.get(&c.profile))
            else {
                continue;
            };
            if entry.hidden && !include_hidden {
                continue;
            }
            let mut c = c.clone();
            c.enabled = profile.enabled;
            c.bypass = profile.bypass;
            directories.insert(c.cwd.clone(), entry.canonical_dir.clone());
            if entry.ready {
                dialogues.push((c.clone(), dialogue.clone()));
            }
            catalog.conversations.push(c);
        }
        View {
            catalog,
            dialogues,
            directories,
        }
    }

    /// Reuse unchanged *composed* dialogue, including inherited prefix stamps.
    /// A changed ancestor invalidates its children even when their own file did
    /// not grow. Only those affected conversations load physical checkpoints.
    pub(super) fn reconcile(
        &self,
        index: &SessionIndex,
        c: &Conversation,
        stamps: &HashMap<&PathBuf, &Stamp>,
    ) -> Option<(Entry, Dialogue)> {
        let i = *self.by_path.get(&c.transcript_path)?;
        let e = &self.manifest.entries[i];
        if !e.ready
            || e.conversation != *c
            || !e.dependencies.iter().all(|(path, stamp)| {
                index
                    .summaries
                    .get(path)
                    .is_some_and(|s| s.complete && s.stamp == *stamp)
                    && stamps.get(path).is_some_and(|s| **s == *stamp)
            })
        {
            return None;
        }
        if !self.dialogues[i].valid() {
            // Repair must be committed even when the rebuilt metadata/offsets
            // happen to match. Otherwise every new picker remaps the same bad
            // bytes and repeats an in-memory-only repair forever.
            self.damaged.store(true, Ordering::Relaxed);
            return None;
        }
        Some((e.clone(), self.dialogues[i].clone()))
    }

    pub(super) fn prepare(
        index: &SessionIndex,
        c: Conversation,
        messages: Vec<(String, String)>,
        dependencies: Vec<(PathBuf, Stamp)>,
        canonical_dir: PathBuf,
    ) -> (Entry, Dialogue) {
        let hidden = index
            .summaries
            .get(&c.transcript_path)
            .is_some_and(|s| s.hidden);
        (
            Entry {
                conversation: c,
                hidden,
                dependencies,
                canonical_dir,
                offset: 0,
                length: 0,
                ready: true,
            },
            Dialogue::new(messages),
        )
    }

    pub(super) fn unavailable(
        index: &SessionIndex,
        c: Conversation,
        canonical_dir: PathBuf,
    ) -> (Entry, Dialogue) {
        let (mut entry, dialogue) = Self::prepare(index, c, Vec::new(), Vec::new(), canonical_dir);
        entry.ready = false;
        (entry, dialogue)
    }

    pub(super) fn unchanged(&self, rows: &[(Entry, Dialogue)]) -> bool {
        !self.damaged.load(Ordering::Relaxed)
            && self.manifest.entries.len() == rows.len()
            && self.manifest.entries.iter().zip(rows).all(|(a, (b, _))| {
                a.conversation == b.conversation
                    && a.hidden == b.hidden
                    && a.ready == b.ready
                    && a.dependencies == b.dependencies
                    && a.canonical_dir == b.canonical_dir
            })
    }

    /// Publication checks refresh start order under one short
    /// lock. Slow/older refreshes cannot replace a newer pane's snapshot. Text
    /// copying and fsync happen outside the lock and never on the terminal.
    pub(super) fn publish(paths: &Paths, rows: &[(Entry, Dialogue)], created: i64) -> Result<()> {
        let root = paths.state_dir.join("session-index");
        paths::ensure_private_dir(&root)?;
        let mut offset = 0;
        let entries = rows
            .iter()
            .map(|(e, d)| {
                let mut e = e.clone();
                e.offset = offset;
                e.length = d.encoded().len();
                offset += e.length;
                e
            })
            .collect();
        let manifest = Manifest {
            version: VERSION,
            created,
            entries,
        };
        let header = serde_json::to_vec(&manifest)?;
        let mut temp = tempfile::NamedTempFile::new_in(&root)?;
        temp.write_all(MAGIC)?;
        temp.write_all(&(header.len() as u64).to_le_bytes())?;
        temp.write_all(&header)?;
        for (_, d) in rows {
            temp.write_all(d.encoded())?;
        }
        temp.as_file().sync_all()?;
        let target = root.join(NAME);
        file_lock::with_exclusive_lock(&file_lock::lock_path(&target), || {
            if Self::open(paths).is_some_and(|p| p.manifest.created > created) {
                return Ok(());
            }
            // Native writers may append, replace, or delete sources while this
            // immutable snapshot is being prepared. It remains a valid cached
            // view of what we observed; rejecting it for one busy transcript
            // would prevent the entire corpus from ever becoming warm. The
            // next reconciliation compares every dependency stamp instead.
            temp.persist(&target)?;
            File::open(root)?.sync_all()?;
            Ok(())
        })
    }

    pub(super) fn from_rows(rows: Vec<(Entry, Dialogue)>, created: i64) -> Self {
        let (entries, dialogues) = rows.into_iter().unzip();
        let entries: Vec<Entry> = entries;
        let by_path = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.conversation.transcript_path.clone(), i))
            .collect();
        Self {
            manifest: Manifest {
                version: VERSION,
                created,
                entries,
            },
            dialogues,
            by_path,
            damaged: AtomicBool::new(false),
        }
    }
}
