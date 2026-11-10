//! Background catalog/index production and query evaluation.
//!
//! Loader -> worker -> terminal is a one-way data flow; terminal requests travel
//! back to the worker. A refresh generation rejects old disk results, and a query
//! revision rejects old searches. Neither thread owns the terminal or launches
//! agents. Dropping Worker cancels them without delaying native child handoff.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::time::{Duration, Instant};

use super::{
    launch::Launch,
    search::{self, Anchor, Excerpt, Filters, Index, Query, Row, Search, Tab},
};
use crate::{
    config::Config,
    conversations::{self, Catalog, Conversation, ConversationKey},
    paths::Paths,
};

/// Worker-produced window and its logical landing point. The UI owns only the
/// scroll offset from this anchor, so paging can replace the window safely.
#[derive(Clone, Default)]
pub(super) struct Preview {
    pub key: Option<String>,
    pub excerpts: Vec<Excerpt>,
    pub matches: usize,
    pub match_index: usize,
    pub anchor: Option<Anchor>,
    pub match_note: Option<String>,
    pub launch: Launch,
}

#[derive(Clone, Default)]
/// One worker response, accepted only for the current refresh and query.
pub(super) struct Snapshot {
    pub generation: u64,
    pub revision: u64,
    pub preview_revision: u64,
    pub preview_only: bool,
    pub rows: Vec<Row>,
    pub selected: Option<String>,
    pub preview: Preview,
    pub indexed: usize,
    pub build_progress: Option<(usize, usize)>,
    pub total: usize,
    pub profiles: Vec<String>,
    pub diagnostics: Vec<String>,
    pub loaded: bool,
    pub error: Option<String>,
}

enum Request {
    Query(Query),
    Preview(Query),
    Progress(u64, usize, usize),
    Catalog(u64, Result<CatalogUpdate, String>),
    Painted(u64),
    Stop,
}

/// One catalog generation plus ready-to-search mapped dialogue. The optional
/// gate prevents reconciliation and preview I/O until the UI paints these rows.
struct CatalogUpdate {
    catalog: Catalog,
    config: Config,
    indexes: Option<HashMap<String, Index>>,
    directories: HashMap<PathBuf, PathBuf>,
    first_frame: Option<Sender<()>>,
}

impl CatalogUpdate {
    fn cached(
        view: crate::session_index::packed::View,
        config: Config,
        first_frame: Option<Sender<()>>,
    ) -> Self {
        let indexes = view
            .dialogues
            .into_iter()
            .map(|(c, d)| {
                (
                    ConversationKey::from(&c).encode(),
                    Index::from_dialogue(&c, d),
                )
            })
            .collect();
        Self {
            catalog: view.catalog,
            config,
            indexes: Some(indexes),
            directories: view.directories,
            first_frame,
        }
    }
}

pub(super) struct Worker {
    tx: Sender<Request>,
    pub updates: Receiver<Snapshot>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    revision: Arc<AtomicU64>,
    paths: Paths,
    include_non_conversations: bool,
}

impl Worker {
    pub fn new(
        paths: &Paths,
        cwd: PathBuf,
        extra: Vec<String>,
        include_non_conversations: bool,
    ) -> Self {
        let (tx, requests) = mpsc::channel();
        let (updates, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let revision = Arc::new(AtomicU64::new(0));
        let worker_stop = stop.clone();
        let worker_revision = revision.clone();
        let worker_generation = generation.clone();
        let worker_cwd = cwd.clone();
        std::thread::spawn(move || {
            let mut data = Data::default();
            let mut query = Query::default();
            let mut pending = false;
            let mut last_paint = Instant::now() - Duration::from_secs(1);
            while !worker_stop.load(Ordering::Relaxed) {
                let mut batch = match requests.recv_timeout(Duration::from_millis(50)) {
                    Ok(first) => vec![first],
                    Err(mpsc::RecvTimeoutError::Timeout) => Vec::new(),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                batch.extend(requests.try_iter());
                let mut interactive = false;
                for request in batch {
                    match request {
                        Request::Stop => return,
                        Request::Painted(gen) if gen == data.generation => {
                            if let Some(gate) = data.first_frame.take() {
                                let _ = gate.send(());
                                pending = true;
                                interactive = true;
                            }
                        }
                        Request::Painted(_) => {}
                        Request::Query(value) => {
                            query = value;
                            pending = true;
                            interactive = true;
                        }
                        Request::Preview(value)
                            if value.revision == worker_revision.load(Ordering::Relaxed)
                                && data.generation == worker_generation.load(Ordering::Relaxed) =>
                        {
                            query.selected = value.selected.clone();
                            query.tab = value.tab;
                            query.match_index = value.match_index;
                            query.preview_revision = value.preview_revision;
                            query.preview_messages = value.preview_messages;
                            query.preview_newer_messages = value.preview_newer_messages;
                            // Explicit retries obey the same paint gate as
                            // automatic previews. Preserve their latest intent;
                            // Painted will schedule it after the frame commits.
                            if data.first_frame.is_some() {
                                continue;
                            }
                            if let Some((key, conversation)) = data
                                .rows
                                .iter()
                                .find(|(key, _)| Some(key) == value.selected.as_ref())
                                .cloned()
                            {
                                let row = Row {
                                    key,
                                    conversation,
                                    highlights: Vec::new(),
                                };
                                let generation = data.generation;
                                let cancelled = || {
                                    worker_stop.load(Ordering::Relaxed)
                                        || worker_revision.load(Ordering::Relaxed) != value.revision
                                        || worker_generation.load(Ordering::Relaxed) != generation
                                };
                                let preview = data.preview(&row, &value, &extra, cancelled);
                                let _ = updates.send(Snapshot {
                                    generation: data.generation,
                                    revision: value.revision,
                                    preview_revision: value.preview_revision,
                                    preview_only: true,
                                    preview,
                                    ..Snapshot::default()
                                });
                            }
                        }
                        Request::Progress(gen, done, total) if gen == data.generation => {
                            data.progress =
                                Some((data.progress.map_or(done, |p| p.0.max(done)), total));
                            pending = true;
                        }
                        Request::Progress(..) | Request::Preview(..) => {}
                        Request::Catalog(gen, result)
                            if gen == worker_generation.load(Ordering::Relaxed) =>
                        {
                            if data.generation != gen {
                                data = Data {
                                    generation: gen,
                                    ..Data::default()
                                };
                            }
                            match result {
                                Ok(update) => {
                                    data.diagnostics = update.catalog.diagnostics;
                                    data.config = update.config;
                                    data.directories = update.directories;
                                    data.first_frame = update.first_frame;
                                    if let Some(indexes) = update.indexes {
                                        // Unreadable dialogue is a completed miss,
                                        // not an indexing task that runs forever.
                                        data.unavailable = update
                                            .catalog
                                            .conversations
                                            .iter()
                                            .map(|c| ConversationKey::from(c).encode())
                                            .filter(|key| !indexes.contains_key(key))
                                            .collect();
                                        data.indexed = update.catalog.conversations.len();
                                        data.progress = None;
                                        data.index = indexes;
                                        data.match_cache = None;
                                    }
                                    data.tails.clear();
                                    data.rows = update
                                        .catalog
                                        .conversations
                                        .into_iter()
                                        .map(|conversation| {
                                            let key = ConversationKey::from(&conversation).encode();
                                            (key, Arc::new(conversation))
                                        })
                                        .collect();
                                    data.loaded = true;
                                }
                                Err(error) => data.error = Some(error),
                            }
                            pending = true;
                            interactive = true;
                        }
                        Request::Catalog(..) => {}
                    }
                }
                // Batch index arrivals; keystrokes bypass this throttle. This
                // avoids rescanning the growing corpus once per tiny transcript.
                if pending && (interactive || last_paint.elapsed() >= Duration::from_millis(100)) {
                    let generation = data.generation;
                    let cancelled = || {
                        worker_stop.load(Ordering::Relaxed)
                            || worker_revision.load(Ordering::Relaxed) != query.revision
                            || worker_generation.load(Ordering::Relaxed) != generation
                    };
                    if let Some(snapshot) = data.snapshot(&query, &worker_cwd, cancelled) {
                        // Publish rows before finding positions or formatting dialogue.
                        // A slow selected transcript must not delay typed results.
                        query.selected = snapshot.selected.clone();
                        if updates.send(snapshot).is_err() {
                            break;
                        }
                        if let Some(row) = data
                            .selected_row(&query)
                            .filter(|_| data.first_frame.is_none())
                        {
                            let preview = data.preview(&row, &query, &extra, cancelled);
                            if !cancelled() {
                                let _ = updates.send(Snapshot {
                                    generation,
                                    revision: query.revision,
                                    preview_revision: query.preview_revision,
                                    preview_only: true,
                                    preview,
                                    ..Snapshot::default()
                                });
                            }
                        }
                    }
                    pending = false;
                    last_paint = Instant::now();
                }
            }
        });
        Self {
            tx,
            updates: rx,
            stop,
            generation,
            revision,
            paths: paths.clone(),
            include_non_conversations,
        }
    }

    pub fn search(&self, query: &Query) {
        self.revision.store(query.revision, Ordering::Relaxed);
        let _ = self.tx.send(Request::Query(query.clone()));
    }

    pub fn preview(&self, query: &Query) {
        let _ = self.tx.send(Request::Preview(query.clone()));
    }
    pub fn painted(&self, generation: u64) {
        let _ = self.tx.send(Request::Painted(generation));
    }
    pub fn refresh(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let current = self.generation.clone();
        let stop = self.stop.clone();
        let paths = self.paths.clone();
        let tx = self.tx.clone();
        let include_non_conversations = self.include_non_conversations;
        std::thread::spawn(move || {
            let cancelled =
                || stop.load(Ordering::Relaxed) || current.load(Ordering::Relaxed) != generation;
            let config = match Config::load(&paths.config_file()) {
                Ok(config) => config,
                Err(e) => {
                    let _ = tx.send(Request::Catalog(generation, Err(format!("{e:#}"))));
                    return;
                }
            };
            let cached = crate::session_index::packed::Packed::open(&paths);
            let (painted, first_frame) = mpsc::channel();
            if let Some(cached) = &cached {
                let view = cached.view(&config, include_non_conversations);
                let _ = tx.send(Request::Catalog(
                    generation,
                    Ok(CatalogUpdate::cached(view, config.clone(), Some(painted))),
                ));
                // This acknowledgement comes after Terminal::draw, not after
                // queueing rows. No source discovery or preview read may race
                // the first frame on an oversubscribed host.
                loop {
                    if cancelled() {
                        return;
                    }
                    match first_frame.recv_timeout(Duration::from_millis(20)) {
                        Ok(()) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(_) => return,
                    }
                }
            }
            let mut index =
                match crate::session_index::SessionIndex::open(&paths, &config, &cancelled) {
                    Ok(index) => index,
                    Err(e) => {
                        let _ = tx.send(Request::Catalog(generation, Err(format!("{e:#}"))));
                        return;
                    }
                };
            if cancelled() {
                return;
            }
            if cached.is_none() {
                let catalog = index.catalog(include_non_conversations);
                let directories = catalog
                    .conversations
                    .iter()
                    .map(|c| {
                        (
                            c.cwd.clone(),
                            c.cwd.canonicalize().unwrap_or_else(|_| c.cwd.clone()),
                        )
                    })
                    .collect();
                let _ = tx.send(Request::Catalog(
                    generation,
                    Ok(CatalogUpdate {
                        catalog,
                        config: config.clone(),
                        indexes: None,
                        directories,
                        first_frame: None,
                    }),
                ));
            }
            let progress = |done, total| {
                if cached.is_none() {
                    let _ = tx.send(Request::Progress(generation, done, total));
                }
            };
            let packed = index.packed(&paths, cached.as_ref(), &cancelled, &progress);
            if cancelled() {
                return;
            }
            let mut view = packed.view(&config, include_non_conversations);
            view.catalog.diagnostics = index.diagnostics;
            let _ = tx.send(Request::Catalog(
                generation,
                Ok(CatalogUpdate::cached(view, config, None)),
            ));
        });
        generation
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Request::Stop);
    }
}

#[derive(Default)]
/// Worker-owned catalog, dialogue index, and previews for one refresh generation.
struct Data {
    generation: u64,
    first_frame: Option<Sender<()>>,
    directories: HashMap<PathBuf, PathBuf>,
    rows: Vec<(String, Arc<Conversation>)>,
    index: HashMap<String, Index>,
    unavailable: HashSet<String>,
    // Only the selected conversation retains match positions. New selections or
    // queries replace this cache; it never scales with the result list.
    match_cache: Option<(String, String, Vec<usize>)>,
    // Bounded tail reads are cached separately so the selected session is
    // previewable even while a different, huge rollout is being indexed.
    tails: HashMap<String, Result<Vec<(String, String)>, String>>,
    config: Config,
    indexed: usize,
    progress: Option<(usize, usize)>,
    loaded: bool,
    diagnostics: Vec<String>,
    error: Option<String>,
}

impl Data {
    fn snapshot(
        &mut self,
        query: &Query,
        cwd: &Path,
        cancelled: impl Fn() -> bool + Sync,
    ) -> Option<Snapshot> {
        let empty = query.text.trim().is_empty();
        let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let directories: HashMap<_, _> = self
            .rows
            .iter()
            .map(|(_, c)| {
                let here = c.cwd == cwd
                    || self
                        .directories
                        .get(&c.cwd)
                        .is_some_and(|p| p == &canonical);
                (c.cwd.clone(), here)
            })
            .collect();
        let mut hits: Vec<_> = crate::session_index::parallel_map(
            &self.rows.chunks(128).collect::<Vec<_>>(),
            |part| {
                let search = Search::new(&query.text);
                part.iter()
                    .filter_map(|(key, conversation)| {
                        if cancelled() {
                            return None;
                        }
                        let here = directories[&conversation.cwd];
                        if !allows(&query.filters, conversation, here) {
                            return None;
                        }
                        let rank = if empty {
                            0
                        } else {
                            search.matches(conversation, self.index.get(key))?
                        };
                        Some((
                            (
                                std::cmp::Reverse(if empty { 0 } else { rank }),
                                !empty || !here,
                                std::cmp::Reverse(conversation.updated_at),
                                key.clone(),
                            ),
                            Row {
                                key: key.clone(),
                                conversation: conversation.clone(),
                                highlights: if empty {
                                    Vec::new()
                                } else {
                                    search.literal_indices(&search::clean(&conversation.title))
                                },
                            },
                        ))
                    })
                    .collect::<Vec<_>>()
            },
        )
        .into_iter()
        .flatten()
        .collect();
        if cancelled() {
            return None;
        }
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        let rows: Vec<_> = hits.into_iter().map(|(_, row)| row).collect();
        let selected = query
            .selected
            .as_ref()
            .and_then(|key| rows.iter().find(|row| row.key == *key))
            .or_else(|| rows.first());
        let selected_key = selected.map(|row| row.key.clone());

        let mut profiles: Vec<_> = self
            .rows
            .iter()
            .filter(|(_, c)| {
                query
                    .filters
                    .tool
                    .as_ref()
                    .is_none_or(|tool| c.tool == *tool)
            })
            .map(|(_, c)| c.profile.clone())
            .collect();
        profiles.sort();
        profiles.dedup();
        if cancelled() {
            return None;
        }
        Some(Snapshot {
            generation: self.generation,
            revision: query.revision,
            preview_revision: query.preview_revision,
            preview_only: false,
            selected: selected_key,
            preview: Preview::default(),
            rows,
            indexed: self.indexed,
            build_progress: self.progress,
            total: self.rows.len(),
            profiles,
            diagnostics: self.diagnostics.clone(),
            loaded: self.loaded,
            error: self.error.clone(),
        })
    }

    fn selected_row(&self, query: &Query) -> Option<Row> {
        self.rows
            .iter()
            .find(|(key, _)| Some(key) == query.selected.as_ref())
            .map(|(key, conversation)| Row {
                key: key.clone(),
                conversation: conversation.clone(),
                highlights: Vec::new(),
            })
    }

    fn preview(
        &mut self,
        row: &Row,
        query: &Query,
        extra: &[String],
        cancelled: impl Fn() -> bool,
    ) -> Preview {
        let conversation = &row.conversation;
        let launch = Launch::resolve(&self.config, conversation, extra);
        let mut preview = Preview {
            key: Some(row.key.clone()),
            launch,
            ..Preview::default()
        };
        match query.tab {
            Tab::Conversation => {
                if let Some(index) = self.index.get(&row.key) {
                    let search = Search::new(&query.text);
                    if self
                        .match_cache
                        .as_ref()
                        .is_none_or(|(key, text, _)| key != &row.key || text != &query.text)
                    {
                        let matches = index.matching_messages(&search, &cancelled);
                        if cancelled() {
                            return preview;
                        }
                        self.match_cache = Some((row.key.clone(), query.text.clone(), matches));
                    }
                    let matches = &self.match_cache.as_ref().unwrap().2;
                    preview.matches = matches.len();
                    preview.match_index = query.match_index.min(matches.len().saturating_sub(1));
                    let target = matches.get(preview.match_index).copied();
                    (preview.excerpts, preview.anchor) = index.conversation(
                        query.preview_messages.max(24),
                        query.preview_newer_messages,
                        target,
                        &search,
                    );
                    if !query.text.trim().is_empty() && matches.is_empty() {
                        let fields = index.metadata_matches(&search);
                        preview.match_note = Some(if fields.is_empty() {
                            "Matched by exclusion · showing latest messages".into()
                        } else {
                            format!("Matched in {} · showing latest messages", fields.join(", "))
                        });
                    }
                    if preview.excerpts.is_empty() {
                        preview.excerpts.push(note(
                            "No dialogue",
                            "No human messages were found in this transcript.",
                        ));
                    }
                    return preview;
                }
                if !query.text.trim().is_empty() {
                    preview.match_note = Some(if self.unavailable.contains(&row.key) {
                        "Transcript unavailable · showing recent messages".into()
                    } else {
                        "Finding matching messages…".into()
                    });
                }
                let messages = self.tails.entry(row.key.clone()).or_insert_with(|| {
                    conversations::preview_dialogue(conversation)
                        .map_err(|error| format!("{error:#}"))
                });
                preview.excerpts = match messages {
                    Ok(messages) if !messages.is_empty() => messages
                        .iter()
                        .map(|(role, text)| Excerpt {
                            label: search::role_label(role).into(),
                            text: text.clone(),
                            highlights: Vec::new(),
                        })
                        .collect(),
                    Ok(_) => vec![note(
                        "No recent dialogue",
                        "No human messages were found near the end of this transcript.",
                    )],
                    Err(error) => vec![note("Could not read conversation", error)],
                };
            }
            Tab::Details => {
                preview.excerpts = vec![
                    note("Conversation", &search::clean(&conversation.title)),
                    note("Session ID", &search::clean(&conversation.id)),
                    note(
                        "Agent / profile",
                        &format!("{}/{}", conversation.tool, conversation.profile),
                    ),
                    note(
                        "Working directory",
                        &search::clean(&conversation.cwd.display().to_string()),
                    ),
                    note(
                        "Requested model / effort",
                        &format!(
                            "{} / {}",
                            preview.launch.model.as_deref().unwrap_or("native default"),
                            preview.launch.effort.as_deref().unwrap_or("native default")
                        ),
                    ),
                    note("Native arguments", &preview.launch.arguments),
                    note(
                        "Transcript",
                        &search::clean(&conversation.transcript_path.display().to_string()),
                    ),
                ];
                if !conversation.enabled || conversation.bypass {
                    preview.excerpts.push(note("Profile policy",
                        "Resume uses this isolated home, including disabled or bypassed profiles. Fork selects the next enabled isolated profile unless --to-profile is supplied."));
                }
                if preview.launch.model.is_none() || preview.launch.effort.is_none() {
                    preview.excerpts.push(note("Native defaults",
                        "Unspecified settings are resolved by the native CLI. RTR does not guess layered configuration or saved-session settings."));
                }
                if !self.diagnostics.is_empty() {
                    preview.excerpts.push(note(
                        "Catalog notes",
                        &self
                            .diagnostics
                            .iter()
                            .map(|line| search::clean(line))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ));
                }
            }
        }
        preview
    }
}

fn note(label: &str, text: &str) -> Excerpt {
    Excerpt {
        label: label.into(),
        text: text.into(),
        highlights: Vec::new(),
    }
}

fn allows(filters: &Filters, conversation: &Conversation, here: bool) -> bool {
    (!filters.here || here)
        && filters
            .tool
            .as_ref()
            .is_none_or(|tool| conversation.tool == *tool)
        && filters
            .profile
            .as_ref()
            .is_none_or(|profile| conversation.profile == *profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_selectable_before_indexing_and_full_text_respects_filters() {
        let mut here = crate::picker::tests::conversation();
        here.id = "nearby".into();
        here.cwd = PathBuf::from("/nearby");
        let mut other = here.clone();
        other.id = "elsewhere".into();
        other.cwd = PathBuf::from("/elsewhere");
        let key = ConversationKey::from(&other).encode();
        let mut data = Data {
            loaded: true,
            rows: vec![
                (key.clone(), Arc::new(other.clone())),
                (ConversationKey::from(&here).encode(), Arc::new(here)),
            ],
            ..Data::default()
        };
        let mut query = Query::default();
        let initial = data
            .snapshot(&query, Path::new("/nearby"), || false)
            .unwrap();
        assert_eq!(initial.indexed, 0);
        assert_eq!(initial.rows[0].conversation.id, "nearby");

        query.text = "'old_needle".into();
        query.tab = Tab::Conversation;
        assert!(data
            .snapshot(&query, Path::new("/nearby"), || false)
            .unwrap()
            .rows
            .is_empty());
        data.index.insert(
            key.clone(),
            Index::new(
                &other,
                vec![(
                    "user".into(),
                    "An old_needle from the start of the session".into(),
                )],
            ),
        );
        let matched = data
            .snapshot(&query, Path::new("/nearby"), || false)
            .unwrap();
        assert_eq!(matched.selected.as_deref(), Some(key.as_str()));
        assert!(data
            .preview(&matched.rows[0], &query, &[], || false)
            .excerpts[0]
            .text
            .contains("old_needle"));
        query.text = "login !old_needle".into();
        assert!(!data
            .snapshot(&query, Path::new("/nearby"), || false)
            .unwrap()
            .rows
            .iter()
            .any(|row| row.conversation.id == "elsewhere"));
        query.text = "'old_needle".into();
        query.filters.here = true;
        assert!(data
            .snapshot(&query, Path::new("/nearby"), || false)
            .unwrap()
            .rows
            .is_empty());
        assert!(data
            .snapshot(&query, Path::new("/nearby"), || true)
            .is_none());
    }

    #[test]
    fn background_refresh_reindexes_changed_dialogue_and_retains_identity() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.config_file(),
            "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.nit]\n",
        )
        .unwrap();
        let transcript = paths
            .profile_home_dir("codex", "nit")
            .join("sessions/rollout-stable-id.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let write = |message: &str| {
            let records = [
                serde_json::json!({"type": "session_meta", "payload": {
                    "id": "stable-id", "cwd": temp.path(), "timestamp": "2026-09-07T12:00:00Z"
                }}),
                serde_json::json!({"type": "response_item", "payload": {
                    "type": "message", "role": "user", "content": [{"type": "input_text", "text": message}]
                }}),
            ];
            std::fs::write(
                &transcript,
                records
                    .iter()
                    .map(|record| record.to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n",
            )
            .unwrap();
        };
        write("old_needle");
        let worker = Worker::new(&paths, temp.path().to_path_buf(), Vec::new(), false);
        let first = worker.refresh();
        let mut query = Query {
            revision: 1,
            text: "'old_needle".into(),
            tab: Tab::Conversation,
            ..Query::default()
        };
        worker.search(&query);
        let wait = |generation, revision| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "background index did not complete"
                );
                if let Ok(snapshot) = worker.updates.recv_timeout(Duration::from_millis(50)) {
                    if snapshot.loaded {
                        worker.painted(snapshot.generation);
                    }
                    if snapshot.generation == generation
                        && snapshot.revision == revision
                        && snapshot.preview_only
                    {
                        break snapshot;
                    }
                }
            }
        };
        let before = wait(first, 1);
        query.selected = before.selected.clone();
        write("new_needle");
        let second = worker.refresh();
        query.revision += 1;
        query.text = "'new_needle".into();
        worker.search(&query);
        let after = wait(second, 2);
        assert_eq!(after.selected, before.selected);
        assert!(after.preview.excerpts[0].text.contains("new_needle"));
        assert!(!after.preview.excerpts[0].text.contains("old_needle"));
    }
}

#[cfg(test)]
mod ranking_tests {
    use super::*;
    #[test]
    fn title_then_metadata_then_dialogue_rank_before_recency_and_empty_prefers_here() {
        let base = crate::picker::tests::conversation();
        let mut data = Data {
            loaded: true,
            ..Data::default()
        };
        for (id, title, cwd, seconds) in [
            ("title-old", "Needle", "/other", 1),
            ("title-new", "Needle", "/other", 2),
            ("metadata", "Unrelated", "/needle", 3),
            ("dialogue", "Unrelated", "/here", 4),
        ] {
            let mut c = base.clone();
            c.id = id.into();
            c.title = title.into();
            c.cwd = PathBuf::from(cwd);
            c.updated_at = chrono::DateTime::from_timestamp(seconds, 0).unwrap();
            let key = ConversationKey::from(&c).encode();
            data.index.insert(
                key.clone(),
                Index::new(&c, vec![("user".into(), "A literal needle".into())]),
            );
            data.rows.push((key, Arc::new(c)));
        }
        let mut query = Query {
            text: "needle".into(),
            ..Query::default()
        };
        let found = data.snapshot(&query, Path::new("/here"), || false).unwrap();
        assert_eq!(
            found
                .rows
                .iter()
                .map(|r| r.conversation.id.as_str())
                .collect::<Vec<_>>(),
            ["title-new", "title-old", "metadata", "dialogue"]
        );
        query.text.clear();
        assert_eq!(
            data.snapshot(&query, Path::new("/here"), || false)
                .unwrap()
                .rows[0]
                .conversation
                .id,
            "dialogue"
        );
    }
    #[test]
    fn indexed_conversation_can_page_past_the_old_24_message_tail() {
        let c = crate::picker::tests::conversation();
        let key = ConversationKey::from(&c).encode();
        let index = Index::new(
            &c,
            (0..80)
                .map(|i| ("user".into(), format!("message {i}")))
                .collect(),
        );
        let mut data = Data::default();
        data.index.insert(key.clone(), index);
        let row = Row {
            key,
            conversation: Arc::new(c),
            highlights: Vec::new(),
        };
        let mut query = Query::default();
        assert_eq!(
            data.preview(&row, &query, &[], || false).excerpts[0].text,
            "message 56"
        );
        query.preview_messages = 96;
        assert_eq!(
            data.preview(&row, &query, &[], || false).excerpts[0].text,
            "message 0"
        );
    }
}

#[cfg(test)]
mod landing_tests {
    use super::*;
    use crate::picker::{view, App, Options};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{backend::TestBackend, Terminal};

    fn fixture(messages: Vec<(String, String)>) -> (Data, App) {
        let conversation = crate::picker::tests::conversation();
        let key = ConversationKey::from(&conversation).encode();
        let mut data = Data {
            generation: 1,
            loaded: true,
            indexed: 1,
            ..Data::default()
        };
        data.index
            .insert(key.clone(), Index::new(&conversation, messages));
        data.rows.push((key, Arc::new(conversation)));
        let mut app = App::new(
            Options {
                query: Some("needle".into()),
                tool: None,
                profile: None,
                here: false,
                include_non_conversations: false,
                mode: conversations::OpenMode::Fork,
                to_profile: None,
                extra_args: Vec::new(),
            },
            PathBuf::from("/projects/browseros"),
        );
        app.generation = 1;
        (data, app)
    }

    // Exercise the same two deliveries as the worker: rows first, then the
    // bounded selected preview. This also tests the UI's revision acceptance.
    fn settle(data: &mut Data, app: &mut App) {
        if !app.ready() || app.query.selected.is_none() {
            let snapshot = data.snapshot(&app.query, &app.cwd, || false).unwrap();
            app.apply(snapshot);
        }
        let row = data.selected_row(&app.query).unwrap();
        let preview = data.preview(&row, &app.query, &[], || false);
        app.apply(Snapshot {
            generation: app.generation,
            revision: app.query.revision,
            preview_revision: app.query.preview_revision,
            preview_only: true,
            preview,
            ..Snapshot::default()
        });
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal.draw(|frame| view::draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(140)
            .map(|row| {
                row.iter()
                    .map(|c| c.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn key(app: &mut App, ch: char) {
        app.key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL));
    }

    #[test]
    fn newest_landing_navigation_position_and_continuous_scroll() {
        let (mut data, mut app) = fixture(vec![
            ("user".into(), "old needle".into()),
            ("assistant".into(), "context between matches".into()),
            ("user".into(), "newest needle".into()),
            ("assistant".into(), "after the newest match".into()),
        ]);
        settle(&mut data, &mut app);
        let rendered = screen(&mut app);
        assert!(rendered.contains("Match 1/2 · newest first"));
        assert!(rendered.contains("context between matches"));
        assert!(
            rendered.lines().nth(33).unwrap().ends_with("newest needle"),
            "{rendered}"
        );
        assert!(!rendered.contains("after the newest match"));
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/picker-snapshots/140x36-search.txt");
        if std::env::var_os("RTR_UPDATE_PICKER_SNAPSHOTS").is_some() {
            std::fs::write(&path, &rendered).unwrap();
        }
        assert_eq!(rendered, std::fs::read_to_string(path).unwrap());
        let revision = app.query.revision;
        key(&mut app, 'n');
        settle(&mut data, &mut app);
        let rendered = screen(&mut app);
        assert!(rendered.contains("Match 2/2 · newest first"));
        assert!(rendered.contains("old needle"));
        assert!(!rendered.contains("newest needle"));
        key(&mut app, 'b');
        settle(&mut data, &mut app);
        assert!(screen(&mut app).contains("Match 1/2 · newest first"));
        key(&mut app, 'd');
        settle(&mut data, &mut app);
        assert!(screen(&mut app).contains("after the newest match"));
        assert_eq!(
            app.query.revision, revision,
            "preview navigation must not rerun search"
        );
        app.query.text.clear();
        app.cursor = 0;
        app.changed(true);
        settle(&mut data, &mut app);
        let rendered = screen(&mut app);
        assert!(!rendered.contains("Match 1/"));
        assert!(rendered
            .lines()
            .nth(33)
            .unwrap()
            .ends_with("after the newest match"));
    }

    #[test]
    fn selection_change_resets_older_match_and_scroll_to_newest_match() {
        let (mut data, mut app) = fixture(vec![
            ("user".into(), "first needle".into()),
            ("user".into(), "second needle".into()),
        ]);
        let mut other = (*data.rows[0].1).clone();
        other.id = "other".into();
        let other_key = ConversationKey::from(&other).encode();
        data.index.insert(
            other_key.clone(),
            Index::new(
                &other,
                vec![
                    ("user".into(), "other old needle".into()),
                    ("assistant".into(), "other newest needle".into()),
                ],
            ),
        );
        data.rows.push((other_key, Arc::new(other)));
        settle(&mut data, &mut app);
        let previous = app.query.selected.clone();
        key(&mut app, 'n');
        settle(&mut data, &mut app);
        key(&mut app, 'u');
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_ne!(app.query.selected, previous);
        assert_eq!(app.query.match_index, 0);
        assert_eq!(app.scroll, 0);
        settle(&mut data, &mut app);
        assert_eq!(app.snapshot.preview.match_index, 0);
        let anchor = app.snapshot.preview.anchor.as_ref().unwrap();
        assert!(
            app.snapshot.preview.excerpts[anchor.excerpt]
                .text
                .contains("second needle")
                || app.snapshot.preview.excerpts[anchor.excerpt]
                    .text
                    .contains("other newest needle")
        );
    }

    #[test]
    fn metadata_only_match_names_the_field_and_lands_at_tail() {
        for (term, field) in [
            ("login", "title"),
            ("browseros", "project"),
            ("opaque", "ID"),
            ("callback", "first prompt"),
        ] {
            let (mut data, mut app) = fixture(vec![("user".into(), "recent dialogue".into())]);
            app.query.text = term.into();
            app.cursor = app.query.text.len();
            settle(&mut data, &mut app);
            assert_eq!(app.snapshot.preview.matches, 0);
            assert!(app
                .snapshot
                .preview
                .match_note
                .as_ref()
                .unwrap()
                .contains(field));
            assert!(screen(&mut app)
                .lines()
                .nth(33)
                .unwrap()
                .ends_with("recent dialogue"));
        }
    }

    #[test]
    fn last_hit_in_long_message_stays_visible_with_unicode_highlights() {
        let text = format!(
            "first needle\n{}\n東京 👩‍💻 İSTANBUL ΟΣ last needle\n{}",
            "older filler\n".repeat(100),
            "later filler\n".repeat(100)
        );
        let (mut data, mut app) = fixture(vec![("user".into(), text)]);
        app.query.text = "needle i\u{307}stanbul ος".into();
        settle(&mut data, &mut app);
        let excerpt = &app.snapshot.preview.excerpts[0];
        let highlighted: String = excerpt
            .text
            .chars()
            .enumerate()
            .filter(|(i, _)| excerpt.highlights.contains(&(*i as u32)))
            .map(|(_, c)| c)
            .collect();
        assert_eq!(highlighted, "needleİSTANBULΟΣneedle");
        let rendered = screen(&mut app);
        assert!(rendered.contains("last needle"), "{rendered}");
        assert!(!rendered.contains("first needle"));
        assert!(!rendered.contains("later filler"));
        key(&mut app, 'd');
        settle(&mut data, &mut app);
        assert!(screen(&mut app).contains("later filler"));
    }

    #[test]
    fn paging_preserves_anchor_while_crossing_both_loaded_edges() {
        let messages = (0..200)
            .map(|i| {
                (
                    "user".into(),
                    if i == 100 {
                        "target needle".into()
                    } else {
                        format!("context {i:03}")
                    },
                )
            })
            .collect();
        let (mut data, mut app) = fixture(messages);
        settle(&mut data, &mut app);
        assert!(screen(&mut app)
            .lines()
            .nth(33)
            .unwrap()
            .ends_with("target needle"));
        for _ in 0..40 {
            key(&mut app, 'd');
            // A frame may render before the expanded window arrives.
            screen(&mut app);
            settle(&mut data, &mut app);
            screen(&mut app);
        }
        assert!(screen(&mut app).contains("context 199"));
        let at_end = app.scroll;
        key(&mut app, 'd');
        settle(&mut data, &mut app);
        screen(&mut app);
        assert_eq!(app.scroll, at_end, "the real end clamps excess scrolling");
        key(&mut app, 'u');
        settle(&mut data, &mut app);
        assert!(
            !screen(&mut app).contains("context 199"),
            "one reverse scroll must move immediately"
        );
        for _ in 0..65 {
            key(&mut app, 'u');
            screen(&mut app);
            settle(&mut data, &mut app);
            screen(&mut app);
        }
        assert!(screen(&mut app).contains("context 000"));
        key(&mut app, 'b');
        settle(&mut data, &mut app);
        assert!(screen(&mut app)
            .lines()
            .nth(33)
            .unwrap()
            .ends_with("target needle"));
    }

    #[test]
    fn long_unbroken_message_lands_on_its_last_hit_after_wrapping() {
        let (mut data, mut app) = fixture(vec![(
            "user".into(),
            format!(
                "early needle {} last needle{}",
                "界".repeat(1600),
                "x".repeat(2000)
            ),
        )]);
        settle(&mut data, &mut app);
        for (width, height) in [(140, 36), (80, 32), (44, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            let highlighted: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .filter(|c| c.fg == ratatui::style::Color::Yellow)
                .map(|c| c.symbol())
                .collect();
            assert_eq!(highlighted, "needle", "{width}: {text}");
            assert!(!text.contains("early needle"));
        }
    }

    #[test]
    fn terms_can_land_in_separate_messages_and_exclusion_has_no_message_target() {
        let (mut data, mut app) = fixture(vec![
            ("user".into(), "alpha".into()),
            ("assistant".into(), "beta".into()),
        ]);
        app.query.text = "alpha beta".into();
        settle(&mut data, &mut app);
        assert_eq!(app.snapshot.preview.matches, 2);
        assert!(screen(&mut app).lines().nth(33).unwrap().ends_with("beta"));
        app.query.text = "!absent".into();
        app.changed(true);
        settle(&mut data, &mut app);
        assert_eq!(app.snapshot.preview.matches, 0);
        assert!(app
            .snapshot
            .preview
            .match_note
            .as_ref()
            .unwrap()
            .contains("exclusion"));
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use crate::session_index::{packed::Packed, SessionIndex};

    #[test]
    fn cached_rows_are_searchable_before_the_ui_allows_reconciliation() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.config_file(),
            "[tools.claude]\ncommand=[\"false\"]\n[tools.claude.profiles.test]\n",
        )
        .unwrap();
        let path = paths
            .profile_home_dir("claude", "test")
            .join("projects/project/synthetic.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let record = |text: &str| {
            serde_json::json!({"type":"user", "sessionId":"synthetic", "cwd":temp.path(), "message":{"content":text}}).to_string()+"\n"
        };
        std::fs::write(&path, record("old cached dialogue")).unwrap();
        let config = Config::load(&paths.config_file()).unwrap();
        SessionIndex::open(&paths, &config, &|| false)
            .unwrap()
            .packed(&paths, None, &|| false, &|_, _| {});
        assert!(Packed::open(&paths).is_some());
        std::fs::write(&path, record("replacement dialogue")).unwrap();
        let worker = Worker::new(&paths, temp.path().to_path_buf(), Vec::new(), false);
        let generation = worker.refresh();
        let query = Query {
            text: "cached".into(),
            ..Query::default()
        };
        worker.search(&query);
        let snapshot = loop {
            let s = worker.updates.recv_timeout(Duration::from_secs(5)).unwrap();
            if s.loaded {
                break s;
            }
        };
        assert_eq!(snapshot.generation, generation);
        assert_eq!(
            snapshot.rows.len(),
            1,
            "cached dialogue must already be searchable"
        );
        assert_eq!(snapshot.indexed, 1);
        assert_eq!(snapshot.rows[0].conversation.title, "old cached dialogue");
        // No acknowledgement yet: changed disk content cannot have replaced the
        // cached frame, even though the source was replaced before Worker::new.
        assert_eq!(
            Packed::open(&paths)
                .unwrap()
                .view(&config, false)
                .catalog
                .conversations[0]
                .title,
            "old cached dialogue"
        );
        let preview_query = Query {
            selected: snapshot.selected.clone(),
            preview_revision: 1,
            tab: Tab::Details,
            ..query.clone()
        };
        worker.preview(&preview_query);
        assert!(
            matches!(
                worker.updates.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "explicit preview bypassed the paint gate"
        );
        worker.painted(generation);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                Instant::now() < deadline,
                "reconciliation did not complete after paint"
            );
            let s = worker.updates.recv_timeout(Duration::from_secs(1)).unwrap();
            if !s.preview_only && s.loaded && s.total == 1 && s.rows.is_empty() {
                break;
            }
        }
        assert_eq!(
            Packed::open(&paths)
                .unwrap()
                .view(&config, false)
                .catalog
                .conversations[0]
                .title,
            "replacement dialogue"
        );
    }
}

#[cfg(test)]
mod unavailable_tests {
    use super::*;
    #[test]
    fn completed_dialogue_miss_is_unavailable_instead_of_pending() {
        let conversation = Arc::new(crate::picker::tests::conversation());
        let key = ConversationKey::from(conversation.as_ref()).encode();
        let mut data = Data::default();
        data.unavailable.insert(key.clone());
        let row = Row {
            key,
            conversation,
            highlights: Vec::new(),
        };
        let query = Query {
            text: "login".into(),
            ..Query::default()
        };
        let preview = data.preview(&row, &query, &[], || false);
        assert_eq!(
            preview.match_note.as_deref(),
            Some("Transcript unavailable · showing recent messages")
        );
    }
}
