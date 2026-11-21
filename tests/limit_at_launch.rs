//! Launch-exit contracts use shell recorders and native-shaped transcripts in
//! test-owned homes. No case launches a real agent or reads the owner's state.
//! RTR_TEST_CARGO_RUN repeats the contracts through actual cargo run commands.
use chrono::{DateTime, Duration, Utc};
use rtr::{paths::Paths, quota};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

const FIRST_ID: &str = "11111111-1111-4111-8111-111111111111";
const SECOND_ID: &str = "22222222-2222-4222-8222-222222222222";
const OTHER_ID: &str = "33333333-3333-4333-8333-333333333333";

// The recorder preserves argv as NUL-delimited bytes, so whitespace, empty
// arguments and shell-looking prompt text cannot hide an accidental rewrite.
// Native ownership compares transcript timestamps to file birth time. Create
// each file first and cross a second before using portable `date` timestamps;
// this models owned native writes rather than backdating fixture evidence.
const RECORDER: &str = r#"set -eu
attempt=1
while [ -d "$RTR_TEST_RECORD/$attempt" ]; do attempt=$((attempt + 1)); done
record="$RTR_TEST_RECORD/$attempt"
mkdir -p "$record"
: > "$record/argv"
for arg do printf '%s\000' "$arg" >> "$record/argv"; done
pwd -P > "$record/cwd"
cat > "$record/stdin"
if [ "$RTR_TEST_TOOL" = claude ]; then
    task_home="$CLAUDE_CONFIG_DIR"
else
    task_home="$CODEX_HOME"
fi
printf '%s\n' "$task_home" > "$record/home"
if [ "$attempt" -eq 1 ]; then native_id="$RTR_TEST_FIRST_ID"; else native_id="$RTR_TEST_SECOND_ID"; fi
expect_id=false
is_exec=false
is_json=false
native_source=cli
for arg do
    if [ "$expect_id" = true ]; then native_id="$arg"; expect_id=false; fi
    case "$arg" in
        --session-id|--resume|-r) expect_id=true ;;
        --session-id=*) native_id="${arg#--session-id=}" ;;
        exec|e) is_exec=true; native_source=exec ;;
        --json|--experimental-json) is_json=true ;;
    esac
done
printf '%s\n' "$native_id" > "$record/id"
if [ "$RTR_TEST_TOOL" = claude ]; then
    native_dir="$task_home/projects/fixture"
    native_file="$native_dir/$native_id.jsonl"
    mkdir -p "$task_home/sessions"
    printf '{"pid":%s,"sessionId":"%s"}\n' "$$" "$native_id" > "$task_home/sessions/$$.json"
else
    native_dir="$task_home/sessions"
    native_file="$native_dir/rollout-$native_id.jsonl"
fi
mkdir -p "$native_dir"
: > "$native_file"
if [ "$RTR_TEST_AMBIGUOUS" = true ]; then
    other_file="$native_dir/rollout-$RTR_TEST_OTHER_ID.jsonl"
    : > "$other_file"
fi
sleep 1
native_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
if [ "$attempt" -le "$RTR_TEST_REJECTIONS" ]; then template="$RTR_TEST_REJECT"; else template="$RTR_TEST_SUCCESS"; fi
sed -e "s/@ID@/$native_id/g" -e "s/@TIMESTAMP@/$native_at/g" -e "s/@SOURCE@/$native_source/g" "$template" > "$native_file"
if [ "$RTR_TEST_AMBIGUOUS" = true ]; then
    sed -e "s/@ID@/$RTR_TEST_OTHER_ID/g" -e "s/@TIMESTAMP@/$native_at/g" -e "s/@SOURCE@/$native_source/g" "$template" > "$other_file"
fi
if [ "$RTR_TEST_TOOL" = codex ] && [ "$is_exec" = true ]; then
    if [ "$is_json" = true ]; then
        printf '{"type":"thread.started","thread_id":"%s"}\n' "$native_id"
    else
        printf 'OpenAI Codex v0.169.0\n--------\nsession id: %s\n--------\n' "$native_id" >&2
    fi
fi
if [ "$attempt" -le "$RTR_TEST_REJECTIONS" ]; then exit 1; fi
if [ "$RTR_TEST_TOOL" = codex ] && [ "$is_json" = true ]; then
    printf '{"type":"item.completed","item":{"type":"agent_message","text":"OK"}}\n'
else
    printf 'OK\n'
fi
"#;

#[derive(Clone, Copy)]
enum Evidence {
    Rejection,
    ModelOutput,
    ToolCall,
    PreviousTurn,
}

/// A complete private RTR installation whose configured native executable is
/// the recorder. Each attempt gets a separate record, while profile homes and
/// quota persistence stay shared exactly as they do across real launches.
struct Fixture {
    temp: tempfile::TempDir,
    paths: Paths,
    cwd: PathBuf,
    tool: &'static str,
    reset: DateTime<Utc>,
}

impl Fixture {
    fn new(tool: &'static str, evidence: Evidence) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
        };
        fs::create_dir_all(&paths.config_dir).unwrap();
        let cwd = fs::canonicalize(temp.path()).unwrap().join("working dir");
        fs::create_dir(&cwd).unwrap();
        let script = temp.path().join("record.sh");
        fs::write(&script, RECORDER).unwrap();
        let config = format!(
            "[tools.{tool}]\ncommand=['sh',{}]\ncopy=[]\n[tools.{tool}.profiles.a]\n[tools.{tool}.profiles.b]\n[tools.{tool}.profiles.c]\n",
            toml::Value::String(script.display().to_string())
        );
        // Manual acceptance may begin with a temporary config copy. Replace
        // its whole policy before any launch so native commands stay synthetic.
        if let Some(source) = std::env::var_os("RTR_TEST_CONFIG_COPY") {
            fs::copy(source, paths.config_file()).unwrap();
        }
        fs::write(paths.config_file(), config).unwrap();
        // Codex's native human diagnostic has minute precision and no offset;
        // every RTR subprocess uses TZ=UTC to keep that format deterministic.
        let reset = DateTime::from_timestamp((Utc::now().timestamp() / 60 + 120) * 60, 0).unwrap();
        let f = Self {
            temp,
            paths,
            cwd,
            tool,
            reset,
        };
        f.template("reject.jsonl", f.rows(evidence, true, reset));
        f.template("success.jsonl", f.rows(Evidence::Rejection, false, reset));
        f
    }

    fn template(&self, name: &str, rows: Vec<Value>) {
        let mut file = fs::File::create(self.temp.path().join(name)).unwrap();
        for row in rows {
            writeln!(file, "{row}").unwrap();
        }
    }

    fn rows(&self, evidence: Evidence, rejected: bool, reset: DateTime<Utc>) -> Vec<Value> {
        if self.tool == "claude" {
            let user = json!({"type":"user","sessionId":"@ID@","cwd":self.cwd,"timestamp":"@TIMESTAMP@","message":{"role":"user","content":"fixture prompt"}});
            let output = json!({"type":"assistant","sessionId":"@ID@","timestamp":"@TIMESTAMP@","message":{"role":"assistant","model":"fixture-model","content":[{"type":"text","text":"already produced output"}]}});
            let mut rows = vec![user.clone()];
            match evidence {
                Evidence::ModelOutput => rows.push(output.clone()),
                Evidence::ToolCall => rows.push(json!({"type":"assistant","sessionId":"@ID@","timestamp":"@TIMESTAMP@","message":{"role":"assistant","model":"fixture-model","content":[{"type":"tool_use","id":"call","name":"Bash","input":{"command":"touch important-file"}}]}})),
                Evidence::PreviousTurn => rows.extend([output.clone(), user]),
                Evidence::Rejection => {}
            }
            rows.push(if rejected {
                json!({"type":"assistant","sessionId":"@ID@","timestamp":"@TIMESTAMP@","isApiErrorMessage":true,"apiError":"usage_limit_reached","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"Usage limit reached"}]},"quotaLimits":{"rateLimitType":"seven_day","resetsAt":reset.timestamp()}})
            } else {
                output
            });
            return rows;
        }

        let mut rows = vec![
            json!({"type":"session_meta","timestamp":"@TIMESTAMP@","payload":{"id":"@ID@","cwd":self.cwd,"source":"@SOURCE@","history_mode":"legacy"}}),
        ];
        let input = |turn| {
            vec![
                json!({"type":"event_msg","timestamp":"@TIMESTAMP@","payload":{"type":"task_started","turn_id":turn}}),
                json!({"type":"turn_context","timestamp":"@TIMESTAMP@","payload":{"turn_id":turn,"model":"fixture-model","effort":"high"}}),
                json!({"type":"event_msg","timestamp":"@TIMESTAMP@","payload":{"type":"item_started","turn_id":turn,"item":{"type":"UserMessage","content":[{"type":"text","text":"fixture prompt"}]}}}),
                json!({"type":"event_msg","timestamp":"@TIMESTAMP@","payload":{"type":"item_completed","turn_id":turn,"item":{"type":"UserMessage","content":[{"type":"text","text":"fixture prompt"}]}}}),
            ]
        };
        rows.extend(input("first"));
        match evidence {
            Evidence::ModelOutput => rows.push(json!({"type":"response_item","timestamp":"@TIMESTAMP@","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"already produced output"}]}})),
            Evidence::ToolCall => rows.push(json!({"type":"response_item","timestamp":"@TIMESTAMP@","payload":{"type":"function_call","name":"exec_command","call_id":"call","arguments":"{}"}})),
            Evidence::PreviousTurn => {
                rows.push(json!({"type":"event_msg","timestamp":"@TIMESTAMP@","payload":{"type":"task_complete","turn_id":"first","last_agent_message":"already completed a turn"}}));
                rows.extend(input("second"));
            }
            Evidence::Rejection => {}
        }
        let turn = if matches!(evidence, Evidence::PreviousTurn) {
            "second"
        } else {
            "first"
        };
        rows.push(if rejected {
            json!({"type":"event_msg","timestamp":"@TIMESTAMP@","payload":{"type":"task_complete","turn_id":turn,"error":{"codex_error_info":"usage_limit_exceeded","message":format!("You have hit your ChatGPT usage limit. Please try again at {}.",reset.format("%b %d, %Y %I:%M %p"))}}})
        } else {
            json!({"type":"event_msg","timestamp":"@TIMESTAMP@","payload":{"type":"task_complete","turn_id":turn,"last_agent_message":"OK"}})
        });
        rows
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = if std::env::var_os("RTR_TEST_CARGO_RUN").is_some() {
            let mut cargo = Command::new("cargo");
            let task_home = PathBuf::from(std::env::var_os("HOME").unwrap());
            for (key, fallback) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
                if std::env::var_os(key).is_none() {
                    cargo.env(key, task_home.join(fallback));
                }
            }
            cargo.args([
                "run",
                "--quiet",
                "--manifest-path",
                concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
                "--",
            ]);
            cargo
        } else {
            Command::new(env!("CARGO_BIN_EXE_rtr"))
        };
        command
            .args(args)
            .env("RTR_CONFIG_DIR", &self.paths.config_dir)
            .env("RTR_STATE_DIR", &self.paths.state_dir)
            .env("HOME", self.temp.path())
            .env("TZ", "UTC")
            .env("NO_COLOR", "1")
            .env("RTR_TEST_TOOL", self.tool)
            .env("RTR_TEST_RECORD", self.temp.path().join("launches"))
            .env("RTR_TEST_REJECT", self.temp.path().join("reject.jsonl"))
            .env("RTR_TEST_SUCCESS", self.temp.path().join("success.jsonl"))
            .env("RTR_TEST_REJECTIONS", "1")
            .env("RTR_TEST_AMBIGUOUS", "false")
            .env("RTR_TEST_FIRST_ID", FIRST_ID)
            .env("RTR_TEST_SECOND_ID", SECOND_ID)
            .env("RTR_TEST_OTHER_ID", OTHER_ID)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .stdin(Stdio::null())
            .current_dir(&self.cwd);
        command
    }

    fn launch(&self, argv: &[&str]) -> Command {
        let mut args = vec![self.tool, "--"];
        args.extend(argv);
        self.command(&args)
    }

    fn json(&self, args: &[&str]) -> Vec<Value> {
        let output = self.command(args).output().unwrap();
        assert_success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn record(&self, attempt: usize, field: &str) -> String {
        fs::read_to_string(
            self.temp
                .path()
                .join("launches")
                .join(attempt.to_string())
                .join(field),
        )
        .unwrap()
        .trim_end_matches('\n')
        .to_owned()
    }

    fn argv(&self, attempt: usize) -> Vec<String> {
        let bytes = fs::read(
            self.temp
                .path()
                .join("launches")
                .join(attempt.to_string())
                .join("argv"),
        )
        .unwrap();
        if bytes.is_empty() {
            return Vec::new();
        }
        bytes
            .strip_suffix(b"\0")
            .unwrap()
            .split(|byte| *byte == 0)
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect()
    }

    fn attempts(&self) -> usize {
        fs::read_dir(self.temp.path().join("launches"))
            .unwrap()
            .count()
    }

    fn assert_hold(&self, profile: &str) {
        // Read persisted quota before running ls or another preflight; otherwise
        // those commands could learn a missing exit-time hold and mask the bug.
        let holds = quota::read_holds(&self.paths).unwrap();
        let own = &holds[&(self.tool.to_owned(), profile.to_owned())];
        assert_eq!(own.holds.len(), 1, "{holds:?}");
        assert_eq!(own.holds[0].reset_at, Some(self.reset));
    }

    fn assert_launch_records(&self, expected: &[&str]) {
        assert_eq!(self.attempts(), expected.len());
        for (index, profile) in expected.iter().enumerate() {
            assert_eq!(
                self.record(index + 1, "home"),
                self.paths
                    .profile_home_dir(self.tool, profile)
                    .to_str()
                    .unwrap()
            );
            assert_eq!(self.record(index + 1, "cwd"), self.cwd.to_str().unwrap());
        }
    }

    fn assert_retry_log_and_listing(&self, output: &Output) {
        self.assert_hold("a");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("rejected before model output or tool calls"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let events = self.json(&["log", "-j", "-n", "100"]);
        let routes: Vec<_> = events.iter().filter(|e| e["kind"] == "route").collect();
        assert_eq!(routes.len(), 2, "{events:?}");
        assert_eq!(routes[0]["destination"], "a");
        assert_eq!(routes[1]["destination"], "b");
        assert_eq!(routes[1]["source"], "a");
        assert_eq!(
            routes[1]["reason"],
            "rerun_after_limit_before_model_activity"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e["kind"] == "hold_learned")
                .count(),
            1
        );
        let held = events.iter().find(|e| e["kind"] == "hold_learned").unwrap();
        assert_eq!(held["profile"], "a");
        assert_eq!(held["reset_at"], json!(self.reset));
        assert_eq!(
            events
                .iter()
                .map(|event| event["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["route", "hold_learned", "route"],
            "the hold must be committed before reserving the retry"
        );
        let usage: Vec<Value> = fs::read_to_string(self.paths.usage_file())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0]["profile"], "a");
        assert_eq!(usage[0]["exit_code"], 1);
        assert_eq!(usage[1]["profile"], "b");
        assert_eq!(usage[1]["exit_code"], 0);
        let listing = self.command(&["ls"]).output().unwrap();
        assert_success(&listing);
        let listing = String::from_utf8(listing.stdout).unwrap();
        assert!(
            listing.lines().any(|line| (line.starts_with("a ")
                || line.starts_with(&format!("{}/a ", self.tool)))
                && line.contains("limited")),
            "{listing}"
        );
        self.print_evidence(output);
        for event in &events {
            println!("rtr log -j: {event}");
        }
        println!("rtr ls: {listing}");
        // Re-reading the same rejected transcript must not emit another hold.
        let after = self.json(&["log", "-j", "-n", "100"]);
        assert_eq!(
            after.iter().filter(|e| e["kind"] == "hold_learned").count(),
            1
        );
    }

    fn print_evidence(&self, output: &Output) {
        println!("{} fixture exit={:?}", self.tool, output.status.code());
        for attempt in 1..=self.attempts() {
            println!(
                "attempt {attempt}: home={} cwd={} argv={:?}",
                self.record(attempt, "home"),
                self.record(attempt, "cwd"),
                self.argv(attempt)
            );
        }
        println!("stdout: {:?}", String::from_utf8_lossy(&output.stdout));
        println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }

    fn seed_limit(&self, profile: &str, id: &str, reset: DateTime<Utc>) {
        self.seed_transcript(profile, id, self.rows(Evidence::Rejection, true, reset));
    }

    fn seed_transcript(&self, profile: &str, id: &str, rows: Vec<Value>) {
        let relative = if self.tool == "claude" {
            format!("projects/fixture/{id}.jsonl")
        } else {
            format!("sessions/rollout-{id}.jsonl")
        };
        let path = self
            .paths
            .profile_home_dir(self.tool, profile)
            .join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        let timestamp = Utc::now().to_rfc3339();
        for row in rows {
            writeln!(
                file,
                "{}",
                row.to_string()
                    .replace("@ID@", id)
                    .replace("@TIMESTAMP@", &timestamp)
                    .replace("@SOURCE@", "exec")
            )
            .unwrap();
        }
    }
}

fn assert_success(output: &Output) {
    assert_status(output, 0);
}

fn assert_status(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn claude_print_retries_with_fresh_native_ids_and_identical_arguments() {
    for print in ["-p", "--print"] {
        let f = Fixture::new("claude", Evidence::Rejection);
        let argv = [
            print,
            "literal '$()' prompt\nwith whitespace",
            "--model",
            "fixture-model",
            "",
        ];
        let output = f.launch(&argv).output().unwrap();
        assert_success(&output);
        assert_eq!(output.stdout, b"OK\n");
        f.assert_launch_records(&["a", "b"]);
        let first = f.argv(1);
        let second = f.argv(2);
        for args in [&first, &second] {
            assert_eq!(args[0], "--session-id");
            assert!(uuid::Uuid::parse_str(&args[1]).is_ok());
            assert_eq!(&args[2..], &argv);
        }
        assert_ne!(first[1], second[1]);
        f.assert_retry_log_and_listing(&output);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("weekly limit"), "{stderr}");
        assert!(
            stderr.contains(&f.reset.format("%b %-d %H:%M").to_string()),
            "{stderr}"
        );
        assert!(!stderr.contains("resume: rtr claude -p a"), "{stderr}");
    }
}

#[test]
fn supplied_claude_session_id_pins_first_home_and_qualifies_fork_hint() {
    let f = Fixture::new("claude", Evidence::Rejection);
    // Native IDs are home-owned. A sibling carrying this same ID makes a bare
    // fork selector ambiguous, so the printed continuation must name profile a.
    f.seed_transcript("b", FIRST_ID, f.rows(Evidence::Rejection, false, f.reset));
    let argv = ["--session-id", FIRST_ID, "--print", "fixture"];
    let output = f.launch(&argv).output().unwrap();
    assert_status(&output, 1);
    f.assert_launch_records(&["a"]);
    assert_eq!(f.argv(1), argv);
    assert_eq!(f.record(1, "id"), FIRST_ID);
    f.assert_hold("a");
    f.print_evidence(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("rtr fork {FIRST_ID} --tool claude -p a")),
        "{stderr}"
    );
    let events = f.json(&["log", "-j", "-n", "100"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "route")
            .count(),
        1
    );
    let preview = f
        .command(&[
            "fork",
            FIRST_ID,
            "--tool",
            "claude",
            "-p",
            "a",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert_success(&preview);
    let plan: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(plan["destination"]["profile"], "b");
    f.assert_launch_records(&["a"]);
}

#[test]
fn codex_exec_retries_from_native_startup_id_and_preserves_arguments() {
    for mut argv in [
        vec!["exec"],
        vec!["exec", "--json"],
        vec!["e"],
        vec!["exec", "--experimental-json"],
        vec!["exec", "-o", "resume"],
    ] {
        let f = Fixture::new("codex", Evidence::Rejection);
        let json = argv
            .iter()
            .any(|arg| matches!(*arg, "--json" | "--experimental-json"));
        argv.extend([
            "--model",
            "fixture-model",
            "literal '$()' prompt\nwith whitespace",
            "",
        ]);
        let output = f.launch(&argv).output().unwrap();
        assert_success(&output);
        f.assert_launch_records(&["a", "b"]);
        assert_eq!(f.argv(1), argv);
        assert_eq!(f.argv(2), argv);
        if json {
            let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                events[0],
                json!({"type":"thread.started","thread_id":FIRST_ID})
            );
            assert_eq!(
                events[1],
                json!({"type":"thread.started","thread_id":SECOND_ID})
            );
            assert_eq!(events[2]["item"]["text"], "OK");
        } else {
            assert_eq!(output.stdout, b"OK\n");
            let stderr = String::from_utf8_lossy(&output.stderr);
            for id in [FIRST_ID, SECOND_ID] {
                assert!(
                    stderr.contains(&format!(
                        "OpenAI Codex v0.169.0\n--------\nsession id: {id}\n--------\n"
                    )),
                    "{stderr}"
                );
            }
        }
        f.assert_retry_log_and_listing(&output);
    }
}

#[test]
fn interactive_limit_learns_hold_and_prints_exact_fork_without_relaunch() {
    for (tool, changed_cwd) in [("claude", false), ("codex", false), ("codex", true)] {
        let f = Fixture::new(tool, Evidence::Rejection);
        let mut argv = vec!["--model", "fixture-model"];
        if changed_cwd {
            let native_cwd = f.cwd.join("other directory");
            fs::create_dir(&native_cwd).unwrap();
            // Codex resolves --cd inside the native process; its rollout owns
            // that absolute cwd even though RTR was invoked in the parent.
            let mut rows = f.rows(Evidence::Rejection, true, f.reset);
            rows[0]["payload"]["cwd"] = json!(fs::canonicalize(native_cwd).unwrap());
            f.template("reject.jsonl", rows);
            argv.extend(["--cd", "other directory"]);
        }
        let output = f.launch(&argv).output().unwrap();
        assert_status(&output, 1);
        f.assert_launch_records(&["a"]);
        f.assert_hold("a");
        f.print_evidence(&output);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains(&format!("rtr fork {FIRST_ID} --tool {tool}")),
            "{stderr}"
        );
        assert!(
            stderr.contains(&f.reset.format("%b %-d %H:%M").to_string()),
            "{stderr}"
        );
        assert!(!stderr.contains("resume: rtr"), "{stderr}");
        let events = f.json(&["log", "-j", "-n", "100"]);
        assert_eq!(events.iter().filter(|e| e["kind"] == "route").count(), 1);
        assert_eq!(
            events
                .iter()
                .filter(|e| e["kind"] == "hold_learned")
                .count(),
            1
        );
    }
}

#[test]
fn ambiguous_interactive_codex_learns_hold_without_fork_hint_or_relaunch() {
    let f = Fixture::new("codex", Evidence::Rejection);
    let output = f
        .launch(&[])
        .env("RTR_TEST_AMBIGUOUS", "true")
        .output()
        .unwrap();
    assert_status(&output, 1);
    f.assert_launch_records(&["a"]);
    assert!(f.argv(1).is_empty());
    f.assert_hold("a");
    f.print_evidence(&output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("usage limit"), "{stderr}");
    assert!(!stderr.contains("rtr fork "), "{stderr}");
    assert!(!stderr.contains("resume: rtr"), "{stderr}");
}

#[test]
fn model_output_tool_calls_and_previous_turns_prevent_one_shot_replay() {
    for tool in ["claude", "codex"] {
        for evidence in [
            Evidence::ModelOutput,
            Evidence::ToolCall,
            Evidence::PreviousTurn,
        ] {
            let f = Fixture::new(tool, evidence);
            let argv = if tool == "claude" {
                vec!["--print", "fixture"]
            } else {
                vec!["exec", "fixture"]
            };
            let output = f.launch(&argv).output().unwrap();
            assert_status(&output, 1);
            f.assert_launch_records(&["a"]);
            f.assert_hold("a");
            f.print_evidence(&output);
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                stderr.contains(&format!("rtr fork {} --tool {tool}", f.record(1, "id"))),
                "{stderr}"
            );
        }
    }
}

#[test]
fn second_rejection_cannot_start_a_third_attempt() {
    for tool in ["claude", "codex"] {
        let f = Fixture::new(tool, Evidence::Rejection);
        let argv = if tool == "claude" {
            vec!["--print", "fixture"]
        } else {
            vec!["exec", "fixture"]
        };
        let output = f
            .launch(&argv)
            .env("RTR_TEST_REJECTIONS", "3")
            .output()
            .unwrap();
        assert_status(&output, 1);
        f.assert_launch_records(&["a", "b"]);
        f.assert_hold("a");
        f.assert_hold("b");
        f.print_evidence(&output);
        let events = f.json(&["log", "-j", "-n", "100"]);
        assert_eq!(events.iter().filter(|e| e["kind"] == "route").count(), 2);
        assert_eq!(
            events
                .iter()
                .filter(|e| e["kind"] == "hold_learned")
                .count(),
            2
        );
    }
}

#[test]
fn every_profile_full_returns_75_with_earliest_reset_without_relaunch() {
    for tool in ["claude", "codex"] {
        let f = Fixture::new(tool, Evidence::Rejection);
        let earliest = f.reset - Duration::hours(1);
        f.seed_limit("b", SECOND_ID, earliest);
        f.seed_limit("c", OTHER_ID, f.reset + Duration::hours(1));
        let argv = if tool == "claude" {
            vec!["--print", "fixture"]
        } else {
            vec!["exec", "fixture"]
        };
        let output = f.launch(&argv).output().unwrap();
        assert_status(&output, 75);
        f.assert_launch_records(&["a"]);
        f.assert_hold("a");
        f.print_evidence(&output);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains(&format!(
                "earliest reset {}",
                earliest.format("%b %-d %H:%M")
            )),
            "{stderr}"
        );
        assert!(!stderr.contains("rtr fork "), "{stderr}");
        let events = f.json(&["log", "-j", "-n", "100"]);
        let unavailable = events.iter().find(|e| e["kind"] == "unavailable").unwrap();
        assert_eq!(unavailable["earliest_reset"], json!(earliest));
    }
}

#[test]
fn consumed_pipe_stdin_cannot_authorize_an_automatic_retry() {
    for tool in ["claude", "codex"] {
        let f = Fixture::new(tool, Evidence::Rejection);
        let argv = if tool == "claude" {
            vec!["--print", "fixture"]
        } else {
            vec!["exec", "fixture"]
        };
        let mut child = f
            .launch(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"one-shot input")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_status(&output, 1);
        f.assert_launch_records(&["a"]);
        f.assert_hold("a");
        assert_eq!(f.record(1, "stdin"), "one-shot input");
        f.print_evidence(&output);
    }
}

#[test]
fn codex_json_relay_returns_numeric_141_when_stdout_reader_closes() {
    let f = Fixture::new("codex", Evidence::Rejection);
    // A single small startup write succeeds into RTR's own pipe. Closing the
    // downstream reader first isolates the relay's EPIPE from native failures.
    fs::write(
        f.temp.path().join("record.sh"),
        format!(
            "sleep 0.1\nprintf '%s\\n' '{}'\n",
            json!({"type":"thread.started","thread_id":FIRST_ID})
        ),
    )
    .unwrap();
    let mut child = f
        .launch(&["exec", "--json", "fixture"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert_status(&output, 141);
    println!("codex closed stdout: exit={:?}", output.status.code());
}
