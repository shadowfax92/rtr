//! RTR-owned transcript projections shared by lists and the interactive picker.
//!
//! Opening reads a small catalog and stats sources; it never loads dialogue on
//! the terminal thread. Physical files are indexed in parallel, then Codex live
//! lineages compose their cached prefixes. Native homes are always read-only.
pub(crate) mod parse;
mod store;
#[cfg(test)]
mod tests;
use crate::{
    config::Config,
    conversations::{self, Catalog, Conversation},
    paths::Paths,
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use store::{Stamp, Store, Summary};

/// Discovery-time identity and current profile policy, kept outside cached state.
#[derive(Clone)]
struct Source {
    path: PathBuf,
    stamp: Stamp,
    tool: String,
    profile: String,
    enabled: bool,
    bypass: bool,
    live: bool,
    prompts: Arc<HashMap<String, String>>,
    names: Arc<HashMap<String, String>>,
}
/// One disk generation. Metadata is available immediately; complete dialogue
/// arrives through build's callback without holding cache-publication locks.
pub(crate) struct SessionIndex {
    store: Option<Store>,
    sources: Vec<Source>,
    summaries: BTreeMap<PathBuf, Summary>,
    pub diagnostics: Vec<String>,
}
impl SessionIndex {
    pub fn open(
        paths: &Paths,
        config: &Config,
        cancelled: &(impl Fn() -> bool + Sync),
    ) -> Result<Self> {
        let (store, mut diagnostics) = match Store::new(paths) {
            Ok(s) => (Some(s), Vec::new()),
            Err(e) => (None, vec![format!("session cache unavailable: {e:#}")]),
        };
        let summaries = store.as_ref().map(Store::summaries).unwrap_or_default();
        let mut sources = Vec::new();
        for (tool_name, tool) in &config.tools {
            if !matches!(tool_name.as_str(), "claude" | "codex") {
                continue;
            }
            for (profile_name, profile) in &tool.profiles {
                if cancelled() {
                    break;
                }
                let home = paths.profile_home_dir(tool_name, profile_name);
                let discovery = (|| -> Result<_> {
                    let (files, prompts, names) = if tool_name == "codex" {
                        (
                            conversations::codex_transcript_files(&home)?,
                            conversations::codex_prompts(
                                &home.join("history.jsonl"),
                                &mut diagnostics,
                                &cancelled,
                            )?,
                            conversations::codex_names(
                                &home.join("session_index.jsonl"),
                                &mut diagnostics,
                                &cancelled,
                            )?,
                        )
                    } else {
                        let mut files = Vec::new();
                        for dir in conversations::directories_if_exists(&home.join("projects"))? {
                            files.extend(conversations::jsonl_files(&dir)?);
                        }
                        (files, HashMap::new(), HashMap::new())
                    };
                    let live = crate::codex_history::live_files(&files);
                    let prompts = Arc::new(prompts);
                    let names = Arc::new(names);
                    for path in files {
                        match Stamp::read(&path) {
                            Ok(stamp) => sources.push(Source {
                                live: tool_name == "claude"
                                    || crate::codex_history::file_ids(&path)
                                        .and_then(|(id, _)| live.get(id))
                                        .is_some_and(|p| p == &path),
                                path,
                                stamp,
                                tool: tool_name.clone(),
                                profile: profile_name.clone(),
                                enabled: profile.enabled,
                                bypass: profile.bypass,
                                prompts: prompts.clone(),
                                names: names.clone(),
                            }),
                            Err(e) => diagnostics.push(format!("{}: {e}", path.display())),
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = discovery {
                    diagnostics.push(format!("could not scan {tool_name}/{profile_name}: {e:#}"));
                }
            }
        }
        let mut index = Self {
            store,
            sources,
            summaries,
            diagnostics,
        };
        // Cold/new files get a bounded metadata projection so they are selectable
        // while the complete corpus is building. Warm files use catalog summaries
        // even if a live writer has appended since the last completed projection.
        for source in &index.sources {
            if cancelled() {
                break;
            }
            if index
                .summaries
                .get(&source.path)
                .is_none_or(|s| !s.stamp.same_file(&source.stamp))
                && source.live
            {
                match quick_summary(source) {
                    Ok(summary) => {
                        index.summaries.insert(source.path.clone(), summary);
                    }
                    Err(e) => index
                        .diagnostics
                        .push(format!("{}: {e:#}", source.path.display())),
                }
            }
        }
        Ok(index)
    }
    pub fn catalog(&self, include_hidden: bool) -> Catalog {
        let mut rows = Vec::new();
        for source in &self.sources {
            if !source.live {
                continue;
            }
            let Some(summary) = self.summaries.get(&source.path) else {
                continue;
            };
            if summary.hidden && !include_hidden {
                continue;
            }
            if let Some(mut c) = summary.conversation.clone() {
                c.enabled = source.enabled;
                c.bypass = source.bypass;
                if source.tool == "codex" {
                    c.first_prompt = source.prompts.get(&c.id).cloned();
                    c.native_name = source.names.get(&c.id).cloned();
                    c.title = c
                        .native_name
                        .as_deref()
                        .map(|s| conversations::display_text(s, 160))
                        .filter(|s| !s.is_empty())
                        .or_else(|| c.first_prompt.clone())
                        .unwrap_or_else(|| c.id.clone());
                }
                rows.push(c);
            }
        }
        rows.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| (&a.tool, &a.profile, &a.id).cmp(&(&b.tool, &b.profile, &b.id)))
        });
        Catalog {
            conversations: rows,
            diagnostics: self.diagnostics.clone(),
        }
    }
    /// Progress counts physical files, including hidden threads and retired
    /// segments. Each completed shard survives cancellation independently.
    pub fn build(
        &mut self,
        cancelled: &(impl Fn() -> bool + Sync),
        progress: &(impl Fn(usize, usize) + Sync),
    ) -> HashMap<PathBuf, Arc<parse::Record>> {
        let completed = AtomicUsize::new(0);
        let updates = Mutex::new((BTreeMap::new(), Instant::now(), 0usize));
        let errors = Mutex::new(Vec::new());
        let records = parallel_map(&self.sources, |source| {
            if cancelled() {
                return None;
            }
            let result = parse::load(source, self.store.as_ref(), cancelled);
            let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
            match result {
                Ok(record) => {
                    if let Some(e) = &record.cache_error {
                        errors.lock().unwrap().push(e.clone());
                    }
                    if let Ok(summary) = record.summary(source) {
                        let mut updates = updates.lock().unwrap();
                        if self.summaries.get(&source.path).is_some_and(|s| {
                            s.complete
                                && s.stamp == summary.stamp
                                && s.conversation == summary.conversation
                        }) {
                            progress(count, self.sources.len());
                            return Some((source.path.clone(), Arc::new(record)));
                        }
                        updates.0.insert(source.path.clone(), summary);
                        updates.2 += 1;
                        if updates.2 >= 32 || updates.1.elapsed() >= Duration::from_millis(200) {
                            if let Some(store) = &self.store {
                                if let Err(e) = store.merge(&updates.0) {
                                    errors
                                        .lock()
                                        .unwrap()
                                        .push(format!("saving session catalog: {e:#}"));
                                }
                            }
                            updates.1 = Instant::now();
                            updates.2 = 0;
                        }
                    }
                    progress(count, self.sources.len());
                    Some((source.path.clone(), Arc::new(record)))
                }
                Err(e) => {
                    errors
                        .lock()
                        .unwrap()
                        .push(format!("{}: {e:#}", source.path.display()));
                    progress(count, self.sources.len());
                    None
                }
            }
        });
        let (updates, _, _) = updates.into_inner().unwrap();
        if let Some(store) = &self.store {
            if let Err(e) = store.merge(&updates) {
                errors
                    .lock()
                    .unwrap()
                    .push(format!("saving session catalog: {e:#}"));
            }
            if let Err(e) = store.prune_orphans() {
                errors
                    .lock()
                    .unwrap()
                    .push(format!("pruning session cache: {e:#}"));
            }
        }
        self.summaries.extend(updates);
        self.diagnostics.extend(errors.into_inner().unwrap());
        records.into_iter().flatten().collect()
    }
    pub fn dialogue(
        &self,
        conversation: &Conversation,
        records: &HashMap<PathBuf, Arc<parse::Record>>,
    ) -> Result<Vec<(String, String)>> {
        let fragments = if conversation.tool == "codex" {
            crate::codex_history::lineage(
                crate::codex_history::home_for(&conversation.transcript_path)?,
                &conversation.transcript_path,
            )?
            .into_iter()
            .map(|f| (f.path, f.limit.map(|(bytes, _)| bytes)))
            .collect()
        } else {
            vec![(conversation.transcript_path.clone(), None)]
        };
        let mut messages = Vec::new();
        for (path, limit) in fragments {
            let record = records.get(&path).ok_or_else(|| {
                anyhow::anyhow!("transcript projection unavailable: {}", path.display())
            })?;
            for message in &record.messages {
                if limit.is_some_and(|end| message.end > end) {
                    break;
                }
                if messages
                    .last()
                    .is_none_or(|(role, text)| role != &message.role || text != &message.text)
                {
                    messages.push((message.role.clone(), message.text.clone()));
                }
            }
        }
        Ok(messages)
    }
}
fn quick_summary(source: &Source) -> Result<Summary> {
    if source.tool == "codex" {
        let header = crate::codex_history::read_header(&source.path)?;
        let hidden = header.is_some_and(|h| {
            h["payload"]["source"] == "exec" || h["payload"]["source"].get("subagent").is_some()
        });
        return Ok(Summary {
            stamp: source.stamp.clone(),
            hidden,
            complete: false,
            conversation: conversations::codex_conversation(
                &source.path,
                &source.profile,
                source.enabled,
                source.bypass,
                &source.prompts,
                &source.names,
                true,
            )?,
        });
    }
    use std::io::{BufRead, BufReader, Read};
    let file = std::fs::File::open(&source.path)?;
    let mut metadata = conversations::ClaudeMetadata::default();
    for line in BufReader::new(file.take(256 * 1024))
        .lines()
        .map_while(Result::ok)
    {
        if let Ok(value) = serde_json::from_str(&line) {
            metadata.observe(&value);
        }
    }
    Ok(Summary {
        stamp: source.stamp.clone(),
        hidden: false,
        complete: false,
        conversation: metadata.conversation(
            &source.path,
            &source.profile,
            source.enabled,
            source.bypass,
        )?,
    })
}

pub(crate) fn catalog(
    paths: &Paths,
    include_hidden: bool,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<Catalog> {
    let config = Config::load(&paths.config_file())?;
    let mut index = SessionIndex::open(paths, &config, &cancelled)?;
    // Lists need fresh summaries, not cached dialogue. Avoid loading every shard
    // when the catalog stamp already matches its source.
    let sources = index.sources.clone();
    index.sources.retain(|s| {
        index
            .summaries
            .get(&s.path)
            .is_none_or(|v| !v.complete || v.stamp != s.stamp)
    });
    index.build(&cancelled, &|_, _| {});
    index.sources = sources;
    Ok(index.catalog(include_hidden))
}

/// Bounded fan-out shared by disk projection and query evaluation. A fixed cap
/// leaves cores available to native agents; jobs check their generation/revision.
pub(crate) fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .min(8)
        .min(items.len().max(1));
    let chunk = items.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let f = &f;
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(f).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("session worker panicked"))
            .collect()
    })
}
