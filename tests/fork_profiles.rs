//! Exercise fork selection and native launch together, with disposable profile
//! homes and a recording child instead of real account credentials.

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
        std::fs::write(&script, "printf '%s\\n' \"${CODEX_HOME:-$CLAUDE_CONFIG_DIR}\" \"$PWD\" \"$@\" > \"$RTR_FORK_MARKER\"\nexit \"${RTR_FORK_EXIT:-0}\"\n").unwrap();
        let command = toml::Value::String(script.display().to_string());
        std::fs::write(paths.config_file(), format!(
            "[tools.{tool}]\ncommand=['sh', {command}]\nargs=['--model','configured-model']\ncopy=[]\n[tools.{tool}.profiles.a]\n[tools.{tool}.profiles.b]\nbypass=true\n[tools.{tool}.profiles.disabled]\nenabled=false\n"
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
    use std::os::unix::fs::PermissionsExt;
    for tool in ["codex", "claude"] {
        let f = Fixture::new(tool);
        let picker = f.temp.path().join("picker.sh");
        std::fs::write(&picker,"#!/bin/sh\nIFS='\t' read -r key rest\nprintf '%s\\n%s\\n' \"${RTR_PICK_KEY-}\" \"$key\"\nexit \"${RTR_PICK_EXIT:-0}\"\n").unwrap();
        std::fs::set_permissions(&picker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = f
            .command(&[
                "fork",
                "--tool",
                tool,
                "--profile",
                "a",
                "--to-profile",
                "b",
            ])
            .env("RTR_FZF", &picker)
            .env("RTR_PICK_KEY", "ctrl-r")
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert_eq!(f.cursor(), 0);
        assert!(f.copies().is_empty());
        let result = f
            .command(&["sessions", "--tool", tool])
            .env("RTR_FZF", &picker)
            .env("RTR_PICK_EXIT", "130")
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(f.cursor(), 0);
        f.run(&[tool]);
        let result = f
            .command(&["sessions", "--tool", tool, "--profile", "a"])
            .env("RTR_FZF", &picker)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(f.cursor(), 0);
        assert_eq!(f.copies().len(), 1);
    }
}
