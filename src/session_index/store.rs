//! Private, disposable cache storage. Shards commit before their small catalog
//! summaries; a crash can lose a summary update, but the shard is independently
//! discoverable by path on the next run. Locks cover publication, never parsing.
use super::parse::Record;
use crate::{
    conversations::Conversation,
    file_lock,
    paths::{self, Paths},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

// Version 5 adds the mapped picker snapshot. Physical checkpoints rebuild once
// so the old corpus shards are removed as part of the version transition.
pub(super) const VERSION: u32 = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Stamp {
    pub size: u64,
    pub seconds: i64,
    pub nanos: i64,
    pub inode: u64,
    pub device: u64,
}
impl Stamp {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        Self::from_metadata(&fs::metadata(path)?)
    }
    pub fn from_metadata(m: &fs::Metadata) -> std::io::Result<Self> {
        Ok(Self {
            size: m.len(),
            seconds: m.mtime(),
            nanos: m.mtime_nsec(),
            inode: m.ino(),
            device: m.dev(),
        })
    }
    pub fn same_file(&self, other: &Self) -> bool {
        self.inode == other.inode && self.device == other.device
    }
}

/// Metadata-only view that makes rows available before dialogue shards load.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Summary {
    pub stamp: Stamp,
    pub conversation: Option<Conversation>,
    pub hidden: bool,
    pub complete: bool,
}
#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    entries: BTreeMap<PathBuf, Summary>,
}

#[derive(Clone)]
pub(super) struct Store {
    root: PathBuf,
}
impl Store {
    pub fn new(paths: &Paths) -> Result<Self> {
        let root = paths.state_dir.join("session-index");
        paths::ensure_private_dir(&root)?;
        paths::ensure_private_dir(&root.join("shards"))?;
        let store = Self { root };
        let catalog = store.root.join("catalog.json");
        file_lock::with_exclusive_lock(&file_lock::lock_path(&catalog), || {
            let old_version = fs::read(&catalog)
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .and_then(|v| v["version"].as_u64());
            if old_version.is_some_and(|v| v != VERSION as u64) {
                // Establish the new version while holding the shared catalog
                // lock. Concurrent new builders migrate exactly once; old JSON
                // dialogue is removed rather than stranded after invalidation.
                for entry in fs::read_dir(store.root.join("shards"))? {
                    let path = entry?.path();
                    if path.extension().is_some_and(|e| e == "json") && path.is_file() {
                        fs::remove_file(path)?;
                    }
                }
                file_lock::write_private_atomic(
                    &catalog,
                    &serde_json::to_string(&Manifest {
                        version: VERSION,
                        ..Manifest::default()
                    })?,
                )?;
                let _ = fs::remove_file(store.root.join("picker-v5.bin"));
            }
            Ok(())
        })?;
        Ok(store)
    }
    pub fn summaries(&self) -> BTreeMap<PathBuf, Summary> {
        self.read_manifest().entries
    }
    fn read_manifest(&self) -> Manifest {
        fs::read(self.root.join("catalog.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
            .filter(|m| m.version == VERSION)
            .unwrap_or_default()
    }
    pub fn shard(&self, path: &Path) -> PathBuf {
        // A stable filename avoids an extra directory index. Verify the full
        // source path inside every shard, so hash collisions are cache misses.
        let hash = fingerprint(path.as_os_str().as_bytes());
        self.root.join("shards").join(format!("{hash:016x}.json"))
    }
    pub fn read(&self, path: &Path) -> Option<Record> {
        fs::read(self.shard(path))
            .ok()
            .and_then(|b| serde_json::from_slice::<Record>(&b).ok())
            .filter(|r| {
                r.version == VERSION
                    && r.path == path
                    && r.offset <= r.stamp.size
                    && r.messages.iter().all(|m| m.end <= r.offset)
            })
    }
    pub fn save(&self, record: &Record) -> Result<()> {
        let shard = self.shard(&record.path);
        file_lock::with_exclusive_lock(&file_lock::lock_path(&shard), || {
            let current = Stamp::read(&record.path)?;
            // An old parser must not overwrite a replacement file or a newer
            // pane's checkpoint. Appends after our snapshot remain reusable.
            if !current.same_file(&record.stamp)
                || current.size < record.stamp.size
                || (current.size == record.stamp.size && current != record.stamp)
            {
                return Ok(());
            }
            if self.read(&record.path).is_some_and(|r| {
                r.stamp.same_file(&record.stamp)
                    && (r.stamp.size > record.stamp.size
                        || (r.stamp == record.stamp && r.offset > record.offset))
            }) {
                return Ok(());
            }
            file_lock::write_private_atomic(&shard, &serde_json::to_string(record)?)
        })
    }
    pub fn merge(&self, updates: &BTreeMap<PathBuf, Summary>) -> Result<()> {
        let target = self.root.join("catalog.json");
        file_lock::with_exclusive_lock(&file_lock::lock_path(&target), || {
            let mut manifest = self.read_manifest();
            manifest.version = VERSION;
            for (path, summary) in updates {
                let Ok(stamp) = Stamp::read(path) else {
                    continue;
                };
                if stamp.same_file(&summary.stamp)
                    && stamp.size >= summary.stamp.size
                    && (stamp.size != summary.stamp.size || stamp == summary.stamp)
                    && !manifest.entries.get(path).is_some_and(|s| {
                        s.stamp.same_file(&summary.stamp) && s.stamp.size > summary.stamp.size
                    })
                {
                    manifest.entries.insert(path.clone(), summary.clone());
                }
            }
            manifest.entries.retain(|path, _| {
                if path.exists() {
                    true
                } else {
                    let _ = fs::remove_file(self.shard(path));
                    false
                }
            });
            file_lock::write_private_atomic(&target, &serde_json::to_string(&manifest)?)
        })
    }

    /// A checkpoint can outlive its catalog entry after an interrupted publish
    /// or a corrupt/version-incompatible manifest. Recover ownership from only
    /// unreferenced shards, preserving resumable checkpoints whose source exists.
    pub fn prune_orphans(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct Owner {
            path: PathBuf,
        }
        let referenced: HashSet<_> = self
            .read_manifest()
            .entries
            .keys()
            .map(|path| self.shard(path))
            .collect();
        for entry in fs::read_dir(self.root.join("shards"))? {
            let entry = entry?;
            let path = entry.path();
            // Never collect another writer's staging file or lock inode.
            if path.extension().is_none_or(|ext| ext != "json")
                || !entry.file_type()?.is_file()
                || referenced.contains(&path)
            {
                continue;
            }
            file_lock::with_exclusive_lock(&file_lock::lock_path(&path), || {
                // Re-read after acquiring the publication lock: another pane
                // may have completed or replaced this checkpoint while we waited.
                let owner = fs::File::open(&path)
                    .ok()
                    .and_then(|file| serde_json::from_reader::<_, Owner>(file).ok());
                let retain = owner.is_some_and(|owner| match fs::metadata(owner.path) {
                    Ok(_) => true,
                    Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                });
                if !retain {
                    match fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}

/// Cache-integrity fingerprint, not an authentication primitive.
pub(super) fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ *byte as u64).wrapping_mul(0x100000001b3)
    })
}
