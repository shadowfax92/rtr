//! Exercise fork selection and native launch together, with disposable profile
//! homes and a recording child instead of real account credentials.

mod support;

use std::path::PathBuf;
use std::process::{Command, Output};

use rtr::conversations::{query, ConversationQuery};
use rtr::paths::Paths;
use rtr::state::State;
use serde_json::json;

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
fn weighted_forks_and_launches_share_progress_while_explicit_runs_and_resumes_preserve_it() {
    for tool in ["codex", "claude"] {
        let f = Fixture::new(tool);
        f.run(&["weight", tool, "--profile", "a", "25"]);
        // Normal launches retain their shares; forks of a always select b.
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
        assert_eq!(automatic, ["b", "b", "b", "b"]);
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
        assert_eq!(f.cursor(), 0);
        assert_eq!(
            f.launch()[0],
            f.paths.profile_home_dir(tool, "b").to_str().unwrap()
        );
        assert!(!f.launch().contains(&"source-id".into()));

        f.run(&["disable", tool, "--profile", "a"]);
        f.run(&["resume", "source-id", "--tool", tool, "--profile", "a"]);
        assert_eq!(f.cursor(), 0);
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
        assert_eq!(f.cursor(), 0);
        assert_eq!(f.copies().len(), 4);
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

#[test]
fn missing_native_ids_explain_search_and_reset_the_picker_to_here() {
    let f = Fixture::new("claude");
    let missing = "00000000-0000-4000-8000-000000000099";
    let output = f
        .command(&["fork", missing, "--tool", "claude"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(missing));
    assert!(stderr.contains("Searched homes:"));
    assert!(stderr.contains(f.paths.profile_home_dir("claude", "a").to_str().unwrap()));
    let default = f.temp.path().join(".claude/projects/p");
    std::fs::create_dir_all(&default).unwrap();
    std::fs::write(default.join(format!("{missing}.jsonl")), "{}\n").unwrap();
    let output = f
        .command(&["resume", missing, "--tool", "claude"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("started outside rtr"));
    let output = support::drive_picker(
        &f.paths,
        f.temp.path(),
        &["fork", missing, "--tool", "claude"],
        "history marker",
        b"\x1b",
    );
    assert!(output.contains("Opening the picker in the current directory"));
    assert!(!f.temp.path().join("marker").exists());
}

#[test]
fn sole_enabled_profile_forks_in_place_with_an_explanation() {
    for tool in ["claude", "codex"] {
        let f = Fixture::new(tool);
        f.run(&["disable", tool, "--profile", "b"]);
        let output = f.run(&["fork", "source-id", "--tool", tool]);
        assert!(String::from_utf8_lossy(&output.stderr).contains("nowhere to rotate"));
        assert!(f.launch().contains(&"source-id".into()));
        assert!(f.copies().is_empty());
    }
}

#[test]
fn non_conversation_flag_lists_exec_runs_and_exact_resume_still_works() {
    let f = Fixture::new("codex");
    let mut lines: Vec<serde_json::Value> = std::fs::read_to_string(&f.transcript)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    lines[0]["payload"]["source"] = json!("exec");
    std::fs::write(
        &f.transcript,
        lines.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    let hidden: serde_json::Value =
        serde_json::from_slice(&f.run(&["sessions", "--json"]).stdout).unwrap();
    assert!(hidden["conversations"].as_array().unwrap().is_empty());
    let included: serde_json::Value = serde_json::from_slice(
        &f.run(&["sessions", "--json", "--include-non-conversations"])
            .stdout,
    )
    .unwrap();
    assert_eq!(included["conversations"][0]["id"], "source-id");
    f.run(&["resume", "source-id", "--tool", "codex"]);
    assert!(f.launch().contains(&"source-id".into()));
}
