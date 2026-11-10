//! Synthetic-only cache lifecycle checks. Native profile data never enters fixtures.
use super::*;
use serde_json::json;
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path};

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

fn codex_metadata(id: &str, cwd: &str, source: serde_json::Value, time: &str) -> serde_json::Value {
    json!({"type":"session_meta","timestamp":time,"payload":{"id":id,"cwd":cwd,"source":source,"timestamp":time}})
}
fn codex_fixture() -> (tempfile::TempDir, Paths, Config, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: temp.path().join("config"),
        state_dir: temp.path().join("state"),
    };
    fs::create_dir_all(&paths.config_dir).unwrap();
    fs::write(
        paths.config_file(),
        "[tools.codex]\ncommand=[\"false\"]\n[tools.codex.profiles.test]\n",
    )
    .unwrap();
    let sessions = paths.profile_home_dir("codex", "test").join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let config = Config::load(&paths.config_file()).unwrap();
    (temp, paths, config, sessions)
}
fn write_records(path: &std::path::Path, records: &[serde_json::Value]) {
    fs::write(
        path,
        records
            .iter()
            .map(|r| r.to_string() + "\n")
            .collect::<String>(),
    )
    .unwrap();
}
#[test]
fn replayed_codex_headers_cannot_duplicate_parents_or_hide_children() {
    let (_temp, paths, config, sessions) = codex_fixture();
    let parent = codex_metadata("parent", "/parent", json!("cli"), "2026-01-01T00:00:00Z");
    let hidden = codex_metadata(
        "hidden-parent",
        "/hidden-parent",
        json!({"subagent":{}}),
        "2026-01-02T00:00:00Z",
    );
    write_records(
        &sessions.join("rollout-parent.jsonl"),
        std::slice::from_ref(&parent),
    );
    write_records(
        &sessions.join("rollout-hidden-parent.jsonl"),
        std::slice::from_ref(&hidden),
    );
    for (id, source, replay) in [
        ("child", json!("cli"), parent.clone()),
        ("child-of-hidden", json!("cli"), hidden),
        ("exec-child", json!("exec"), parent),
    ] {
        write_records(
            &sessions.join(format!("rollout-{id}.jsonl")),
            &[
                codex_metadata(id, "/child", source, "2026-02-01T00:00:00Z"),
                replay,
                json!({"type":"response_item","timestamp":"2026-02-02T00:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Inherited content followed by child dialogue"}]}}),
            ],
        );
    }
    for _ in 0..2 {
        let (index, _) = build(&paths, &config);
        let rows = index.catalog(false).conversations;
        let ids: std::collections::BTreeSet<_> = rows.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            ids,
            std::collections::BTreeSet::from(["parent", "child", "child-of-hidden"])
        );
        for c in rows.iter().filter(|c| c.id != "parent") {
            assert_eq!(c.cwd, PathBuf::from("/child"));
            assert_eq!(
                c.started_at.unwrap().to_rfc3339(),
                "2026-02-01T00:00:00+00:00"
            );
        }
        assert_eq!(index.catalog(true).conversations.len(), 5);
    }
}
#[test]
fn append_checkpoint_retains_the_first_codex_owner_and_rebuilds_version_two() {
    let (_temp, paths, config, sessions) = codex_fixture();
    let path = sessions.join("rollout-child.jsonl");
    write_records(
        &path,
        &[codex_metadata(
            "child",
            "/child",
            json!("cli"),
            "2026-02-01T00:00:00Z",
        )],
    );
    build(&paths, &config);
    let replay = codex_metadata("parent", "/parent", json!("exec"), "2026-01-01T00:00:00Z")
        .to_string()
        + "\n";
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(replay.as_bytes())
        .unwrap();
    let (index, records) = build(&paths, &config);
    assert_eq!(records[&path].parsed_bytes, replay.len() as u64);
    assert_eq!(index.catalog(false).conversations[0].id, "child");
    let store = Store::new(&paths).unwrap();
    let mut cached: serde_json::Value =
        serde_json::from_slice(&fs::read(store.shard(&path)).unwrap()).unwrap();
    cached["version"] = json!(2);
    cached["metadata"]["id"] = json!("parent");
    cached["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("header_seen");
    fs::write(store.shard(&path), cached.to_string()).unwrap();
    let manifest = paths.state_dir.join("session-index/catalog.json");
    let mut old: serde_json::Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    old["version"] = json!(2);
    for entry in old["entries"].as_object_mut().unwrap().values_mut() {
        entry["conversation"]["id"] = json!("parent");
    }
    fs::write(manifest, old.to_string()).unwrap();
    let (fixed, reparsed) = build(&paths, &config);
    assert_eq!(fixed.catalog(false).conversations[0].id, "child");
    assert_eq!(
        reparsed[&path].parsed_bytes,
        fs::metadata(path).unwrap().len()
    );
}

fn write_prompt_records(path: &Path, records: &[serde_json::Value]) {
    fs::write(
        path,
        records
            .iter()
            .map(|r| r.to_string() + "\n")
            .collect::<String>(),
    )
    .unwrap();
}

fn human_record(text: &str, minute: u8) -> serde_json::Value {
    json!({"type":"user","sessionId":"synthetic","cwd":"/synthetic/project",
        "timestamp":format!("2026-01-01T00:{minute:02}:00Z"),"message":{"content":text}})
}

fn queued_record(prompt: serde_json::Value, minute: u8) -> serde_json::Value {
    json!({"type":"attachment","sessionId":"synthetic","cwd":"/synthetic/project",
        "timestamp":"2026-01-01T00:09:00Z","renderedRole":"system",
        "attachment":{"type":"queued_command","prompt":prompt,"commandMode":"prompt",
        "origin":{"kind":"human"},"humanTurn":true,"timestamp":format!("2026-01-01T00:{minute:02}:00Z")}})
}

#[test]
fn queued_human_inputs_sort_by_send_time_and_all_readers_agree() {
    let (_temp, paths, config, path) = fixture();
    let mut notification = queued_record(json!("machine notification"), 3);
    notification["attachment"]["origin"]["kind"] = json!("task-notification");
    let mut hook = queued_record(json!("hook output"), 4);
    hook["attachment"]["type"] = json!("hook_success");
    let mut untrusted = queued_record(json!("origin missing"), 4);
    untrusted["attachment"]
        .as_object_mut()
        .unwrap()
        .remove("origin");
    write_prompt_records(
        &path,
        &[
            human_record("first human prompt", 0),
            human_record("later human prompt", 2),
            queued_record(
                json!([{"type":"text","text":"queued human prompt"}, {"type":"image","source":{"data":"not-searchable"}}]),
                1,
            ),
            notification,
            hook,
            untrusted,
            json!({"type":"queue-operation","operation":"enqueue","content":"queue bookkeeping"}),
            json!({"type":"queue-operation","operation":"remove","content":"queue bookkeeping"}),
        ],
    );
    let (index, records) = build(&paths, &config);
    let conversation = &index.catalog(false).conversations[0];
    let expected = vec![
        ("user".into(), "first human prompt".into()),
        ("user".into(), "queued human prompt".into()),
        ("user".into(), "later human prompt".into()),
    ];
    assert_eq!(index.dialogue(conversation, &records).unwrap(), expected);
    assert_eq!(
        conversations::read_dialogue(conversation, || false).unwrap(),
        expected
    );
    assert_eq!(
        conversations::preview_dialogue(conversation).unwrap(),
        expected
    );
    assert!(
        records[&path]
            .messages
            .windows(2)
            .all(|m| m[0].end < m[1].end),
        "physical endpoints cannot be sorted for presentation"
    );
    let (warm, cached) = build(&paths, &config);
    assert_eq!(cached[&path].parsed_bytes, 0);
    assert_eq!(warm.dialogue(conversation, &cached).unwrap(), expected);
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(
            (queued_record(json!("late consumed but earlier sent"), 1).to_string() + "\n")
                .as_bytes(),
        )
        .unwrap();
    let (appended, cached) = build(&paths, &config);
    let messages = appended.dialogue(conversation, &cached).unwrap();
    assert_eq!(messages[2].1, "late consumed but earlier sent");
    assert_eq!(messages[3].1, "later human prompt");
}

#[test]
fn slash_prompts_are_readable_and_local_output_cannot_become_first_prompt() {
    let (_temp, paths, config, path) = fixture();
    let command = "<command-message>deploy</command-message>\n<command-name>/deploy</command-name>\n<command-args>staging --check</command-args>";
    write_prompt_records(
        &path,
        &[
            human_record(
                "<local-command-stdout>machine-only</local-command-stdout>",
                0,
            ),
            human_record(
                "<local-command-stderr>machine-error</local-command-stderr>",
                0,
            ),
            human_record(
                "<local-command-caveat>machine-caveat</local-command-caveat>",
                0,
            ),
            human_record(command, 1),
            queued_record(json!(command), 2),
        ],
    );
    let (index, records) = build(&paths, &config);
    let conversation = &index.catalog(false).conversations[0];
    assert_eq!(
        conversation.first_prompt.as_deref(),
        Some("/deploy staging --check")
    );
    assert_eq!(conversation.title, "/deploy staging --check");
    let expected = vec![("user".into(), "/deploy staging --check".into()); 2];
    assert_eq!(index.dialogue(conversation, &records).unwrap(), expected);
    assert_eq!(
        conversations::preview_dialogue(conversation).unwrap(),
        expected
    );
    assert_eq!(
        conversations::read_dialogue(conversation, || false).unwrap(),
        expected
    );
    assert_eq!(
        conversations::searchable_transcript_text(conversation).unwrap(),
        "/deploy staging --check /deploy staging --check"
    );
}

#[test]
fn version_three_shards_rebuild_to_include_preexisting_queued_prompts() {
    let (_temp, paths, config, path) = fixture();
    write_prompt_records(&path, &[queued_record(json!("already on disk"), 1)]);
    build(&paths, &config);
    let store = Store::new(&paths).unwrap();
    let mut shard: serde_json::Value =
        serde_json::from_slice(&fs::read(store.shard(&path)).unwrap()).unwrap();
    shard["version"] = json!(3);
    shard["messages"] = json!([]);
    fs::write(store.shard(&path), shard.to_string()).unwrap();
    let (_, rebuilt) = build(&paths, &config);
    assert_eq!(rebuilt[&path].version, 4);
    assert!(rebuilt[&path].parsed_bytes > 0);
    assert_eq!(rebuilt[&path].messages[0].text, "already on disk");
}
