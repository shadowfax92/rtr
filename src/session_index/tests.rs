//! Synthetic-only cache lifecycle checks. Native profile data never enters fixtures.
use super::*;
use serde_json::json;
use std::{fs, io::Write, os::unix::fs::PermissionsExt};

fn fixture() -> (tempfile::TempDir, Paths, Config, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: temp.path().join("config"),
        state_dir: temp.path().join("state"),
    };
    fs::create_dir_all(&paths.config_dir).unwrap();
    fs::write(
        paths.config_file(),
        "[tools.claude]\ncommand=[\"claude\"]\n[tools.claude.profiles.test]\n",
    )
    .unwrap();
    let path = paths
        .profile_home_dir("claude", "test")
        .join("projects/project/synthetic.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, line("original")).unwrap();
    let config = Config::load(&paths.config_file()).unwrap();
    (temp, paths, config, path)
}
fn line(text: &str) -> String {
    json!({"type":"user","sessionId":"synthetic","cwd":"/synthetic/project","timestamp":"2026-01-01T00:00:00Z","message":{"content":text}}).to_string()+"\n"
}
fn build(paths: &Paths, config: &Config) -> (SessionIndex, HashMap<PathBuf, Arc<parse::Record>>) {
    let mut index = SessionIndex::open(paths, config, &|| false).unwrap();
    let records = index.build(&|| false, &|_, _| {});
    (index, records)
}
#[test]
fn reuse_append_and_same_size_rewrite_obey_source_identity() {
    let (_temp, paths, config, path) = fixture();
    let (_, first) = build(&paths, &config);
    assert_eq!(
        first[&path].parsed_bytes,
        fs::metadata(&path).unwrap().len()
    );
    let store = Store::new(&paths).unwrap();
    let written = fs::metadata(store.shard(&path))
        .unwrap()
        .modified()
        .unwrap();
    let (_, warm) = build(&paths, &config);
    assert_eq!(warm[&path].parsed_bytes, 0);
    assert_eq!(
        fs::metadata(store.shard(&path))
            .unwrap()
            .modified()
            .unwrap(),
        written
    );
    let append = line("appended");
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(append.as_bytes())
        .unwrap();
    let (_, grown) = build(&paths, &config);
    assert_eq!(grown[&path].parsed_bytes, append.len() as u64);
    assert_eq!(grown[&path].messages.len(), 2);
    fs::write(&path, line("replaced") + &line("appended")).unwrap();
    let (_, rewritten) = build(&paths, &config);
    assert_eq!(rewritten[&path].messages[0].text, "replaced");
    assert_eq!(
        rewritten[&path].parsed_bytes,
        fs::metadata(&path).unwrap().len()
    );
}
#[test]
fn replacement_truncation_and_deletion_remove_old_dialogue() {
    let (_temp, paths, config, path) = fixture();
    let (_, first) = build(&paths, &config);
    let replacement = path.with_extension("new");
    fs::write(&replacement, line("new inode") + &line("second")).unwrap();
    fs::rename(replacement, &path).unwrap();
    let (_, replaced) = build(&paths, &config);
    assert_ne!(first[&path].stamp.inode, replaced[&path].stamp.inode);
    assert_eq!(replaced[&path].messages[0].text, "new inode");
    fs::write(&path, line("short")).unwrap();
    let (_, truncated) = build(&paths, &config);
    assert_eq!(truncated[&path].messages.len(), 1);
    assert_eq!(truncated[&path].messages[0].text, "short");
    fs::remove_file(&path).unwrap();
    let (deleted, _) = build(&paths, &config);
    assert!(deleted.catalog(true).conversations.is_empty());
    let store = Store::new(&paths).unwrap();
    assert!(!store.summaries().contains_key(&path));
    assert!(!store.shard(&path).exists());
}
#[test]
fn corrupt_and_versioned_cache_entries_rebuild_privately() {
    let (_temp, paths, config, path) = fixture();
    build(&paths, &config);
    let store = Store::new(&paths).unwrap();
    fs::write(store.shard(&path), b"{broken").unwrap();
    let (_, rebuilt) = build(&paths, &config);
    assert!(rebuilt[&path].parsed_bytes > 0);
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(store.shard(&path)).unwrap()).unwrap();
    value["version"] = json!(999);
    fs::write(store.shard(&path), value.to_string()).unwrap();
    fs::write(
        paths.state_dir.join("session-index/catalog.json"),
        "{\"version\":999,\"entries\":{}}",
    )
    .unwrap();
    let (rebuilt, records) = build(&paths, &config);
    assert_eq!(rebuilt.catalog(true).conversations.len(), 1);
    assert!(records[&path].parsed_bytes > 0);
    let root = paths.state_dir.join("session-index");
    for dir in [&root, &root.join("shards")] {
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for file in [root.join("catalog.json"), store.shard(&path)] {
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    fs::write(root.join("catalog.json"), "invalid").unwrap();
    assert_eq!(
        build(&paths, &config).0.catalog(true).conversations.len(),
        1
    );
}
#[test]
fn partial_records_and_cancelled_builds_resume_without_losing_messages() {
    let (_temp, paths, config, path) = fixture();
    let append = line("finished later");
    let cut = append.len() / 2;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&append.as_bytes()[..cut])
        .unwrap();
    let (_, partial) = build(&paths, &config);
    assert!(!partial[&path].complete);
    assert_eq!(partial[&path].messages.len(), 1);
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&append.as_bytes()[cut..])
        .unwrap();
    let (_, finished) = build(&paths, &config);
    assert_eq!(finished[&path].messages.len(), 2);
    assert_eq!(finished[&path].parsed_bytes, append.len() as u64);
    fs::write(&path, line("checkpoint") + &line("after checkpoint")).unwrap();
    let index = SessionIndex::open(&paths, &config, &|| false).unwrap();
    let calls = AtomicUsize::new(0);
    let cancelled = || calls.fetch_add(1, Ordering::Relaxed) > 0;
    let record = parse::load(&index.sources[0], index.store.as_ref(), &cancelled).unwrap();
    assert!(!record.complete);
    assert_eq!(record.messages.len(), 1);
    let (_, resumed) = build(&paths, &config);
    assert_eq!(resumed[&path].messages.len(), 2);
    assert_eq!(
        resumed[&path].parsed_bytes,
        line("after checkpoint").len() as u64
    );
}
#[test]
fn unwriteable_cache_is_a_miss_not_a_catalog_failure() {
    let (_temp, paths, config, path) = fixture();
    fs::write(
        paths.state_dir.join("session-index"),
        "blocks cache directory",
    )
    .unwrap();
    let (index, records) = build(&paths, &config);
    assert_eq!(index.catalog(true).conversations.len(), 1);
    assert_eq!(records[&path].messages[0].text, "original");
    assert!(index
        .diagnostics
        .iter()
        .any(|d| d.contains("cache unavailable")));
}
#[test]
fn raw_gate_handles_spaces_escapes_and_does_not_index_tool_payloads() {
    let (_temp, paths, config, path) = fixture();
    let records = [
        json!({"type":"user","sessionId":"synthetic","cwd":"/synthetic/project","message":{"content":"human needle"}}),
        json!({"type":"assistant","message":{"content":[{"type":"tool_use","input":{"text":"hidden tool input"}},{"type":"text","text":"human answer"}]}}),
        json!({"type":"user","message":{"content":[{"type":"tool_result","content":"hidden result"}]}}),
        json!({"type":"api-request-blob","request":{"text":"hidden request"}}),
    ];
    fs::write(
        &path,
        records
            .iter()
            .map(|v| serde_json::to_string(v).unwrap() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    let (_, indexed) = build(&paths, &config);
    assert_eq!(
        indexed[&path]
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["human needle", "human answer"]
    );
    assert!(parse::dialogue_candidate(
        br#"{"type": "\u0075ser", "message":{"content":"escaped"}}"#,
        "claude"
    ));
}

#[test]
fn a_failed_shard_write_keeps_dialogue_available_in_memory() {
    let (_temp, paths, config, path) = fixture();
    let store = Store::new(&paths).unwrap();
    fs::create_dir(store.shard(&path)).unwrap();
    let (index, records) = build(&paths, &config);
    assert_eq!(records[&path].messages[0].text, "original");
    assert_eq!(index.catalog(true).conversations.len(), 1);
    assert!(index
        .diagnostics
        .iter()
        .any(|d| d.contains("saving transcript cache")));
}

#[test]
fn deleted_dialogue_is_pruned_even_when_its_manifest_was_lost() {
    for mode in ["missing", "corrupt", "version"] {
        let (_temp, paths, config, path) = fixture();
        build(&paths, &config);
        let store = Store::new(&paths).unwrap();
        let shard = store.shard(&path);
        assert!(shard.exists());
        fs::remove_file(&path).unwrap();
        let manifest = paths.state_dir.join("session-index/catalog.json");
        match mode {
            "missing" => fs::remove_file(&manifest).unwrap(),
            "corrupt" => fs::write(&manifest, "{corrupt").unwrap(),
            _ => fs::write(&manifest, r#"{"version":999,"entries":{}}"#).unwrap(),
        }
        let (index, _) = build(&paths, &config);
        assert!(index.catalog(true).conversations.is_empty());
        assert!(!shard.exists(), "deleted dialogue survived {mode} manifest");
    }
}

#[test]
fn orphan_checkpoint_is_retained_while_its_source_can_resume() {
    let (_temp, paths, config, path) = fixture();
    build(&paths, &config);
    let store = Store::new(&paths).unwrap();
    let shard = store.shard(&path);
    fs::remove_file(paths.state_dir.join("session-index/catalog.json")).unwrap();
    store.prune_orphans().unwrap();
    assert!(shard.exists());
    assert_eq!(build(&paths, &config).1[&path].parsed_bytes, 0);
}
