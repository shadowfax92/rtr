//! Process-level checks with synthetic caches, histories, and a shell child.
//! RTR_TEST_CARGO_RUN drives the same checks through real cargo run invocations.

use chrono::{DateTime, Duration, Utc};
use rtr::{paths::Paths, state::State};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const PROFILES: &[&str] = &["eng", "eng2", "nik", "nit", "ops", "res"];
const FABLE: &str = "claude-fable-5-2";
const OPUS: &str = "claude-opus-5-5";

struct Fixture {
    root: tempfile::TempDir,
    paths: Paths,
    config: String,
    now: DateTime<Utc>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: root.path().join("config"),
            state_dir: root.path().join("state"),
        };
        fs::create_dir(&paths.config_dir).unwrap();
        let script = root.path().join("record.sh");
        fs::write(
            &script,
            "printf '%s\\n' \"${CODEX_HOME:-${CLAUDE_CONFIG_DIR:-native-default}}\" \"$@\"\n",
        )
        .unwrap();
        let mut config = format!(
            "[tools.claude]\ncommand=['sh',{:?}]\nargs=['--model','{OPUS}']\ncopy=[]\n",
            script.to_str().unwrap()
        );
        for profile in PROFILES {
            config.push_str(&format!("[tools.claude.profiles.{profile}]\n"));
            if *profile == "eng" {
                config.push_str("share_percent=10\n");
            }
        }
        config.push_str(&format!(
            "[tools.codex]\ncommand=['sh',{:?}]\ncopy=[]\n",
            script.to_str().unwrap()
        ));
        // Manual acceptance can start from a temporary copy of the owner's
        // RTR config. Replace the entire fixture tool policy before using it.
        if let Some(source) = std::env::var_os("RTR_TEST_CONFIG_COPY") {
            fs::copy(source, paths.config_file()).unwrap();
        }
        fs::write(paths.config_file(), &config).unwrap();
        Self {
            root,
            paths,
            config,
            now: Utc::now(),
        }
    }

    fn configure(&self, config: &str) {
        fs::write(self.paths.config_file(), config).unwrap();
    }

    fn home(&self, profile: &str) -> PathBuf {
        self.paths.profile_home_dir("claude", profile)
    }

    fn current(
        &self,
        profile: &str,
        short: Option<(u8, i64)>,
        common: (u8, i64),
        fable: Option<(u8, i64)>,
    ) {
        self.meters(profile, self.now, short, common, fable);
    }

    fn meters(
        &self,
        profile: &str,
        at: DateTime<Utc>,
        short: Option<(u8, i64)>,
        common: (u8, i64),
        fable: Option<(u8, i64)>,
    ) {
        let reset = |hours| self.now + Duration::hours(hours);
        let meter = |(used, hours)| json!({"utilization": used, "resets_at": reset(hours)});
        let mut limits = Vec::new();
        if let Some((used, hours)) = fable {
            limits.push(json!({
                "kind": "weekly_scoped",
                "percent": used,
                "resets_at": reset(hours),
                "scope": {"model": {"id": null, "display_name": "Fable"}}
            }));
        }
        let snapshot = json!({"cachedUsageUtilization": {
            "fetchedAtMs": at.timestamp_millis(),
            "utilization": {
                "five_hour": short.map(meter),
                "seven_day": meter(common),
                "limits": limits,
            }
        }});
        fs::create_dir_all(self.home(profile)).unwrap();
        fs::write(
            self.home(profile).join(".claude.json"),
            snapshot.to_string(),
        )
        .unwrap();
    }

    fn source(&self) {
        let folder = self.home("eng").join("projects/test");
        fs::create_dir_all(&folder).unwrap();
        let transcript = folder.join("fixture-source.jsonl");
        fs::write(&transcript, "").unwrap();
        // The original turn predates the meter snapshot. A newer owned turn
        // would be recovery evidence and could clear the fixture's blocker.
        let timestamp = self.now - Duration::minutes(30);
        let user = json!({"type":"user","sessionId":"fixture-source","uuid":"u","cwd":self.root.path(),"timestamp":timestamp,"message":{"role":"user","content":"fixture prompt"}});
        let assistant = json!({"type":"assistant","sessionId":"fixture-source","uuid":"a","parentUuid":"u","timestamp":timestamp,"message":{"model":FABLE,"content":[{"type":"text","text":"fixture response"}]},"effort":"high"});
        fs::write(transcript, format!("{user}\n{assistant}\n")).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = if std::env::var_os("RTR_TEST_CARGO_RUN").is_some() {
            let mut cargo = Command::new("cargo");
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            for (key, fallback) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
                if std::env::var_os(key).is_none() {
                    cargo.env(key, home.join(fallback));
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
            .current_dir(self.root.path())
            .env("RTR_STATE_DIR", &self.paths.state_dir)
            .env("RTR_CONFIG_DIR", &self.paths.config_dir)
            .env("HOME", self.root.path())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .env_remove("CODEX_HOME");
        command
    }

    fn run(&self, args: &[&str], status: i32) -> Output {
        let output = self.command(args).output().unwrap();
        assert_status(&output, status, args);
        output
    }

    fn logs(&self) -> Vec<Value> {
        serde_json::from_slice(&self.run(&["log", "-j", "-n", "300"], 0).stdout).unwrap()
    }

    fn preview(&self, status: i32) -> Value {
        let output = self.run(
            &[
                "fork",
                "fixture-source",
                "-t",
                "claude",
                "--no-picker",
                "-n",
                "-j",
            ],
            status,
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn state(&self) -> State {
        State::load(&self.paths.state_file()).unwrap()
    }
}

fn assert_status(output: &Output, status: i32, args: &[&str]) {
    assert_eq!(
        output.status.code(),
        Some(status),
        "{args:?}\n{}",
        stderr(output)
    );
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn argv(output: &Output) -> Vec<&str> {
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .collect()
}

fn profile(output: &Output) -> String {
    Path::new(argv(output)[0])
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

fn tree(path: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut result = BTreeMap::new();
    if path.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap().path();
            if entry.is_dir() {
                result.insert(entry.clone(), None);
                result.extend(tree(&entry));
            } else {
                result.insert(entry.clone(), Some(fs::read(entry).unwrap()));
            }
        }
    }
    result
}

#[test]
fn six_home_weights_hold_for_full_and_excluded_pools_with_concurrent_reservations() {
    for (blocked, runs) in [(false, 100), (true, 82)] {
        let fixture = Fixture::new();
        for &name in PROFILES {
            fixture.current(
                name,
                None,
                (43, 60),
                Some((if blocked && name == "nik" { 100 } else { 56 }, 20)),
            );
        }
        // Overlap four CLI processes at a time. Each child writes only to its
        // own pipe so a concurrent choice cannot overwrite the evidence.
        let args = ["claude", "--model", FABLE];
        let mut counts = BTreeMap::new();
        for start in (0..runs).step_by(4) {
            let children: Vec<_> = (start..runs.min(start + 4))
                .map(|_| {
                    fixture
                        .command(&args)
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap()
                })
                .collect();
            for child in children {
                let output = child.wait_with_output().unwrap();
                assert_status(&output, 0, &args);
                assert_eq!(&argv(&output)[1..], ["--model", FABLE]);
                *counts.entry(profile(&output)).or_insert(0) += 1;
            }
        }
        assert_eq!(counts["eng"], 10);
        for &name in &PROFILES[1..] {
            assert_eq!(
                counts.get(name).copied().unwrap_or(0),
                if blocked && name == "nik" { 0 } else { 18 }
            );
        }
        let state = fixture.state();
        let rotation = &state.weighted["claude"];
        assert_eq!(rotation.weights.len(), 6);
        assert!(rotation.scores.values().all(|score| *score == 0));
        let logs = fixture.logs();
        assert_eq!(logs.len(), runs);
        assert!(logs
            .iter()
            .all(|event| event["kind"] == "route" && event["effective_model"] == FABLE));
        assert_eq!(
            fs::read_to_string(fixture.paths.state_dir.join("usage.jsonl"))
                .unwrap()
                .lines()
                .count(),
            runs
        );
        eprintln!("allocation over {runs} launches: {counts:?}; {runs} route and child records");
    }
}

#[test]
fn alternating_model_pools_freeze_excluded_scores_and_keep_unknown_profiles() {
    let fixture = Fixture::new();
    let launch = |model| fixture.run(&["claude", "--model", model], 0);
    assert_eq!(profile(&launch(FABLE)), "eng2");
    fixture.current("nik", None, (43, 60), Some((100, 20)));
    fixture.meters(
        "ops",
        fixture.now - Duration::hours(7),
        None,
        (100, -1),
        Some((100, -1)),
    );
    fixture.meters(
        "res",
        fixture.now - Duration::hours(6),
        None,
        (99, 20),
        Some((99, 20)),
    );
    assert_eq!(profile(&launch(FABLE)), "nit");
    let state = fixture.state();
    let rotation = &state.weighted["claude"];
    assert_eq!(rotation.scores["nik"], 90);
    assert_eq!(rotation.scores["eng"], 100);
    assert_eq!(rotation.scores["nit"], -230);
    assert_eq!(profile(&launch(OPUS)), "ops");
    let state = fixture.state();
    assert_eq!(state.weighted["claude"].scores["nik"], 180);
    let logs = fixture.logs();
    assert_eq!(logs[1]["blocked"], 1);
    assert_eq!(logs[2]["blocked"], 0);
    assert!(logs[2]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["reason"] == "unknown"));
    eprintln!(
        "Fable picked eng2 then nit; Opus picked ops; nik score froze at 90 then reached 180"
    );
}

#[test]
fn fallback_commits_one_route_and_herdr_fork_reports_the_new_native_id() {
    let fixture = Fixture::new();
    fixture.source();
    for &name in PROFILES {
        fixture.current(
            name,
            None,
            (if name == "nik" { 32 } else { 100 }, 60),
            Some((100, 20)),
        );
    }
    let launch = fixture.run(
        &[
            "claude",
            "--model=claude-fable-5-2",
            "--",
            "--",
            "--model",
            FABLE,
        ],
        0,
    );
    let args = argv(&launch);
    assert_eq!(profile(&launch), "nik");
    assert_eq!(
        &args[1..],
        ["--model=claude-opus-5-5", "--", "--model", FABLE]
    );
    assert_eq!(stderr(&launch).matches("Fable unavailable").count(), 1);
    let report = fixture.root.path().join("report.json");
    let before = tree(&fixture.paths.state_dir);
    let preview = fixture.preview(0);
    assert_eq!(preview["model"], OPUS);
    assert_eq!(preview["destination"]["profile"], "nik");
    assert_eq!(tree(&fixture.paths.state_dir), before);
    let fork = fixture.run(
        &[
            "fork",
            "fixture-source",
            "--tool",
            "claude",
            "--no-picker",
            "--report",
            report.to_str().unwrap(),
        ],
        0,
    );
    let report: Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
    assert_eq!(report["destination"], preview["destination"]);
    assert_eq!(report["model"], preview["model"]);
    assert_eq!(report["effort"], "high");
    assert_eq!(stderr(&fork).matches("Fable unavailable").count(), 1);
    let id = report["id"].as_str().unwrap();
    assert_ne!(id, "fixture-source");
    assert!(uuid::Uuid::parse_str(id).is_ok());
    assert!(argv(&fork).contains(&id));
    assert!(argv(&fork).contains(&OPUS));
    assert_eq!(profile(&fork), "nik");
    let session = fixture.run(&["session", id, "-t", "claude", "-p", "nik", "-j"], 0);
    let session: Value = serde_json::from_slice(&session.stdout).unwrap();
    assert_eq!(session["id"], id);
    assert_eq!(session["profile"], "nik");
    assert!(session["limit"].is_null());

    let herdr = fixture.run(&["fork", "fixture-source", "--tool", "claude"], 0);
    assert_eq!(profile(&herdr), "nik");
    assert_eq!(stderr(&herdr).matches("Fable unavailable").count(), 1);

    let logs = fixture.logs();
    assert_eq!(
        logs.iter()
            .map(|row| row["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "model_drop",
            "route",
            "model_drop",
            "route",
            "fork_prepared",
            "model_drop",
            "route",
            "fork_prepared"
        ]
    );
    assert_eq!(logs[0]["blocked"], 6);
    assert_eq!(logs[0]["requested_model"], FABLE);
    assert_eq!(logs[1]["effective_model"], OPUS);
    assert_eq!(logs[3]["operation_id"], logs[4]["operation_id"]);
    assert_eq!(logs[4]["source"], "eng");
    assert_eq!(logs[4]["destination"], "nik");
    assert_eq!(logs[4]["prepared_id"], id);
    assert!(fixture.state().weighted["claude"]
        .scores
        .values()
        .all(|score| *score == 0));

    fixture.configure(&format!(
        "[routing.claude]\nauto_drop=false\n{}",
        fixture.config
    ));
    let strict = fixture.preview(75);
    assert_eq!(
        strict["earliest_reset"],
        json!(fixture.now + Duration::hours(20))
    );
    let before = fs::read(fixture.paths.state_file()).unwrap();
    let unavailable = fixture.run(&["claude", "--model", FABLE], 75);
    assert!(!stderr(&unavailable).contains("Fable unavailable"));
    assert!(unavailable.stdout.is_empty());
    assert_eq!(fs::read(fixture.paths.state_file()).unwrap(), before);
    assert_eq!(fixture.logs().last().unwrap()["kind"], "unavailable");

    fixture.configure(&fixture.config);
    fixture.meters(
        "nik",
        fixture.now - Duration::hours(7),
        None,
        (32, 60),
        Some((100, -1)),
    );
    let unknown = fixture.run(&["claude", "--model", FABLE], 0);
    assert_eq!(profile(&unknown), "nik");
    assert!(argv(&unknown).contains(&FABLE));
    assert!(!stderr(&unknown).contains("Fable unavailable"));
    eprintln!("Fable dropped once per handoff; dry run and report agree; strict mode exited 75; expired Fable stayed eligible");
}

#[test]
fn unavailable_uses_latest_simultaneous_release_and_excludes_manual_launch_profiles() {
    let fixture = Fixture::new();
    let config = fixture
        .config
        .replace("share_percent=10", "share_percent=0");
    fixture.configure(&config);
    for &name in PROFILES {
        let (short, common, fable) = match name {
            "eng" => (1, 2, 3),
            "nik" => (8, 6, 1),
            _ => (12, 16, 4),
        };
        fixture.current(name, Some((100, short)), (100, common), Some((100, fable)));
    }
    let output = fixture.run(&["claude", "--model", FABLE], 75);
    assert!(output.stdout.is_empty());
    assert!(!fixture.paths.state_file().exists());
    let logs = fixture.logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["kind"], "unavailable");
    assert_eq!(logs[0]["blocked"], 5);
    assert_eq!(
        logs[0]["earliest_reset"],
        json!(fixture.now + Duration::hours(8))
    );
    fixture.source();
    let fork = fixture.preview(75);
    assert_eq!(
        fork["earliest_reset"],
        json!(fixture.now + Duration::hours(2))
    );
    eprintln!("common blockers prevent an Opus handoff; overlapping windows yield an 8h launch retry and a 2h fork retry");
}

#[test]
fn in_place_forks_report_a_resumable_native_id() {
    for (tool, model, resume) in [
        ("claude", OPUS, "--resume"),
        ("codex", "gpt-test", "resume"),
    ] {
        let fixture = Fixture::new();
        if tool == "claude" {
            fixture.source();
            for &name in PROFILES {
                let common_used = if name == "eng" { 20 } else { 100 };
                fixture.current(name, None, (common_used, 60), Some((100, 20)));
            }
        } else {
            fixture.configure(&format!(
                "{}[tools.codex.profiles.eng]\n[tools.codex.profiles.nik]\n",
                fixture.config
            ));
            for (profile, id, row) in [
                (
                    "eng",
                    "fixture-source",
                    json!({"type":"turn_context","payload":{"model":model,"turn_id":"turn"}}),
                ),
                (
                    "nik",
                    "limited",
                    json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":100,"window_minutes":300,"resets_at":(fixture.now + Duration::hours(2)).timestamp()}}}}),
                ),
            ] {
                let folder = fixture
                    .paths
                    .profile_home_dir(tool, profile)
                    .join("sessions");
                fs::create_dir_all(&folder).unwrap();
                let head = json!({"timestamp":fixture.now,"type":"session_meta","payload":{"id":id,"cwd":fixture.root.path(),"history_mode":"legacy"}});
                let mut row = row;
                row["timestamp"] = json!(fixture.now);
                fs::write(
                    folder.join(format!("rollout-{id}.jsonl")),
                    format!("{head}\n{row}\n"),
                )
                .unwrap();
            }
        }

        let before = tree(&fixture.paths.state_dir);
        let preview = fixture.run(
            &[
                "fork",
                "fixture-source",
                "--tool",
                tool,
                "--no-picker",
                "--dry-run",
                "--json",
            ],
            0,
        );
        let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
        assert_eq!(preview["destination"], json!({"tool":tool,"profile":"eng"}));
        assert_eq!(preview["in_place"], true);
        assert_eq!(preview["model"], model);
        assert_eq!(tree(&fixture.paths.state_dir), before);

        let report = fixture.root.path().join("report.json");
        let fork = fixture.run(
            &[
                "fork",
                "fixture-source",
                "--tool",
                tool,
                "--no-picker",
                "--report",
                report.to_str().unwrap(),
            ],
            0,
        );
        let report: Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        for field in [
            "destination",
            "in_place",
            "model",
            "effort",
            "schema_version",
        ] {
            assert_eq!(report[field], preview[field]);
        }
        let id = report["id"]
            .as_str()
            .expect("report must include the prepared native ID");
        assert!(uuid::Uuid::parse_str(id).is_ok());
        assert_eq!(
            argv(&fork)[0],
            fixture
                .paths
                .profile_home_dir(tool, "eng")
                .to_str()
                .unwrap()
        );
        assert!(argv(&fork).windows(2).any(|args| args == [resume, id]));
        let logs = fixture.logs();
        let event = logs
            .iter()
            .find(|row| row["kind"] == "fork_prepared")
            .unwrap();
        assert_eq!(event["in_place"], true);
        assert_eq!(event["prepared_id"], id);
        let session = fixture.run(&["session", id, "--tool", tool, "--json"], 0);
        let session: Value = serde_json::from_slice(&session.stdout).unwrap();
        assert_eq!(session["id"], id);
        assert_eq!(session["tool"], tool);
        assert_eq!(session["profile"], "eng");
        eprintln!("{tool}: preview wrote nothing; report, {resume} argv, log, and session lookup agree on {id} in source profile eng");
    }
}

#[test]
fn fork_prefers_requested_model_in_place_and_can_rotate_to_zero_share() {
    let fixture = Fixture::new();
    fixture.source();
    for &name in &PROFILES[1..] {
        fixture.current(name, None, (20, 60), Some((100, 20)));
    }
    let plan = fixture.preview(0);
    assert_eq!(plan["model"], FABLE);
    assert_eq!(plan["destination"]["profile"], "eng");
    assert_eq!(plan["in_place"], true);
    let report = fixture.root.path().join("report");
    let fork = fixture.run(
        &[
            "fork",
            "fixture-source",
            "--report",
            report.to_str().unwrap(),
        ],
        0,
    );
    assert!(stderr(&fork).contains("no other profile has room for claude-fable-5-2"));
    let prepared: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    let id = prepared["id"].as_str().unwrap();
    assert!(argv(&fork).windows(2).any(|args| args == ["--resume", id]));
    assert_eq!(fixture.logs()[1]["prepared_id"], id);

    let config = fixture
        .config
        .replace("share_percent=10", "share_percent=100");
    fixture.configure(&config);
    fixture.current("nik", None, (20, 60), Some((10, 20)));
    let alternate = fixture.preview(0);
    assert_eq!(alternate["destination"]["profile"], "nik");
    assert_eq!(alternate["in_place"], false);
    let disabled = config.replace("share_percent=100", "enabled=false\nbypass=true");
    fixture.configure(&disabled);
    assert_eq!(fixture.preview(0)["destination"]["profile"], "nik");

    fixture.configure(&fixture.config);
    fixture.current("eng", None, (100, 60), Some((5, 20)));
    let full_source = fixture.run(&["fork", "fixture-source", "--tool", "claude"], 0);
    assert_eq!(profile(&full_source), "nik");
    assert!(argv(&full_source).contains(&FABLE));
    eprintln!("Fable source stays in place before Opus; zero-share and disabled-source forks select the eligible alternative");
}

#[test]
fn fork_preparation_and_history_copy_fail_on_opposite_sides_of_reservation() {
    let fixture = Fixture::new();
    fixture.source();
    let report = fixture.root.path().join("report");
    let args = [
        "fork",
        "fixture-source",
        "--no-picker",
        "--report",
        report.to_str().unwrap(),
    ];
    let bad = fixture.config.replace(
        "copy=[]",
        "copy=[{source='missing-fixture',destination='settings.json'}]",
    );
    fixture.configure(&bad);
    let failure = fixture.run(&args, 1);
    assert!(stderr(&failure).contains("does not exist"));
    assert!(!report.exists());
    assert!(!fixture.paths.state_file().exists());
    assert!(fixture.logs().is_empty());

    fixture.configure(&fixture.config);
    fs::create_dir(fixture.paths.state_dir.join("fork-origins.json.lock")).unwrap();
    let failure = fixture.run(&args, 1);
    assert!(stderr(&failure).contains("fork-origins.json.lock"));
    assert!(!report.exists());
    assert!(fixture.paths.state_file().exists());
    let logs = fixture.logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["kind"], "route");
    assert_eq!(logs[0]["destination"], "eng2");
    assert_eq!(fixture.preview(0)["destination"]["profile"], "nik");
    assert!(!fixture.paths.state_dir.join("usage.jsonl").exists());
    eprintln!("home preparation failure spends no slot; copy failure leaves one route, no report, and no child outcome");
}

#[test]
fn pinned_launch_fork_and_resume_keep_identity_and_log_failures_allow_handoff() {
    let fixture = Fixture::new();
    fixture.source();
    fixture.current("eng", None, (20, 60), Some((100, 20)));
    fixture.current("nik", None, (100, 60), Some((100, 20)));

    let pinned = fixture.run(&["claude", "-p", "nik", "--model", FABLE], 0);
    assert_eq!(profile(&pinned), "nik");
    assert_eq!(&argv(&pinned)[1..], ["--model", FABLE]);
    assert!(stderr(&pinned).contains("weekly Fable"));

    let fork = fixture.run(&["fork", "fixture-source", "-d", "nik"], 0);
    assert_eq!(profile(&fork), "nik");
    assert!(argv(&fork).contains(&FABLE));
    assert!(stderr(&fork).contains("weekly Fable"));

    let config = fixture
        .config
        .replace("share_percent=10", "enabled=false\nbypass=true");
    fixture.configure(&config);
    let quota = fixture.paths.state_dir.join("quota.json");
    let before = fs::read(&quota).ok();
    let resumed = fixture.run(&["resume", "fixture-source", "-t", "claude"], 0);
    assert_eq!(profile(&resumed), "eng");
    assert!(argv(&resumed).contains(&FABLE));
    assert!(argv(&resumed).contains(&"fixture-source"));
    assert!(stderr(&resumed).contains("rtr fork fixture-source -t claude -p eng"));
    assert_eq!(fs::read(&quota).ok(), before);
    assert!(!fixture.paths.state_file().exists());

    let config = fixture
        .config
        .replace("share_percent=10", "share_percent=10\nbypass=true");
    fixture.configure(&config);
    let row = json!({"entries":{"claude":{"eng":{"limit":{"kind":"weekly","active":true,"message":"fixture quota","at":fixture.now,"resets_at":fixture.now+Duration::hours(2)}}}}});
    fs::write(&quota, row.to_string()).unwrap();
    let bypass = fixture.run(&["claude", "-p", "eng", "--model", FABLE], 0);
    assert_eq!(argv(&bypass)[0], "native-default");
    assert!(!stderr(&bypass).contains("limited"));
    let list = fixture.run(&["ls", "--color", "never"], 0);
    assert!(String::from_utf8_lossy(&list.stdout)
        .to_ascii_lowercase()
        .contains("fable"));
    let logs = fixture.logs();
    assert!(logs
        .iter()
        .any(|event| event["operation"] == "resume" && event["destination"] == "eng"));
    assert!(logs.iter().any(|event| event["operation"] == "launch"
        && event["destination"] == "eng"
        && event["blocked"] == 0));

    let path = fixture.paths.state_dir.join("routing.jsonl");
    fs::remove_file(&path).unwrap();
    fs::create_dir(path).unwrap();
    let output = fixture.run(&["claude", "-p", "res", "--model", FABLE], 0);
    assert_eq!(profile(&output), "res");
    assert!(stderr(&output).contains("routing log"));
    assert_eq!(
        fs::read_to_string(fixture.paths.state_dir.join("usage.jsonl"))
            .unwrap()
            .lines()
            .count(),
        5
    );
    assert!(!fixture.paths.state_file().exists());
    eprintln!("pinned, fork, and resume preserved native models and homes; bypass ignored its hold; log failure still produced the fifth child outcome");
}
