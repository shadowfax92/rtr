//! Exercise fork selection and native launch together, with disposable profile
//! homes and a recording child instead of real account credentials.

mod support;

use std::path::PathBuf;
use std::process::{Command, Output};

use rtr::conversations::{query, ConversationQuery};
use rtr::paths::Paths;
use rtr::state::State;
use serde_json::json;

const CODEX_CONVERSATION_ID: &str = "11111111-1111-4111-8111-111111111111";
const CODEX_HISTORY_FILE_ID: &str = "22222222-2222-4222-8222-222222222222";

struct Fixture {
    temp: tempfile::TempDir,
    paths: Paths,
    tool: &'static str,
    transcript: PathBuf,
}

impl Fixture {
    fn new(tool: &'static str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let script = temp.path().join("record.sh");
        std::fs::write(&script, "record_marker=$1; shift\nprintf '%s\\n' \"${CODEX_HOME:-$CLAUDE_CONFIG_DIR}\" \"$PWD\" \"$@\" > \"$record_marker\"\nexit \"${RTR_FORK_EXIT:-0}\"\n").unwrap();
        let command = toml::Value::String(script.display().to_string());
        let marker = toml::Value::String(temp.path().join("marker").display().to_string());
        std::fs::write(paths.config_file(), format!(
            "[tools.{tool}]\ncommand=['sh', {command}, {marker}]\nargs=['--model','configured-model']\ncopy=[]\n[tools.{tool}.profiles.a]\n[tools.{tool}.profiles.b]\nbypass=true\n[tools.{tool}.profiles.disabled]\nenabled=false\n"
        )).unwrap();
        let home = paths.ensure_profile_home_dir(tool, "a").unwrap();
        let transcript = home.join(if tool == "codex" {
            "sessions/rollout-source-id.jsonl"
        } else {
            "projects/project/source-id.jsonl"
        });
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let rows = if tool == "codex" {
            vec![
                json!({"type":"session_meta","payload":{"id":"source-id","cwd":temp.path(),"history_mode":"legacy"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"history marker"}]}}),
            ]
        } else {
            vec![
                json!({"type":"user","sessionId":"source-id","uuid":"message-1","parentUuid":null,"cwd":temp.path(),"message":{"role":"user","content":"history marker"}}),
            ]
        };
        std::fs::write(
            &transcript,
            rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        Self {
            temp,
            paths,
            tool,
            transcript,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rtr"));
        cmd.args(args)
            .current_dir(self.temp.path())
            .env("HOME", self.temp.path())
            .env("RTR_CONFIG_DIR", &self.paths.config_dir)
            .env("RTR_STATE_DIR", &self.paths.state_dir)
            .env("RTR_FORK_MARKER", self.temp.path().join("marker"))
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn cursor(&self) -> usize {
        State::load(&self.paths.state_file())
            .unwrap()
            .round_robin_cursor(self.tool)
    }
    fn launch(&self) -> Vec<String> {
        std::fs::read_to_string(self.temp.path().join("marker"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
    fn copies(&self) -> Vec<rtr::conversations::Conversation> {
        query(
            &self.paths,
            &ConversationQuery::all()
                .with_tool(self.tool)
                .with_profile("b"),
        )
        .unwrap()
        .conversations
    }
}

#[test]
fn codex_forks_history_segments_with_shared_conversation_ids() {
    let f = Fixture::new("codex");
    let home = f.paths.profile_home_dir("codex", "a");
    let root = home.join(format!(
        "archived_sessions/rollout-{CODEX_CONVERSATION_ID}.jsonl"
    ));
    let segment = home.join(format!(
        "sessions/rollout-{CODEX_CONVERSATION_ID}_{CODEX_HISTORY_FILE_ID}.jsonl"
    ));
    let metadata = |ordinal, base: serde_json::Value, id| json!({"type":"session_meta","ordinal":ordinal,"payload":{"id":id,"session_id":id,"cwd":f.temp.path(),"history_mode":"paginated","history_base":base}});
    let message = |ordinal, text| json!({"type":"response_item","ordinal":ordinal,"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}});
    let write = |path: &std::path::Path, rows: Vec<serde_json::Value>| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            rows.iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        std::fs::metadata(path).unwrap().len()
    };
    let root_bytes = write(
        &root,
        vec![
            metadata(0, json!(null), CODEX_CONVERSATION_ID),
            message(1, "root history marker"),
        ],
    );
    // A history reference names the segment file, even though its metadata
    // retains the root conversation ID. Both files belong in the inherited chain.
    let segment_bytes = write(
        &segment,
        vec![
            metadata(
                2,
                json!({"thread_id":CODEX_CONVERSATION_ID,"end_byte_offset":root_bytes,"end_ordinal_exclusive":2}),
                CODEX_CONVERSATION_ID,
            ),
            message(3, "segment history marker"),
        ],
    );
    write(
        &f.transcript,
        vec![
            metadata(
                4,
                json!({"thread_id":CODEX_HISTORY_FILE_ID,"end_byte_offset":segment_bytes,"end_ordinal_exclusive":4}),
                "source-id",
            ),
            message(5, "child history marker"),
        ],
    );
    use std::io::Write;
    for (path, row) in [
        (&root, message(2, "later root turn")),
        (&segment, message(4, "later segment turn")),
    ] {
        writeln!(
            std::fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{row}"
        )
        .unwrap();
    }
    let originals = [&root, &segment, &f.transcript].map(|path| std::fs::read(path).unwrap());

    f.run(&["fork", "source-id", "--tool", "codex", "--to-profile", "b"]);
    let copies = f.copies();
    assert_eq!(copies.len(), 1);
    let copied = &copies[0];
    assert_ne!(copied.id, "source-id");
    let rows = std::fs::read_to_string(&copied.transcript_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 4);
    assert!(rows[0]["payload"].get("history_base").is_none());
    for (index, marker) in [
        "root history marker",
        "segment history marker",
        "child history marker",
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(rows[index + 1]["ordinal"], index + 1);
        assert_eq!(rows[index + 1]["payload"]["content"][0]["text"], *marker);
    }
    for (path, original) in [&root, &segment, &f.transcript].into_iter().zip(originals) {
        assert_eq!(std::fs::read(path).unwrap(), original);
    }
    // Catalog selection can also start at the segment itself. Its logical ID
    // then equals its parent's ID, but the two files are still distinct history.
    f.run(&[
        "fork",
        CODEX_CONVERSATION_ID,
        "--tool",
        "codex",
        "--to-profile",
        "b",
    ]);
    let segment_copy = f
        .copies()
        .into_iter()
        .find(|conversation| conversation.id != copied.id)
        .unwrap();
    let segment_history = std::fs::read_to_string(segment_copy.transcript_path).unwrap();
    assert!(segment_history.contains("root history marker"));
    assert!(segment_history.contains("segment history marker"));
    assert!(segment_history.contains("later segment turn"));
    assert!(!segment_history.contains("later root turn"));
    std::fs::remove_dir_all(home).unwrap();
    f.run(&["resume", &copied.id, "--tool", "codex", "--profile", "b"]);
    assert!(f.launch().contains(&copied.id));
}

#[test]
fn codex_segment_forks_reject_wrong_conversation_ids_and_real_cycles() {
    for case in [
        "wrong-conversation",
        "history-id-as-conversation",
        "truncated-conversation",
        "cycle",
    ] {
        let f = Fixture::new("codex");
        let segment = f.paths.profile_home_dir("codex", "a").join(format!(
            "sessions/rollout-{CODEX_CONVERSATION_ID}_{CODEX_HISTORY_FILE_ID}.jsonl"
        ));
        let id = match case {
            "history-id-as-conversation" => CODEX_HISTORY_FILE_ID,
            "truncated-conversation" => CODEX_CONVERSATION_ID.rsplit('-').next().unwrap(),
            "cycle" => CODEX_CONVERSATION_ID,
            _ => "unrelated-id",
        };
        let parent = json!({"type":"session_meta","ordinal":0,"payload":{"id":id,"cwd":f.temp.path(),"history_mode":"paginated","history_base":if case=="cycle" {json!({"thread_id":"source-id","end_byte_offset":1,"end_ordinal_exclusive":0})} else {json!(null)}}});
        std::fs::write(&segment, format!("{parent}\n")).unwrap();
        let end = if case == "cycle" { 0 } else { 1 };
        let source = json!({"type":"session_meta","ordinal":end,"payload":{"id":"source-id","cwd":f.temp.path(),"history_mode":"paginated","history_base":{"thread_id":CODEX_HISTORY_FILE_ID,"end_byte_offset":std::fs::metadata(&segment).unwrap().len(),"end_ordinal_exclusive":end}}});
        std::fs::write(&f.transcript, format!("{source}\n")).unwrap();
        let original = std::fs::read(&f.transcript).unwrap();
        let output = f
            .command(&["fork", "source-id", "--tool", "codex", "--to-profile", "b"])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{case}: published invalid history"
        );
        let error = String::from_utf8_lossy(&output.stderr);
        let expected = if case == "cycle" {
            "cyclic Codex history ancestry"
        } else {
            "Codex history filename and session identity disagree"
        };
        assert!(error.contains(expected), "{case}: {error}");
        assert!(f.copies().is_empty());
        assert_eq!(std::fs::read(&f.transcript).unwrap(), original);
    }
}

#[test]
fn weighted_forks_and_launches_share_progress_while_explicit_runs_and_resumes_preserve_it() {
    for tool in ["codex", "claude"] {
        let f = Fixture::new(tool);
        f.run(&["weight", tool, "--profile", "a", "25"]);
        // With a=25 and b=75, deterministic selection is b,a,b,b.
        f.run(&[tool]);
        let before = std::fs::read(f.paths.state_file()).unwrap();
        f.run(&[
            "fork",
            "source-id",
            "--tool",
            tool,
            "--profile",
            "a",
            "--to-profile",
            "b",
        ]);
        assert_eq!(std::fs::read(f.paths.state_file()).unwrap(), before);
        f.run(&["resume", "source-id", "--tool", tool, "--profile", "a"]);
        assert_eq!(std::fs::read(f.paths.state_file()).unwrap(), before);
        for _ in 0..3 {
            f.run(&["fork", "source-id", "--tool", tool, "--profile", "a"]);
        }
        let events = rtr::usage::read_events(&f.paths.usage_file()).unwrap();
        let automatic: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(i, _)| *i == 0 || *i >= 3)
            .map(|(_, event)| event.profile.as_str())
            .collect();
        assert_eq!(automatic, ["b", "a", "b", "b"]);
    }
}

#[test]
fn fork_and_normal_launch_share_rotation_but_explicit_destination_and_resume_do_not() {
    for tool in ["codex", "claude"] {
        let f = Fixture::new(tool);
        let original = std::fs::read(&f.transcript).unwrap();
        f.run(&[tool]);
        assert_eq!(f.cursor(), 1);
        let output = f.run(&[
            "fork",
            "source-id",
            "--tool",
            tool,
            "--profile",
            "a",
            "--",
            "--model",
            "override-model",
        ]);
        assert_eq!(f.cursor(), 0);
        let first = f.copies().pop().unwrap();
        assert_ne!(first.id, "source-id");
        let launch = f.launch();
        assert_eq!(
            launch[0],
            f.paths.profile_home_dir(tool, "b").to_str().unwrap()
        );
        assert!(launch.contains(&first.id));
        assert!(launch.contains(
            &if tool == "codex" {
                "resume"
            } else {
                "--resume"
            }
            .into()
        ));
        assert!(!launch.contains(&"--fork-session".into()));
        assert!(launch.contains(&"override-model".into()));
        assert!(!launch.contains(&"configured-model".into()));
        assert!(String::from_utf8_lossy(&output.stderr).contains(&format!(
            "rtr resume {} --tool {tool} --profile b",
            first.id
        )));

        f.run(&[
            "fork",
            "source-id",
            "--tool",
            tool,
            "--profile",
            "a",
            "--to-profile",
            "b",
        ]);
        assert_eq!(f.cursor(), 0);
        assert_eq!(f.copies().len(), 2);
        f.run(&["fork", "source-id", "--tool", tool, "--profile", "a"]);
        assert_eq!(f.cursor(), 1);
        assert!(f.launch().contains(
            &if tool == "codex" {
                "fork"
            } else {
                "--fork-session"
            }
            .into()
        ));
        assert!(f.launch().contains(&"source-id".into()));

        f.run(&["disable", tool, "--profile", "a"]);
        f.run(&["resume", "source-id", "--tool", tool, "--profile", "a"]);
        assert_eq!(f.cursor(), 1);
        assert_eq!(
            f.launch()[0],
            f.paths.profile_home_dir(tool, "a").to_str().unwrap()
        );
        f.run(&[
            "fork",
            "source-id",
            "--tool",
            tool,
            "--profile",
            "a",
            "--to-profile",
            "b",
        ]);
        assert_eq!(f.cursor(), 1);
        assert_eq!(f.copies().len(), 3);
        assert_eq!(std::fs::read(&f.transcript).unwrap(), original);
        let events = rtr::usage::read_events(&f.paths.usage_file()).unwrap();
        assert_eq!(events.last().unwrap().profile, "b");
        assert!(!events.last().unwrap().bypass);
    }
}

#[test]
fn failed_forks_obey_reservation_boundary_and_preserve_published_copies() {
    for tool in ["codex", "claude"] {
        let f = Fixture::new(tool);
        for target in ["disabled", "missing"] {
            let output = f
                .command(&["fork", "source-id", "--tool", tool, "--to-profile", target])
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert_eq!(f.cursor(), 0);
        }
        let redirect = if tool == "codex" {
            "--remote=ws://localhost"
        } else {
            "--resume=other-id"
        };
        assert!(!f
            .command(&["fork", "source-id", "--tool", tool, "--", redirect])
            .output()
            .unwrap()
            .status
            .success());
        assert_eq!(f.cursor(), 0);
        if tool == "claude" {
            for redirect in [
                "-rother-id",
                "-prOTHER",
                "-c",
                "--cloud",
                "--environment=remote",
            ] {
                assert!(
                    !f.command(&["fork", "source-id", "--tool", tool, "--", redirect])
                        .output()
                        .unwrap()
                        .status
                        .success(),
                    "{redirect}"
                );
                assert_eq!(f.cursor(), 0);
            }
        }
        f.run(&[tool]);
        assert_eq!(f.cursor(), 1);
        let original = std::fs::read(&f.transcript).unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f.transcript)
            .unwrap()
            .write_all(b"{bad}\n")
            .unwrap();
        let output = f
            .command(&["fork", "source-id", "--tool", tool])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(f.cursor(), 0);
        assert!(f.copies().is_empty());
        std::fs::write(&f.transcript, original).unwrap();
        let output = f
            .command(&["fork", "source-id", "--tool", tool, "--to-profile", "b"])
            .env("RTR_FORK_EXIT", "23")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(23));
        let copied = f.copies().pop().unwrap();
        std::fs::remove_dir_all(f.paths.profile_home_dir(tool, "a")).unwrap();
        f.run(&["resume", &copied.id, "--tool", tool, "--profile", "b"]);
        assert!(f.launch().contains(&copied.id));
        assert_eq!(f.cursor(), 0);
    }
}

#[test]
fn picker_forks_share_rotation_and_explicit_target_cannot_become_resume() {
    for tool in ["codex", "claude"] {
        let f = Fixture::new(tool);
        support::drive_picker_steps(
            &f.paths,
            f.temp.path(),
            &[
                "fork",
                "--tool",
                tool,
                "--profile",
                "a",
                "--to-profile",
                "b",
            ],
            &[
                ("configured-model", b"\x12"),
                ("Ctrl-R is unavailable", b"\x1b"),
            ],
        );
        assert_eq!(f.cursor(), 0);
        assert!(f.copies().is_empty());
        f.run(&[tool]);
        support::drive_picker(
            &f.paths,
            f.temp.path(),
            &["sessions", "--tool", tool, "--profile", "a"],
            "configured-model",
            b"\r",
        );
        assert_eq!(f.cursor(), 0);
        assert_eq!(f.copies().len(), 1);
        support::drive_picker_steps(
            &f.paths,
            f.temp.path(),
            &[
                "fork",
                "--tool",
                tool,
                "--profile",
                "a",
                "--to-profile",
                "b",
            ],
            &[("configured-model", b"\r")],
        );
        assert_eq!(f.cursor(), 0);
        assert_eq!(f.copies().len(), 2);
    }
}
