use super::*;
use crate::conversations::{locate_transcript, query, ConversationKey, ConversationQuery};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    _temp: tempfile::TempDir,
    paths: Paths,
    home: PathBuf,
    target: PathBuf,
    conversation: Conversation,
}

impl Fixture {
    fn new(tool: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
        };
        let home = paths.ensure_profile_home_dir(tool, "source").unwrap();
        let target = paths.ensure_profile_home_dir(tool, "target").unwrap();
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.config_file(), format!("[tools.{tool}]\ncommand=['unused']\ncopy=[]\n[tools.{tool}.profiles.source]\n[tools.{tool}.profiles.target]\n")).unwrap();
        let now = chrono::Utc::now();
        let path = home.join(if tool == "codex" {
            "sessions/2026/09/07/rollout-source-id.jsonl"
        } else {
            "projects/project/source-id.jsonl"
        });
        Self {
            _temp: temp,
            paths,
            home,
            target,
            conversation: Conversation {
                tool: tool.into(),
                profile: "source".into(),
                id: "source-id".into(),
                native_name: Some("named source".into()),
                title: "source".into(),
                first_prompt: None,
                cwd: PathBuf::from("/project"),
                started_at: Some(now),
                updated_at: now,
                enabled: true,
                bypass: false,
                transcript_path: path,
            },
        }
    }

    fn write(&self, rows: &[Value]) {
        write_rows(&self.conversation.transcript_path, rows);
    }
    fn copy(&self) -> Result<(String, Vec<Value>)> {
        let id = copy(&self.paths, &self.conversation, &self.target)?;
        let key = ConversationKey::new(&self.conversation.tool, "target", &id);
        let path = locate_transcript(&self.target, &key)?.unwrap();
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
        Ok((id, read_rows(&path)))
    }
}

fn write_rows(path: &Path, rows: &[Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = File::create(path).unwrap();
    for row in rows {
        write_record(&mut file, row).unwrap();
    }
}

fn read_rows(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn meta(id: &str, ordinal: u64) -> Value {
    json!({"timestamp":"2026-09-07T00:00:00Z","ordinal":ordinal,"type":"session_meta","payload":{"id":id,"session_id":id,"cwd":"/project","history_mode":"paginated"}})
}

fn item(ordinal: u64, kind: &str, text: &str) -> Value {
    json!({"timestamp":"2026-09-07T00:00:00Z","ordinal":ordinal,"type":kind,"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}})
}

#[test]
fn codex_copies_nested_inherited_prefixes_and_preserves_native_payloads() {
    let f = Fixture::new("codex");
    let parent_path = f.home.join("archived_sessions/rollout-parent.jsonl");
    let tool = json!({"ordinal":2,"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"tool marker"}});
    write_rows(
        &parent_path,
        &[
            meta("parent", 0),
            item(1, "response_item", "parent marker"),
            tool.clone(),
        ],
    );
    let parent_bytes = std::fs::metadata(&parent_path).unwrap().len();
    let mut file = OpenOptions::new().append(true).open(&parent_path).unwrap();
    write_record(
        &mut file,
        &item(
            3,
            "response_item",
            "later parent turn must not be inherited",
        ),
    )
    .unwrap();
    let middle_path = f.home.join("sessions/rollout-middle.jsonl");
    let mut middle = meta("middle", 3);
    middle["payload"]["history_base"] =
        json!({"thread_id":"parent","end_ordinal_exclusive":3,"end_byte_offset":parent_bytes});
    write_rows(
        &middle_path,
        &[middle, item(4, "compacted", "compaction marker")],
    );
    let middle_bytes = std::fs::metadata(&middle_path).unwrap().len();
    let mut header = meta("source-id", 5);
    header["payload"]["subagent_history_start_ordinal"] = json!(6);
    header["payload"]["history_base"] =
        json!({"thread_id":"middle","end_ordinal_exclusive":5,"end_byte_offset":middle_bytes});
    f.write(&[header, item(6, "response_item", "child marker")]);
    let original = std::fs::read(&f.conversation.transcript_path).unwrap();
    let (id, rows) = f.copy().unwrap();
    assert_ne!(id, "source-id");
    assert_eq!(rows[0]["payload"]["id"], id);
    assert_eq!(rows[0]["payload"]["session_id"], id);
    assert!(rows[0]["payload"].get("history_base").is_none());
    assert!(rows[0]["payload"]
        .get("subagent_history_start_ordinal")
        .is_none());
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[2]["payload"], tool["payload"]);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row["ordinal"], i as u64);
    }
    let text = serde_json::to_string(&rows).unwrap();
    for marker in [
        "parent marker",
        "tool marker",
        "compaction marker",
        "child marker",
    ] {
        assert!(text.contains(marker));
    }
    assert!(!text.contains("later parent turn"));
    assert_eq!(
        std::fs::read(&f.conversation.transcript_path).unwrap(),
        original
    );
    let (second, _) = f.copy().unwrap();
    assert_ne!(id, second);
    std::fs::remove_dir_all(&f.home).unwrap();
    let catalog = query(&f.paths, &ConversationQuery::all().with_profile("target")).unwrap();
    assert_eq!(catalog.conversations.len(), 2);
    assert!(catalog
        .conversations
        .iter()
        .all(|c| c.native_name.as_deref() == Some("named source")));
}

#[test]
fn claude_relocates_companions_and_keeps_message_links_without_runtime_ownership() {
    let f = Fixture::new("claude");
    let companion = f
        .conversation
        .transcript_path
        .with_extension("")
        .join("tool-results/output with spaces.txt");
    std::fs::create_dir_all(companion.parent().unwrap()).unwrap();
    std::fs::write(&companion, "large result marker").unwrap();
    std::fs::write(f.target.join("settings.json"), "destination settings").unwrap();
    std::fs::write(f.home.join("auth.json"), "source account sentinel").unwrap();
    let image = f.home.join("image-cache/source-id/photo.png");
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    std::fs::write(&image, b"synthetic image bytes").unwrap();
    f.write(&[
        json!({"type":"user","sessionId":"source-id","uuid":"message-1","parentUuid":null,"cwd":"/project","message":{"role":"user","content":"literal source-id and /project stay unchanged"}}),
        json!({"type":"assistant","sessionId":"source-id","uuid":"message-2","parentUuid":"message-1","message":{"content":format!("<persisted-output>\nFull output saved to: {}\n</persisted-output>",companion.display())},"imagePath":image}),
        json!({"type":"bridge-session","sessionId":"source-id","ownerAccountUuid":"source-account"}),
        json!({"type":"queue-operation","operation":"enqueue","content":"never run this"}),
    ]);
    let original = std::fs::read(&f.conversation.transcript_path).unwrap();
    let (id, rows) = f.copy().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["sessionId"], id);
    assert_eq!(rows[1]["parentUuid"], "message-1");
    assert_eq!(
        rows[0]["message"]["content"],
        "literal source-id and /project stay unchanged"
    );
    let expected = f
        .target
        .join("projects/project")
        .join(&id)
        .join("tool-results/output with spaces.txt");
    assert!(rows[1]["message"]["content"]
        .as_str()
        .unwrap()
        .contains(expected.to_str().unwrap()));
    assert_eq!(
        std::fs::read(&f.conversation.transcript_path).unwrap(),
        original
    );
    std::fs::remove_dir_all(&f.home).unwrap();
    assert_eq!(
        std::fs::read_to_string(&expected).unwrap(),
        "large result marker"
    );
    assert_eq!(
        std::fs::read(rows[1]["imagePath"].as_str().unwrap()).unwrap(),
        b"synthetic image bytes"
    );
    assert_eq!(
        std::fs::read_to_string(f.target.join("settings.json")).unwrap(),
        "destination settings"
    );
    assert!(!f.target.join("auth.json").exists());
}

#[test]
fn codex_relocates_referenced_assets_only() {
    let f = Fixture::new("codex");
    let asset = f.home.join("attachments/referenced.png");
    std::fs::create_dir_all(asset.parent().unwrap()).unwrap();
    std::fs::write(&asset, b"image").unwrap();
    std::fs::write(asset.with_file_name("unrelated.png"), b"unrelated").unwrap();
    let mut message = item(1, "response_item", "image");
    message["payload"]["content"] =
        json!([{"type":"input_image","image_url":format!("file://{}",asset.display())}]);
    f.write(&[meta("source-id", 0), message]);
    let (_, rows) = f.copy().unwrap();
    let path = rows[1]["payload"]["content"][0]["image_url"]
        .as_str()
        .unwrap()
        .strip_prefix("file://")
        .unwrap();
    std::fs::remove_dir_all(&f.home).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"image");
    assert!(!Path::new(path).with_file_name("unrelated.png").exists());
}

#[test]
fn invalid_histories_and_asset_symlinks_publish_nothing() {
    for case in [
        "missing-parent",
        "cycle",
        "malformed",
        "symlink",
        "unknown-mode",
    ] {
        let f = Fixture::new("codex");
        let mut header = meta("source-id", 0);
        let mut body = item(1, "response_item", "hello");
        match case {
            "missing-parent" | "cycle" => {
                header["payload"]["history_base"] = json!({"thread_id":if case=="cycle" {"source-id"} else {"missing"},"end_byte_offset":1,"end_ordinal_exclusive":0})
            }
            "unknown-mode" => header["payload"]["history_mode"] = json!("future-format"),
            "symlink" => {
                let asset = f.home.join("attachments/result");
                std::fs::create_dir_all(asset.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink("/etc/passwd", &asset).unwrap();
                body["payload"]["imagePath"] = json!(asset);
            }
            _ => {}
        }
        f.write(&[header, body]);
        if case == "malformed" {
            OpenOptions::new()
                .append(true)
                .open(&f.conversation.transcript_path)
                .unwrap()
                .write_all(b"{bad}\n")
                .unwrap();
        }
        assert!(f.copy().is_err(), "{case}");
        assert_eq!(std::fs::read_dir(&f.target).unwrap().count(), 0, "{case}");
    }
}

#[test]
fn live_snapshot_omits_partial_tail_marks_interruption_and_rejects_truncation() {
    let f = Fixture::new("codex");
    f.write(&[meta("source-id",0),json!({"ordinal":1,"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}),item(2,"response_item","complete record")]);
    OpenOptions::new()
        .append(true)
        .open(&f.conversation.transcript_path)
        .unwrap()
        .write_all(b"{unfinished")
        .unwrap();
    let (_, rows) = f.copy().unwrap();
    assert_eq!(rows.last().unwrap()["payload"]["type"], "turn_aborted");
    assert_eq!(rows.last().unwrap()["ordinal"], 3);
    let snapshot = Snapshot::open(&f.home, &f.conversation.transcript_path, None).unwrap();
    File::create(&f.conversation.transcript_path).unwrap();
    assert!(snapshot
        .visit(|_| Ok(()))
        .unwrap_err()
        .to_string()
        .contains("source changed"));
}

#[test]
fn claude_copies_ancestor_references_and_native_notices_without_changing_plain_dialogue() {
    let f = Fixture::new("claude");
    let ancestor = f
        .home
        .join("projects/project/ancestor/tool-results/result.txt");
    let image = f.home.join("image-cache/ancestor/photo.png");
    for (path, bytes) in [
        (&ancestor, b"result".as_slice()),
        (&image, b"image".as_slice()),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let literal = format!(
        "Do not create {} or change {}",
        f.home.join("paste-cache/missing.txt").display(),
        ancestor.display()
    );
    let companion = f
        .conversation
        .transcript_path
        .with_extension("")
        .join("subagents/agent.jsonl");
    write_rows(
        &companion,
        &[
            json!({"type":"fork-context-ref","parentSessionId":"ancestor","parentLastUuid":"message-1"}),
            json!({"type":"assistant","sessionId":"source-id","toolUseResult":{"persistedOutputPath":ancestor}}),
            json!({"type":"queue-operation","operation":"enqueue","content":"pending"}),
        ],
    );
    f.write(&[
        json!({"type":"user","sessionId":"ancestor","uuid":"message-1","cwd":"/project","message":{"role":"user","content":literal}}),
        json!({"type":"assistant","sessionId":"source-id","uuid":"message-2","parentUuid":"message-1","message":{"content":[{"type":"text","text":format!("[Image: source: {}]",image.display())}]},"toolUseResult":{"persistedOutputPath":ancestor,"file":{"filePath":ancestor}}}),
        json!({"type":"artifact-comment-monitor","sessionId":"source-id","armed":true}),
        json!({"type":"artifact-autoreact-ledger","sessionId":"source-id","accountUuid":"source-account"}),
        json!({"type":"worktree-state","sessionId":"source-id"}),
        json!({"type":"isolation-latch","sessionId":"source-id"}),
    ]);
    let (id, rows) = f.copy().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["sessionId"], id);
    assert_eq!(rows[0]["message"]["content"], literal);
    let output = rows[1]["toolUseResult"]["persistedOutputPath"]
        .as_str()
        .unwrap();
    let notice = rows[1]["message"]["content"][0]["text"].as_str().unwrap();
    let copied_image = notice
        .strip_prefix("[Image: source: ")
        .unwrap()
        .strip_suffix(']')
        .unwrap();
    let companion = f
        .target
        .join("projects/project")
        .join(&id)
        .join("subagents/agent.jsonl");
    let companion_rows = read_rows(&companion);
    assert_eq!(companion_rows.len(), 2);
    assert_eq!(companion_rows[0]["parentSessionId"], id);
    assert_eq!(
        companion_rows[1]["toolUseResult"]["persistedOutputPath"],
        output
    );
    std::fs::remove_dir_all(&f.home).unwrap();
    assert_eq!(std::fs::read(output).unwrap(), b"result");
    assert_eq!(std::fs::read(copied_image).unwrap(), b"image");
}

#[test]
fn missing_ancestor_assets_and_inconsistent_parent_bounds_fail_before_publication() {
    let f = Fixture::new("claude");
    f.write(&[json!({"type":"user","sessionId":"source-id","message":{"role":"user","content":"hello"},"toolUseResult":{"persistedOutputPath":f.home.join("projects/project/missing/tool-results/output.txt")}})]);
    assert!(f
        .copy()
        .unwrap_err()
        .to_string()
        .contains("missing conversation asset"));
    assert_eq!(std::fs::read_dir(&f.target).unwrap().count(), 0);

    let f = Fixture::new("codex");
    let parent = f.home.join("sessions/rollout-parent.jsonl");
    write_rows(
        &parent,
        &[meta("parent", 0), item(1, "response_item", "parent")],
    );
    let mut header = meta("source-id", 3);
    header["payload"]["history_base"] = json!({"thread_id":"parent","end_byte_offset":std::fs::metadata(&parent).unwrap().len(),"end_ordinal_exclusive":3});
    f.write(&[header]);
    assert!(f
        .copy()
        .unwrap_err()
        .to_string()
        .contains("boundaries disagree"));
    assert_eq!(std::fs::read_dir(&f.target).unwrap().count(), 0);
}

#[test]
fn claude_subagent_with_unavailable_parent_context_fails_before_publication() {
    let f = Fixture::new("claude");
    f.write(&[json!({"type":"user","uuid":"visible","message":{"role":"user","content":"hello"}})]);
    write_rows(
        &f.conversation
            .transcript_path
            .with_extension("")
            .join("subagents/agent.jsonl"),
        &[
            json!({"type":"fork-context-ref","parentSessionId":"outside","parentLastUuid":"not-in-root"}),
        ],
    );
    assert!(f
        .copy()
        .unwrap_err()
        .to_string()
        .contains("outside the selected conversation"));
    assert_eq!(std::fs::read_dir(&f.target).unwrap().count(), 0);
}
