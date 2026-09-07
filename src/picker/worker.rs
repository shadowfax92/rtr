//! Background catalog/index production and query evaluation.
//!
//! Loader -> worker -> terminal is a one-way data flow; terminal requests travel
//! back to the worker. A refresh generation rejects old disk results, and a query
//! revision rejects old searches. Neither thread owns the terminal or launches
//! agents. Dropping Worker cancels them without delaying native child handoff.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::time::{Duration, Instant};

use super::{
    launch::Launch,
    search::{self, Excerpt, Filters, Index, Query, Row, Search, Tab},
};
use crate::{
    config::Config,
    conversations::{self, Catalog, Conversation, ConversationKey, ConversationQuery},
    paths::Paths,
};

#[derive(Clone, Default)]
pub(super) struct Preview {
    pub key: Option<String>,
    pub excerpts: Vec<Excerpt>,
    pub matches: usize,
    pub match_index: usize,
    pub launch: Launch,
}

#[derive(Clone, Default)]
/// One worker response, accepted only for the current refresh and query.
pub(super) struct Snapshot {
    pub generation: u64,
    pub revision: u64,
    pub rows: Vec<Row>,
    pub selected: Option<String>,
    pub preview: Preview,
    pub indexed: usize,
    pub total: usize,
    pub profiles: Vec<String>,
    pub diagnostics: Vec<String>,
    pub loaded: bool,
    pub error: Option<String>,
}

enum Request {
    Query(Query),
    Catalog(u64, Result<(Catalog, Config), String>),
    Indexed(u64, String, Result<Index, String>),
    Stop,
}

pub(super) struct Worker {
    tx: Sender<Request>,
    pub updates: Receiver<Snapshot>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    revision: Arc<AtomicU64>,
    paths: Paths,
    cwd: PathBuf,
}

impl Worker {
    pub fn new(paths: &Paths, cwd: PathBuf, extra: Vec<String>) -> Self {
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
                        Request::Query(value) => {
                            query = value;
                            pending = true;
                            interactive = true;
                        }
                        Request::Catalog(gen, result)
                            if gen == worker_generation.load(Ordering::Relaxed) =>
                        {
                            data = Data {
                                generation: gen,
                                ..Data::default()
                            };
                            match result {
                                Ok((catalog, config)) => {
                                    data.diagnostics = catalog.diagnostics;
                                    data.config = config;
                                    data.rows = catalog
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
                        Request::Indexed(gen, key, result) if gen == data.generation => {
                            data.indexed += 1;
                            match result {
                                Ok(index) => {
                                    data.index.insert(key, index);
                                }
                                Err(error) => {
                                    data.index_errors.insert(key, error.clone());
                                    data.diagnostics.push(error);
                                }
                            }
                            pending = true;
                        }
                        Request::Indexed(..) | Request::Catalog(..) => {}
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
                    if let Some(snapshot) = data.snapshot(&query, &worker_cwd, &extra, cancelled) {
                        // Preserve identity across index arrivals. Query changes reset it
                        // deliberately, while navigation supplies the selected stable key.
                        query.selected = snapshot.selected.clone();
                        if updates.send(snapshot).is_err() {
                            break;
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
            cwd,
        }
    }

    pub fn search(&self, query: &Query) {
        self.revision.store(query.revision, Ordering::Relaxed);
        let _ = self.tx.send(Request::Query(query.clone()));
    }

    pub fn refresh(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let current = self.generation.clone();
        let stop = self.stop.clone();
        let paths = self.paths.clone();
        let cwd = self.cwd.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let cancelled =
                || stop.load(Ordering::Relaxed) || current.load(Ordering::Relaxed) != generation;
            let result = (|| -> anyhow::Result<_> {
                let config = Config::load(&paths.config_file())?;
                let mut catalog = conversations::query_cancellable(
                    &paths,
                    &ConversationQuery::all(),
                    &cancelled,
                )?;
                // Index nearby/recent sessions first; ranking still belongs to search.
                catalog
                    .conversations
                    .sort_by_key(|c| !same_directory(&c.cwd, &cwd));
                Ok((catalog, config))
            })();
            if cancelled() {
                return;
            }
            let (catalog, config) = match result {
                Ok(value) => value,
                Err(error) => {
                    let _ = tx.send(Request::Catalog(generation, Err(format!("{error:#}"))));
                    return;
                }
            };
            let conversations = catalog.conversations.clone();
            if tx
                .send(Request::Catalog(generation, Ok((catalog, config))))
                .is_err()
            {
                return;
            }
            for conversation in conversations {
                if cancelled() {
                    break;
                }
                let key = ConversationKey::from(&conversation).encode();
                let index = conversations::read_dialogue(&conversation, cancelled)
                    .map(|messages| Index::new(&conversation, messages))
                    .map_err(|error| {
                        format!("{}/{}: {error:#}", conversation.tool, conversation.id)
                    });
                if cancelled() {
                    break;
                }
                if tx.send(Request::Indexed(generation, key, index)).is_err() {
                    break;
                }
            }
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
    rows: Vec<(String, Arc<Conversation>)>,
    index: HashMap<String, Index>,
    index_errors: HashMap<String, String>,
    match_cache: HashMap<(String, String), Vec<Excerpt>>,
    // Bounded tail reads are cached separately so the selected session is
    // previewable even while a different, huge rollout is being indexed.
    tails: HashMap<String, Result<Vec<(String, String)>, String>>,
    config: Config,
    indexed: usize,
    loaded: bool,
    diagnostics: Vec<String>,
    error: Option<String>,
}

impl Data {
    fn snapshot(
        &mut self,
        query: &Query,
        cwd: &Path,
        extra: &[String],
        cancelled: impl Fn() -> bool,
    ) -> Option<Snapshot> {
        let mut search = Search::new(&query.text);
        let empty = query.text.trim().is_empty();
        let mut hits = Vec::new();
        for (position, (key, conversation)) in self.rows.iter().enumerate() {
            if cancelled() {
                return None;
            }
            let here = same_directory(&conversation.cwd, cwd);
            if !allows(&query.filters, conversation, here) {
                continue;
            }
            let metadata_score = if empty {
                Some(0)
            } else {
                search.score(&search::metadata(conversation))
            };
            // Once dialogue is available, it defines membership for the whole
            // query, including exclusions. A title hit may improve ranking but
            // must not bypass a negative term found in an older message.
            // Empty queries need only directory/recency ordering. Do not walk
            // the entire corpus again when navigating the initial catalog.
            let score = match self.index.get(key).filter(|_| !empty) {
                Some(index) => search
                    .score(&index.text)
                    .map(|score| metadata_score.unwrap_or(score)),
                None => metadata_score,
            };
            if let Some(score) = score {
                hits.push((
                    search::rank(score, metadata_score.is_some(), here, position, empty),
                    Row {
                        key: key.clone(),
                        conversation: conversation.clone(),
                        highlights: search.indices(&search::clean(&conversation.title)),
                    },
                ));
            }
        }
        hits.sort_by_key(|(rank, _)| *rank);
        let rows: Vec<_> = hits.into_iter().map(|(_, row)| row).collect();
        let selected = query
            .selected
            .as_ref()
            .and_then(|key| rows.iter().find(|row| row.key == *key))
            .or_else(|| rows.first());
        let selected_key = selected.map(|row| row.key.clone());
        let preview = selected
            .map(|row| self.preview(row, query, extra))
            .unwrap_or_default();
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
            selected: selected_key,
            preview,
            rows,
            indexed: self.indexed,
            total: self.rows.len(),
            profiles,
            diagnostics: self.diagnostics.clone(),
            loaded: self.loaded,
            error: self.error.clone(),
        })
    }

    fn preview(&mut self, row: &Row, query: &Query, extra: &[String]) -> Preview {
        let conversation = &row.conversation;
        let launch = Launch::resolve(&self.config, conversation, extra);
        let mut preview = Preview {
            key: Some(row.key.clone()),
            launch,
            ..Preview::default()
        };
        match query.tab {
            Tab::Conversation => {
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
            Tab::Matches if query.text.trim().is_empty() => {
                preview.excerpts = vec![note(
                    "Search this conversation",
                    "Type a query to see the passages it matches.",
                )];
            }
            Tab::Matches => {
                if let Some(index) = self.index.get(&row.key) {
                    // Keep navigation warm without retaining excerpts for every
                    // intermediate query the user has typed during a long browse.
                    if self.match_cache.len() >= 64 {
                        self.match_cache.clear();
                    }
                    let matches = self
                        .match_cache
                        .entry((row.key.clone(), query.text.clone()))
                        .or_insert_with(|| index.excerpts(&query.text));
                    preview.matches = matches.len();
                    if !matches.is_empty() {
                        preview.match_index = query.match_index.min(matches.len() - 1);
                        preview.excerpts = vec![matches[preview.match_index].clone()];
                    } else {
                        preview.excerpts = vec![note(
                            "No positive highlights",
                            "This query matches by exclusion or session metadata.",
                        )];
                    }
                } else if let Some(error) = self.index_errors.get(&row.key) {
                    preview.excerpts = vec![note("Could not index transcript", error)];
                } else {
                    preview.excerpts = vec![note("Indexing transcript", "Session metadata is searchable now. Matching passages appear when indexing finishes.")];
                }
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

pub(super) fn same_directory(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
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
            .snapshot(&query, Path::new("/nearby"), &[], || false)
            .unwrap();
        assert_eq!(initial.indexed, 0);
        assert_eq!(initial.rows[0].conversation.id, "nearby");

        query.text = "'old_needle".into();
        query.tab = Tab::Matches;
        assert!(data
            .snapshot(&query, Path::new("/nearby"), &[], || false)
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
            .snapshot(&query, Path::new("/nearby"), &[], || false)
            .unwrap();
        assert_eq!(matched.selected.as_deref(), Some(key.as_str()));
        assert!(matched.preview.excerpts[0].text.contains("old_needle"));
        query.text = "login !old_needle".into();
        assert!(!data
            .snapshot(&query, Path::new("/nearby"), &[], || false)
            .unwrap()
            .rows
            .iter()
            .any(|row| row.conversation.id == "elsewhere"));
        query.text = "'old_needle".into();
        query.filters.here = true;
        assert!(data
            .snapshot(&query, Path::new("/nearby"), &[], || false)
            .unwrap()
            .rows
            .is_empty());
        assert!(data
            .snapshot(&query, Path::new("/nearby"), &[], || true)
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
            .join("sessions/rollout.jsonl");
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
        let worker = Worker::new(&paths, temp.path().to_path_buf(), Vec::new());
        let first = worker.refresh();
        let mut query = Query {
            revision: 1,
            text: "'old_needle".into(),
            tab: Tab::Matches,
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
                    if snapshot.generation == generation
                        && snapshot.revision == revision
                        && snapshot.indexed == 1
                        && snapshot.rows.len() == 1
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
