//! Contract tests use synthetic history and recording shell children. They never
//! launch Claude/Codex or resolve the owner's live configuration/state paths.
use chrono::{Duration, Utc};
use rtr::{paths::Paths, state::State};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    temp: tempfile::TempDir,
    paths: Paths,
    tool: &'static str,
}
impl Fixture {
    fn new(tool: &'static str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
        };
        fs::create_dir_all(&paths.config_dir).unwrap();
        // TOML literal command keeps shell quoting simple and test-owned.
        let script = temp.path().join("record.sh");
        fs::write(&script, "printf '%s\\n' \"$@\" > \"$RTR_TEST_LOG\"\n").unwrap();
        let config = format!("[tools.{tool}]\ncommand=['sh',{}]\nargs=['--model','config-model']\ncopy=[]\n[tools.{tool}.profiles.a]\n[tools.{tool}.profiles.b]\n[tools.{tool}.profiles.c]\n", toml::Value::String(script.display().to_string()));
        fs::write(paths.config_file(), config).unwrap();
        Self { temp, paths, tool }
    }
    fn path(&self, p: &str, id: &str) -> PathBuf {
        self.paths
            .profile_home_dir(self.tool, p)
            .join(if self.tool == "claude" {
                format!("projects/test/{id}.jsonl")
            } else {
                format!("sessions/rollout-{id}.jsonl")
            })
    }
    fn write(&self, p: &str, id: &str, rows: Vec<Value>) {
        let path = self.path(p, id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();
        self.append(p, id, rows);
    }
    fn append(&self, p: &str, id: &str, rows: Vec<Value>) {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(self.path(p, id))
            .unwrap();
        for mut r in rows {
            if r.get("timestamp").is_none() {
                r["timestamp"] = json!(Utc::now());
            }
            writeln!(f, "{r}").unwrap();
        }
    }
    fn source(&self) {
        let rows = if self.tool == "claude" {
            vec![
                json!({"type":"user","sessionId":"source","uuid":"u","cwd":self.temp.path(),"message":{"role":"user","content":"handwritten fixture"}}),
                json!({"type":"assistant","sessionId":"source","uuid":"a","parentUuid":"u","message":{"model":"source-model","content":[{"type":"text","text":"done"}]},"effort":"high","perTurnEffort":"low"}),
            ]
        } else {
            vec![
                json!({"type":"session_meta","payload":{"id":"source","cwd":self.temp.path(),"history_mode":"legacy"}}),
                json!({"type":"turn_context","payload":{"model":"source-model","effort":"high","turn_id":"turn"}}),
                json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn"}}),
            ]
        };
        self.write("a", "source", rows);
    }
    fn limited(&self, p: &str) {
        let head = if self.tool == "claude" {
            json!({"type":"user","sessionId":"limit","cwd":self.temp.path(),"message":{"role":"user","content":"fixture"}})
        } else {
            json!({"type":"session_meta","payload":{"id":"limit","cwd":self.temp.path(),"history_mode":"legacy"}})
        };
        self.write(p, "limit", vec![head, self.limit_row()]);
    }
    fn limit_row(&self) -> Value {
        if self.tool == "claude" {
            json!({"type":"assistant","apiError":"usage_limit_reached","message":{"model":"<synthetic>","content":[{"text":"usage"}]},"quotaLimits":{"rateLimitType":"seven_day","resetsAt":(Utc::now()+Duration::hours(1)).timestamp()}})
        } else {
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn","error":{"codex_error_info":"usage_limit_exceeded","message":"unknown"}}})
        }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_rtr"));
        c.args(args)
            .env("RTR_CONFIG_DIR", &self.paths.config_dir)
            .env("RTR_STATE_DIR", &self.paths.state_dir)
            .env("RTR_TEST_LOG", self.temp.path().join("launch"))
            .env("HOME", self.temp.path())
            .current_dir(self.temp.path());
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let o = self.run(args);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        o
    }
    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.ok(args).stdout).unwrap()
    }
}

#[test]
fn settings_precedence_for_fork_and_resume_in_both_tools() {
    for tool in ["claude", "codex"] {
        let f = Fixture::new(tool);
        f.source();
        for op in ["resume", "fork"] {
            let mut args = vec![op, "source", "--tool", tool];
            if op == "fork" {
                args.extend(["--to-profile", "a"]);
            }
            f.ok(&args);
            let launch = fs::read_to_string(f.temp.path().join("launch")).unwrap();
            assert!(launch.contains("source-model"));
            assert!(launch.contains("high"));
            assert!(!launch.contains("config-model"));
            args.extend(if tool == "claude" {
                vec!["--", "--model", "explicit", "--effort", "low"]
            } else {
                vec![
                    "--",
                    "-c",
                    "model=\"explicit\"",
                    "-c",
                    "model_reasoning_effort=\"low\"",
                ]
            });
            f.ok(&args);
            let launch = fs::read_to_string(f.temp.path().join("launch")).unwrap();
            assert!(launch.contains("explicit"));
            assert!(launch.contains("low"));
            assert!(!launch.contains("source-model"));
            assert!(!launch.contains("config-model"));
            assert!(!launch.contains("high"));
        }
    }
}

#[test]
fn dry_run_report_copy_in_place_and_unavailable() {
    for tool in ["claude", "codex"] {
        let f = Fixture::new(tool);
        f.source();
        f.limited("b");
        let plan = f.json(&["fork", "source", "--dry-run", "--json"]);
        assert_eq!(plan["destination"]["profile"], "c");
        assert_eq!(plan["model"], "source-model");
        assert!(!f.paths.state_file().exists());
        assert!(!f.paths.state_dir.join("quota.json").exists());
        assert!(!f.temp.path().join("launch").exists());
        let report = f.temp.path().join("report.json");
        let report = report.to_str().unwrap();
        f.ok(&["fork", "source", "--no-picker", "--report", report]);
        let r: Value = serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap();
        assert_eq!(r["destination"]["profile"], "c");
        assert!(r["id"].is_string());
        assert_eq!(r["in_place"], false);
        let copied = f.json(&[
            "session",
            r["id"].as_str().unwrap(),
            "--profile",
            "c",
            "--json",
        ]);
        assert!(copied["limit"].is_null());
        f.ok(&["fork", "source", "--to-profile", "a", "--report", report]);
        let r: Value = serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap();
        assert_eq!(r["in_place"], true);
        assert!(r["id"].is_null());
        f.limited("c");
        let plan = f.json(&["fork", "source", "--dry-run", "--json"]);
        assert_eq!(plan["destination"]["profile"], "a");
        assert_eq!(plan["in_place"], true);
        f.limited("a");
        let before = fs::read(f.paths.state_file()).unwrap();
        let o = f.run(&["fork", "source", "--dry-run", "--json"]);
        assert_eq!(o.status.code(), Some(75));
        assert!(serde_json::from_slice::<Value>(&o.stdout).unwrap()["destination"].is_null());
        assert_eq!(fs::read(f.paths.state_file()).unwrap(), before);
        let o = f.run(&["fork", "source", "--report", report]);
        assert_eq!(o.status.code(), Some(75));
        let r: Value = serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap();
        assert!(r["destination"].is_null());
        assert!(r["earliest_reset"].is_string());
        assert_eq!(f.run(&[tool]).status.code(), Some(75));
        assert_eq!(fs::read(f.paths.state_file()).unwrap(), before);
    }
}

#[test]
fn owned_inherited_and_new_turn_limits() {
    for tool in ["claude", "codex"] {
        let f = Fixture::new(tool);
        f.source();
        f.append("a", "source", vec![f.limit_row()]);
        assert!(f.json(&["session", "source", "--json"])["limit"]["active"]
            .as_bool()
            .unwrap());
        let report = f.temp.path().join("report");
        f.ok(&[
            "fork",
            "source",
            "--to-profile",
            "b",
            "--report",
            report.to_str().unwrap(),
        ]);
        let r: Value = serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap();
        let id = r["id"].as_str().unwrap();
        assert!(f.json(&["session", id, "--json"])["limit"].is_null());
        // Legacy copies must ignore errors that predate file creation.
        let mut old = f.limit_row();
        old["timestamp"] = json!(Utc::now() - Duration::days(1));
        f.write("c", "old-copy", vec![old]);
        assert!(f.json(&["session", "old-copy", "--json"])["limit"].is_null());
        let next = if tool == "claude" {
            json!({"type":"user","message":{"content":"next"}})
        } else {
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"new"}})
        };
        f.append("a", "source", vec![next]);
        assert!(f.json(&["session", "source", "--json"])["limit"].is_null());
    }
}

#[test]
fn success_and_manual_clear_prevent_stale_rediscovery() {
    let f = Fixture::new("claude");
    f.source();
    f.limited("b");
    f.ok(&["session", "limit", "--json"]);
    assert!(String::from_utf8(f.ok(&["ls"]).stdout)
        .unwrap()
        .contains("limited until"));
    f.ok(&["clear-limit", "claude", "--profile", "b"]);
    assert_eq!(
        f.json(&["fork", "source", "--dry-run", "--json"])["destination"]["profile"],
        "b"
    );
    f.append("b", "limit", vec![f.limit_row()]);
    f.ok(&["session", "limit", "--json"]);
    f.append(
        "b",
        "limit",
        vec![json!({"type":"assistant","message":{"model":"model"},"effort":"high"})],
    );
    f.ok(&["session", "limit", "--json"]);
    assert_eq!(
        f.json(&["fork", "source", "--dry-run", "--json"])["destination"]["profile"],
        "b"
    );
}

#[test]
fn unknown_ambiguous_and_missing_selectors_never_open_picker() {
    let f = Fixture::new("claude");
    f.source();
    for args in [
        vec!["fork", "--no-picker"],
        vec!["fork", "unknown", "--no-picker"],
        vec!["session", "unknown", "--json"],
    ] {
        assert_eq!(f.run(&args).status.code(), Some(1));
    }
    fs::create_dir_all(f.path("b", "source").parent().unwrap()).unwrap();
    fs::copy(f.path("a", "source"), f.path("b", "source")).unwrap();
    assert_eq!(
        f.run(&["session", "source", "--json"]).status.code(),
        Some(1)
    );
    assert_eq!(
        f.run(&["fork", "source", "--no-picker"]).status.code(),
        Some(1)
    );
    assert_eq!(
        f.json(&["session", "source", "--profile", "a", "--json"])["profile"],
        "a"
    );
}

#[test]
fn two_processes_merge_limits_without_lost_updates() {
    let f = Fixture::new("claude");
    f.limited("a");
    f.limited("b");
    let mut a = f
        .command(&["session", "limit", "--profile", "a", "--json"])
        .spawn()
        .unwrap();
    let mut b = f
        .command(&["session", "limit", "--profile", "b", "--json"])
        .spawn()
        .unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    let q: Value =
        serde_json::from_str(&fs::read_to_string(f.paths.state_dir.join("quota.json")).unwrap())
            .unwrap();
    assert!(q["entries"]["claude"]["a"]["limit"].is_object());
    assert!(q["entries"]["claude"]["b"]["limit"].is_object());
    let _ = State::load(&f.paths.state_file()).unwrap();
}

#[test]
fn same_turn_window_beats_message_and_stale_windows_are_rejected() {
    let f = Fixture::new("codex");
    f.source();
    let window = json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":100,"resets_at":(Utc::now()+Duration::hours(2)).timestamp()}}}});
    f.append("a","source",vec![json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"new"}}),json!({"type":"turn_context","payload":{"turn_id":"new","model":"source-model","effort":"high"}}),window.clone(),f.limit_row()]);
    let d = f.json(&["session", "source", "--json"]);
    assert!(d["limit"]["resets_at"].is_string());
    f.append("a","source",vec![json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"newer"}}),json!({"type":"turn_context","payload":{"turn_id":"newer","model":"source-model","effort":"high"}}),f.limit_row()]);
    let d = f.json(&["session", "source", "--json"]);
    assert!(d["limit"]["resets_at"].is_null());
}

#[test]
fn inherited_future_error_cannot_exhaust_copy_and_expired_limits_are_inactive() {
    let f = Fixture::new("claude");
    f.source();
    let mut r = f.limit_row();
    r["timestamp"] = json!(Utc::now() + Duration::hours(1));
    f.append("a", "source", vec![r]);
    let report = f.temp.path().join("copy-report");
    f.ok(&[
        "fork",
        "source",
        "--to-profile",
        "b",
        "--report",
        report.to_str().unwrap(),
    ]);
    let r: Value = serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap();
    let id = r["id"].as_str().unwrap();
    assert!(f.json(&["session", id, "--profile", "b", "--json"])["limit"].is_null());
    f.limited("c");
    let mut r = f.limit_row();
    r["quotaLimits"]["resetsAt"] = json!(Utc::now().timestamp() - 60);
    f.append("c", "limit", vec![r]);
    assert_eq!(
        f.json(&["session", "limit", "--profile", "c", "--json"])["limit"]["active"],
        false
    );
}

#[test]
fn unavailable_codex_model_falls_back_to_config_and_explicit_override_wins() {
    let f = Fixture::new("codex");
    f.source();
    let home = f.paths.profile_home_dir("codex", "a");
    fs::write(
        home.join("models_cache.json"),
        json!({"models":[{"slug":"config-model"}]}).to_string(),
    )
    .unwrap();
    f.ok(&["resume", "source"]);
    let launch = fs::read_to_string(f.temp.path().join("launch")).unwrap();
    assert!(launch.contains("config-model"));
    assert!(!launch.contains("source-model"));
    f.ok(&["resume", "source", "--", "--model", "explicit"]);
    assert!(fs::read_to_string(f.temp.path().join("launch"))
        .unwrap()
        .contains("explicit"));
}

#[test]
fn large_first_claude_message_does_not_hide_exact_identity_or_cwd() {
    let f = Fixture::new("claude");
    f.source();
    let mut rows = fs::read_to_string(f.path("a", "source"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect::<Vec<_>>();
    rows[0]["message"]["content"] = json!("x".repeat(256 * 1024));
    rows[1]["cwd"] = json!(f.temp.path());
    f.write("a", "source", rows);
    let facts = f.json(&["session", "source", "--json"]);
    assert_eq!(facts["cwd"], json!(f.temp.path()));
    f.ok(&["fork", "source", "--no-picker", "--to-profile", "a"]);
}

#[test]
fn automatic_launch_returns_75_when_only_zero_share_profiles_remain() {
    let f = Fixture::new("claude");
    f.source();
    let path = f.paths.config_file();
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace(
            "[tools.claude.profiles.a]",
            "[tools.claude.profiles.a]\nshare_percent=100",
        )
        .replace(
            "[tools.claude.profiles.b]",
            "[tools.claude.profiles.b]\nshare_percent=0",
        )
        .replace(
            "[tools.claude.profiles.c]",
            "[tools.claude.profiles.c]\nshare_percent=0",
        );
    fs::write(path, text).unwrap();
    f.limited("a");
    assert_eq!(f.run(&["claude"]).status.code(), Some(75));
    assert!(!f.paths.state_file().exists());
    // Forks retain the existing equal fallback among zero-share alternatives.
    assert_eq!(
        f.json(&["fork", "source", "--dry-run", "--json"])["destination"]["profile"],
        "b"
    );
}

#[test]
fn large_tool_payload_cannot_hide_last_real_turn_settings() {
    let f = Fixture::new("codex");
    f.source();
    f.append("a","source",vec![json!({"type":"response_item","payload":{"type":"function_call_output","output":"x".repeat(5*1024*1024)}}),json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn"}})]);
    let d = f.json(&["session", "source", "--json"]);
    assert_eq!(d["model"], "source-model");
    assert_eq!(d["effort"], "high");
}

#[test]
fn exhausted_pool_checks_blocked_profiles_for_newer_success() {
    let f = Fixture::new("claude");
    f.source();
    for profile in ["a", "b", "c"] {
        f.limited(profile);
        f.ok(&["session", "limit", "--profile", profile, "--json"]);
    }
    f.append(
        "c",
        "limit",
        vec![json!({"type":"assistant","message":{"model":"model"},"effort":"high"})],
    );
    assert_eq!(
        f.json(&["fork", "source", "--dry-run", "--json"])["destination"]["profile"],
        "c"
    );
    assert!(!f.paths.state_file().exists());
}

#[test]
fn explicit_limited_profile_warns_and_obeys_without_rotation() {
    let f = Fixture::new("claude");
    f.limited("b");
    let o = f.ok(&["claude", "--profile", "b"]);
    assert!(String::from_utf8(o.stderr).unwrap().contains("warning:"));
    assert!(f.temp.path().join("launch").exists());
    assert!(!f.paths.state_file().exists());
}

#[test]
fn quota_messages_use_local_time_and_keep_disabled_policy_visible() {
    // Child-specific TZ avoids mutating process-global timezone state while
    // cargo runs tests in parallel. Winter and summer cover both LA offsets.
    for (utc_reset, local_reset) in [
        ("2100-11-14T20:19:07Z", "Nov 14 12:19"),
        ("2100-06-14T19:19:07Z", "Jun 14 12:19"),
    ] {
        let f = Fixture::new("claude");
        f.source();
        let reset = chrono::DateTime::parse_from_rfc3339(utc_reset).unwrap();
        for profile in ["a", "b", "c"] {
            f.limited(profile);
            let mut limit = f.limit_row();
            limit["quotaLimits"]["resetsAt"] = json!(reset.timestamp());
            f.append(profile, "limit", vec![limit]);
        }
        let run = |args: &[&str], tz: &str| f.command(args).env("TZ", tz).output().unwrap();
        let session_args = ["session", "limit", "--profile", "a", "--json"];
        let local_session = run(&session_args, "America/Los_Angeles");
        let utc_session = run(&session_args, "UTC");
        assert!(local_session.status.success());
        assert_eq!(local_session.stdout, utc_session.stdout);
        let session: Value = serde_json::from_slice(&local_session.stdout).unwrap();
        assert_eq!(session["limit"]["resets_at"], utc_reset);
        for profile in ["b", "c"] {
            assert!(run(
                &["session", "limit", "--profile", profile, "--json"],
                "America/Los_Angeles"
            )
            .status
            .success());
        }
        let fork = run(
            &["fork", "source", "--no-picker", "--dry-run", "--json"],
            "America/Los_Angeles",
        );
        assert_eq!(fork.status.code(), Some(75));
        let unavailable: Value = serde_json::from_slice(&fork.stdout).unwrap();
        assert_eq!(unavailable["earliest_reset"], utc_reset);
        assert!(unavailable["limited"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["until"] == utc_reset));
        let report = f.temp.path().join("unavailable.json");
        let failed_report = run(
            &[
                "fork",
                "source",
                "--no-picker",
                "--report",
                report.to_str().unwrap(),
            ],
            "America/Los_Angeles",
        );
        assert_eq!(failed_report.status.code(), Some(75));
        let reported: Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        assert_eq!(reported, unavailable);
        let error = String::from_utf8(fork.stderr).unwrap();
        assert!(
            error.contains(&format!("earliest reset {local_reset}")),
            "{error}"
        );
        let explicit = run(&["claude", "--profile", "a"], "America/Los_Angeles");
        assert!(explicit.status.success());
        let warning = String::from_utf8(explicit.stderr).unwrap();
        assert!(
            warning.contains(&format!("is limited until {local_reset};")),
            "{warning}"
        );

        f.ok(&["disable", "claude", "--profile", "b"]);
        let ls = run(&["ls", "--color", "never"], "America/Los_Angeles");
        assert!(ls.status.success());
        let listing = String::from_utf8(ls.stdout).unwrap();
        let row = listing
            .lines()
            .find(|line| line.split_whitespace().any(|v| v == "b"))
            .unwrap();
        assert!(
            row.contains(&format!("disabled; limited until {local_reset}")),
            "{row}"
        );
    }
}

#[test]
fn resume_warns_about_limits_without_changing_disabled_policy_or_state() {
    for tool in ["claude", "codex"] {
        let f = Fixture::new(tool);
        f.source();
        f.limited("a");
        f.ok(&["disable", tool, "--profile", "a"]);
        // No lookup has learned this limit. Resume must still warn from its
        // read-only check, preserve exact disabled-home identity, and launch.
        let output = f.ok(&["resume", "source", "--tool", tool]);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains(&format!("warning: {tool}/a is limited until ")),
            "{stderr}"
        );
        assert!(f.temp.path().join("launch").exists());
        assert!(!f.paths.state_file().exists());
        assert!(!f.paths.state_dir.join("quota.json").exists());
    }
}

#[test]
fn session_listing_uses_local_time_while_its_json_stays_utc() {
    let f = Fixture::new("claude");
    f.source();
    let mut rows = fs::read_to_string(f.path("a", "source"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for row in &mut rows {
        row["timestamp"] = json!("2026-01-15T02:19:07Z");
    }
    f.write("a", "source", rows);
    let listing = f
        .command(&["sessions", "--list"])
        .env("TZ", "America/Los_Angeles")
        .output()
        .unwrap();
    assert!(listing.status.success());
    let text = String::from_utf8(listing.stdout).unwrap();
    assert!(text.starts_with("2026-01-14 18:19"), "{text}");
    let run_json = |tz: &str| {
        f.command(&["sessions", "--json"])
            .env("TZ", tz)
            .output()
            .unwrap()
    };
    let local = run_json("America/Los_Angeles");
    let utc = run_json("UTC");
    assert!(local.status.success());
    assert_eq!(local.stdout, utc.stdout);
    let json: Value = serde_json::from_slice(&local.stdout).unwrap();
    assert_eq!(
        json["conversations"][0]["updated_at"],
        "2026-01-15T02:19:07+00:00"
    );
}

#[test]
fn unavailable_warning_metadata_cannot_block_resume() {
    let f = Fixture::new("claude");
    f.source();
    let quota = f.paths.state_dir.join("quota.json");
    fs::write(&quota, "invalid metadata").unwrap();
    let output = f.ok(&["resume", "source"]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("could not check usage limit"), "{stderr}");
    assert!(f.temp.path().join("launch").exists());
    assert_eq!(fs::read_to_string(quota).unwrap(), "invalid metadata");
    assert!(!f.paths.state_file().exists());
}
