//! Exercise the CLI against native-shaped children in temporary profile
//! homes. The fakes record the launched protocol, process and environment keys.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};

use chrono::Utc;
use rtr::paths::Paths;
use serde_json::{json, Value};

const FAKE: &str = r#"
import datetime, json, os, pathlib, subprocess, sys, time

tool = sys.argv[1]
args = sys.argv[2:]
home_key = "CLAUDE_CONFIG_DIR" if tool == "claude" else "CODEX_HOME"
home = pathlib.Path(os.environ[home_key])
probe = args[:2] in (["-p", "--input-format"], ["app-server", "--listen"])
with (home / ("probe.jsonl" if probe else "launch.jsonl")).open("a") as log:
    print(json.dumps({"argv": args, "env_keys": sorted(os.environ),
                      "pid": os.getpid(), "started": time.monotonic()}), file=log)
if not probe:
    if (home / "reject-launch").exists():
        session = args[args.index("--session-id") + 1]
        path = home / "projects" / "fixture" / (session + ".jsonl")
        path.parent.mkdir(parents=True, exist_ok=True)
        # RTR expects native records to be newer than the transcript's birth.
        path.touch()
        now = datetime.datetime.now(datetime.timezone.utc).isoformat()
        rows = [
            {"type": "user", "sessionId": session, "timestamp": now, "uuid": "u",
             "cwd": str(pathlib.Path.cwd()), "message": {"role": "user", "content": "fixture"}},
            {"type": "assistant", "sessionId": session, "timestamp": now, "uuid": "a",
             "parentUuid": "u", "isApiErrorMessage": True, "apiError": "usage_limit_reached",
             "quotaLimits": {"rateLimitType": "seven_day", "resetsAt": int(time.time()) + 86400},
             "message": {"role": "assistant", "model": "<synthetic>",
                         "content": [{"type": "text", "text": "usage limit"}]}}
        ]
        path.write_text("".join(json.dumps(row) + "\n" for row in rows))
        sys.exit(1)
    print(home.name)
    sys.exit(0)

behavior = json.loads((home / "behavior.json").read_text())
kind = behavior["kind"]
reset = int(time.time()) + 86400

def reply(row):
    print(json.dumps(row), flush=True)

def recv():
    return json.loads(sys.stdin.readline())

def control(request_id, result):
    reply({"type": "control_response", "response": {
        "subtype": "success", "request_id": request_id, "response": result
    }})

# Startup notifications and diagnostics can include arbitrary native data.
# rtr should consume replies and discard these on both pipes.
reply({"type": "diagnostic", "message": "private stdout marker"})
print("private stderr marker", file=sys.stderr, flush=True)
if kind in ("timeout", "wrapper-timeout"):
    if kind == "wrapper-timeout":
        descendant = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(40)"])
        (home / "descendant.pid").write_text(str(descendant.pid))
    sys.stdout.write('{"partial":')
    sys.stdout.flush()
    time.sleep(40)
    sys.exit(4)
if kind == "error":
    sys.exit(7)

if tool == "claude":
    assert os.environ["CLAUDE_SECURESTORAGE_CONFIG_DIR"] == str(home)
    assert recv() == {"type": "control_request", "request_id": "init",
                      "request": {"subtype": "initialize"}}
    control("unrelated", {})
    control("init", {})
    assert recv() == {"type": "control_request", "request_id": "usage",
                      "request": {"subtype": "get_usage", "skip_behaviors": True}}
    if kind == "unavailable":
        control("usage", {"rate_limits_available": False, "rate_limits": None})
        sys.exit(0)
    snapshot = {"five_hour": None, "seven_day": {
        "utilization": behavior["used"],
        "resets_at": datetime.datetime.fromtimestamp(reset, datetime.timezone.utc).isoformat()
    }}
    if kind != "fallback":
        (home / ".claude.json").write_text(json.dumps({"cachedUsageUtilization": {
            "fetchedAtMs": int(time.time() * 1000), "utilization": snapshot
        }}))
    control("usage", {"rate_limits_available": True, "rate_limits": snapshot})
else:
    init = recv()
    assert init["id"] == 1 and init["method"] == "initialize"
    reply({"id": 8, "result": {}})
    reply({"id": 1, "result": {"codexHome": str(home)}})
    assert recv() == {"method": "initialized"}
    assert recv() == {"id": 2, "method": "account/rateLimits/read",
                      "params": {"excludeResetCreditDetails": True}}
    if kind in ("unavailable", "login"):
        message = "authentication required" if kind == "login" else "usage service unavailable"
        reply({"id": 2, "error": {"code": -32000, "message": message}})
        sys.exit(0)
    def meter(used, minutes):
        return {"usedPercent": used, "windowDurationMins": minutes, "resetsAt": reset}
    limit_id = "extra" if kind == "extra-only" else "codex"
    primary = {"limitId": limit_id, "primary": meter(behavior["used"], 10080), "secondary": None}
    buckets = {limit_id: primary}
    if limit_id == "codex":
        buckets["extra"] = {"primary": meter(12, 720), "secondary": None}
    reply({"id": 2, "result": {"rateLimits": primary, "rateLimitsByLimitId": buckets}})
    if kind == "trailing-output":
        assert sys.stdin.read() == ""
        print("x" * 262144, flush=True)
        sys.exit(0)

assert sys.stdin.read() == ""
"#;

struct Fixture {
    temp: tempfile::TempDir,
    paths: Paths,
    config: String,
}

impl Fixture {
    fn new(claude: &[(&str, bool)], codex: &[(&str, bool)], refresh: Option<bool>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
        };
        fs::create_dir(&paths.config_dir).unwrap();
        let fake = temp.path().join("native.py");
        fs::write(&fake, FAKE).unwrap();
        let mut config = refresh
            .map(|refresh| format!("[routing]\nrefresh={refresh}\n"))
            .unwrap_or_default();
        for (tool, profiles) in [("claude", claude), ("codex", codex)] {
            if profiles.is_empty() {
                continue;
            }
            config.push_str(&format!(
                "[tools.{tool}]\ncommand=['python3',{:?},'{tool}']\ncopy=[]\n",
                fake.to_str().unwrap()
            ));
            for (profile, enabled) in profiles {
                config.push_str(&format!(
                    "[tools.{tool}.profiles.{profile}]\nenabled={enabled}\n"
                ));
                fs::create_dir_all(paths.profile_home_dir(tool, profile)).unwrap();
            }
        }
        fs::write(paths.config_file(), &config).unwrap();
        Self {
            temp,
            paths,
            config,
        }
    }

    fn home(&self, tool: &str, profile: &str) -> PathBuf {
        self.paths.profile_home_dir(tool, profile)
    }

    fn behavior(&self, tool: &str, profile: &str, kind: &str, used: u8) {
        fs::write(
            self.home(tool, profile).join("behavior.json"),
            json!({"kind":kind,"used":used}).to_string(),
        )
        .unwrap();
    }

    fn claude_cache(&self, profile: &str, at: chrono::DateTime<Utc>, used: u8) {
        let reset = Utc::now() + chrono::Duration::days(1);
        fs::write(
            self.home("claude", profile).join(".claude.json"),
            json!({"cachedUsageUtilization": {
                "fetchedAtMs": at.timestamp_millis(),
                "utilization": {"seven_day": {"utilization": used, "resets_at": reset}}
            }})
            .to_string(),
        )
        .unwrap();
    }

    fn codex_rollout(&self, profile: &str, minutes_ago: i64, weekly: u8) {
        let reset = Utc::now() + chrono::Duration::days(1);
        self.codex_rollout_with_reset(profile, minutes_ago, weekly, reset);
    }

    fn codex_rollout_with_reset(
        &self,
        profile: &str,
        minutes_ago: i64,
        weekly: u8,
        reset: chrono::DateTime<Utc>,
    ) {
        let folder = self.home("codex", profile).join("sessions");
        fs::create_dir_all(&folder).unwrap();
        let at = Utc::now() - chrono::Duration::minutes(minutes_ago);
        let reset = reset.timestamp();
        let head = json!({
            "type":"session_meta","payload":{"id":"example","cwd":self.temp.path()}
        });
        let event = json!({
            "type":"event_msg","timestamp":at,"payload":{"type":"token_count","rate_limits":{
                "limit_id":"codex",
                "primary":{"used_percent":14,"window_minutes":300,"resets_at":reset},
                "secondary":{"used_percent":weekly,"window_minutes":10080,"resets_at":reset}
            }}
        });
        fs::write(
            folder.join("rollout-example.jsonl"),
            format!("{head}\n{event}\n"),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_rtr"))
            .args(args)
            .current_dir(self.temp.path())
            .env("HOME", self.temp.path())
            .env("RTR_STATE_DIR", &self.paths.state_dir)
            .env("RTR_CONFIG_DIR", &self.paths.config_dir)
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .env("OPENAI_API_KEY", "synthetic override")
            .env("ANTHROPIC_API_KEY", "synthetic override")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        for bytes in [&output.stdout, &output.stderr] {
            assert!(
                !String::from_utf8_lossy(bytes).contains("private "),
                "native diagnostics reached rtr output"
            );
        }
        output
    }

    fn list(&self, refresh: bool) -> String {
        let mut args = vec!["ls", "--color", "never"];
        if refresh {
            args.push("--refresh");
        }
        String::from_utf8(self.run(&args).stdout).unwrap()
    }

    fn probes(&self, tool: &str, profile: &str) -> Vec<Value> {
        let path = self.home(tool, profile).join("probe.jsonl");
        if !path.exists() {
            return Vec::new();
        }
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn state(&self) -> Value {
        let path = self.paths.state_dir.join("usage-refresh.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }
}

#[test]
fn explicit_listing_refreshes_enabled_homes_and_prefers_newer_windows_per_meter() {
    let fixture = Fixture::new(
        &[("personal", true), ("disabled", false)],
        &[("work", true)],
        None,
    );
    fixture.behavior("claude", "personal", "usage", 73);
    fixture.behavior("codex", "work", "trailing-output", 91);
    fixture.codex_rollout("work", 60, 34);
    assert!(!fixture.list(false).contains("app_server"));
    assert!(fixture.probes("claude", "personal").is_empty());

    let listing = fixture.list(true);
    assert!(listing.contains("73%"), "{listing}");
    assert!(listing.contains("91%"), "{listing}");
    assert!(listing.contains("app_server"), "{listing}");
    assert!(listing.contains("14%"), "{listing}");
    assert!(listing.contains("rollout"), "{listing}");
    assert!(listing.contains("720m, extra"), "{listing}");
    assert!(!listing.contains("34%"), "{listing}");
    assert!(fixture.probes("claude", "disabled").is_empty());
    for (tool, profile, argv, keys) in [
        (
            "claude",
            "personal",
            json!([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--safe-mode",
                "--no-session-persistence"
            ]),
            &["CLAUDE_CONFIG_DIR", "CLAUDE_SECURESTORAGE_CONFIG_DIR"][..],
        ),
        (
            "codex",
            "work",
            json!(["app-server", "--listen", "stdio://"]),
            &["CODEX_HOME"][..],
        ),
    ] {
        let records = fixture.probes(tool, profile);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["argv"], argv);
        let env_keys = records[0]["env_keys"].as_array().unwrap();
        for key in keys {
            assert!(env_keys.contains(&json!(key)), "{records:?}");
        }
        for absent in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY"] {
            assert!(!env_keys.contains(&json!(absent)), "{records:?}");
        }
    }
    let state = fixture.state();
    let observation = &state["tools"]["codex"]["work"]["observation"];
    assert_eq!(observation["source"], "app_server");
    assert_eq!(
        observation["windows"][0]["primary"]["window_minutes"],
        10080
    );
    assert!(observation["observed_at"].is_string());
    assert!(state["tools"]["claude"]["personal"]["observation"].is_null());

    // A newer rollout wins the weekly meter without dropping App Server extras.
    std::thread::sleep(Duration::from_millis(2));
    fixture.codex_rollout("work", 0, 64);
    let listing = fixture.list(false);
    assert!(listing.contains("64%"), "{listing}");
    assert!(listing.contains("720m, extra"), "{listing}");
    assert!(!listing.contains("91%"), "{listing}");
}

#[test]
fn unavailable_and_login_failures_keep_evidence_and_success_clears_status() {
    let fixture = Fixture::new(
        &[("personal", true)],
        &[("work", true), ("logged_out", true), ("broken", true)],
        None,
    );
    fixture.behavior("claude", "personal", "usage", 73);
    fixture.behavior("codex", "work", "usage", 91);
    fixture.behavior("codex", "logged_out", "login", 0);
    fixture.behavior("codex", "broken", "error", 0);
    fixture.list(true);

    let cache = fixture.home("claude", "personal").join(".claude.json");
    let mut old: Value = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
    old["cachedUsageUtilization"]["fetchedAtMs"] =
        json!((Utc::now() - chrono::Duration::hours(1)).timestamp_millis());
    fs::write(cache, old.to_string()).unwrap();
    fixture.behavior("claude", "personal", "unavailable", 0);
    fixture.behavior("codex", "work", "unavailable", 0);
    let failed = fixture.list(true);
    assert_eq!(failed.matches("refresh failed (unavailable)").count(), 2);
    assert_eq!(failed.matches("refresh failed (login)").count(), 1);
    assert_eq!(failed.matches("refresh failed (error)").count(), 1);
    assert!(failed.contains("73%") && failed.contains("91%"), "{failed}");
    assert_eq!(fixture.list(false).matches("refresh failed").count(), 4);

    fixture.behavior("claude", "personal", "fallback", 27);
    let fallback = fixture.list(true);
    assert!(
        fallback.contains("73%") && !fallback.contains("27%"),
        "{fallback}"
    );
    assert_eq!(fallback.matches("refresh failed (unavailable)").count(), 2);

    fixture.behavior("claude", "personal", "usage", 20);
    fixture.behavior("codex", "work", "usage", 30);
    fixture.behavior("codex", "logged_out", "usage", 40);
    fixture.behavior("codex", "broken", "usage", 50);
    assert!(!fixture.list(true).contains("refresh failed"));
    assert!(!fixture.list(false).contains("refresh failed"));
}

#[test]
fn deadlines_kill_started_children_and_bound_parallelism() {
    let profiles = ["a", "b", "c", "d", "e", "f", "g", "h", "i"];
    let fixture = Fixture::new(&[], &profiles.map(|profile| (profile, true)), None);
    for profile in profiles {
        fixture.behavior("codex", profile, "timeout", 0);
    }
    fixture.behavior("codex", "a", "wrapper-timeout", 0);
    let start = Instant::now();
    let listing = fixture.list(true);
    let elapsed = start.elapsed();
    assert!(
        (Duration::from_secs(7)..Duration::from_secs(9)).contains(&elapsed),
        "{elapsed:?}"
    );
    assert_eq!(listing.matches("refresh failed (timeout)").count(), 9);
    let mut probes: Vec<_> = profiles
        .iter()
        .flat_map(|profile| fixture.probes("codex", profile))
        .collect();
    assert_eq!(probes.len(), 8, "{probes:?}");
    probes.sort_by(|a, b| {
        a["started"]
            .as_f64()
            .partial_cmp(&b["started"].as_f64())
            .unwrap()
    });
    let time = |i: usize| probes[i]["started"].as_f64().unwrap();
    assert!(time(3) - time(0) < 1.0, "{probes:?}");
    assert!(time(4) - time(3) > 4.0, "{probes:?}");
    for probe in probes {
        assert_reaped(probe["pid"].as_i64().unwrap() as libc::pid_t);
    }
    let descendant = fs::read_to_string(fixture.home("codex", "a").join("descendant.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_reaped(descendant);
}

fn assert_reaped(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        // SAFETY: signal 0 checks existence without signaling the process.
        if unsafe { libc::kill(pid, 0) } == -1 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            return;
        }
        assert!(Instant::now() < deadline, "child {pid} survived");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn config_can_disable_explicit_refresh() {
    let fixture = Fixture::new(&[("personal", true)], &[("work", true)], Some(false));
    let output = fixture.run(&["ls", "-r", "--color", "never"]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("refresh failed"));
    assert!(fixture.probes("claude", "personal").is_empty());
    assert!(fixture.probes("codex", "work").is_empty());
    assert!(!fixture.paths.state_dir.join("usage-refresh.json").exists());
}

#[test]
fn automatic_launch_uses_refreshed_full_meter_as_route_evidence() {
    let fixture = Fixture::new(&[], &[("a", true), ("b", true)], None);
    fixture.codex_rollout("a", 60, 17);
    fixture.codex_rollout("b", 0, 48);
    fixture.behavior("codex", "a", "usage", 100);
    fixture.behavior("codex", "b", "usage", 100);

    assert_eq!(
        String::from_utf8(fixture.run(&["codex"]).stdout).unwrap(),
        "b\n"
    );
    assert_eq!(fixture.probes("codex", "a").len(), 1);
    assert!(fixture.probes("codex", "b").is_empty());
    let events: Vec<Value> = serde_json::from_slice(&fixture.run(&["log", "-j"]).stdout).unwrap();
    let route = events
        .iter()
        .find(|event| event["kind"] == "route")
        .unwrap();
    assert_eq!(route["destination"], "b");
    let observed = &fixture.state()["tools"]["codex"]["a"]["observation"]["observed_at"];
    assert!(
        route["evidence"].as_array().unwrap().iter().any(|row| {
            row["profile"] == "a"
                && row["used_percent"].as_f64() == Some(100.0)
                && row["reason"] == "full"
                && &row["observed_at"] == observed
        }),
        "{route}"
    );
    // The old rollout's short window remains visible, while the newer
    // App Server snapshot keeps the same profile from probing again.
    assert_eq!(fixture.run(&["codex"]).stdout, b"b\n");
    assert_eq!(fixture.probes("codex", "a").len(), 1);
}

#[test]
fn automatic_launch_probes_an_initialized_home_without_readings() {
    let fixture = Fixture::new(&[], &[("a", true), ("b", true)], None);
    fixture.codex_rollout("b", 0, 48);
    fixture.behavior("codex", "a", "usage", 100);

    assert_eq!(fixture.run(&["codex"]).stdout, b"b\n");
    assert_eq!(fixture.probes("codex", "a").len(), 1);
    assert!(fixture.probes("codex", "b").is_empty());
}

#[test]
fn dry_fork_exact_launch_exact_resume_and_opt_out_do_not_probe() {
    let fixture = Fixture::new(&[], &[("a", true), ("b", true)], None);
    fixture.codex_rollout("a", 60, 17);
    for profile in ["a", "b"] {
        fixture.behavior("codex", profile, "usage", 100);
    }
    fixture.run(&[
        "fork",
        "example",
        "-t",
        "codex",
        "--no-picker",
        "--dry-run",
        "--json",
    ]);
    fixture.run(&["codex", "-p", "a"]);
    fixture.run(&["resume", "example", "-t", "codex", "-p", "a"]);
    assert!(!fixture.paths.state_dir.join("usage-refresh.json").exists());
    for profile in ["a", "b"] {
        assert!(fixture.probes("codex", profile).is_empty());
    }

    fs::write(
        fixture.paths.config_file(),
        format!("[routing]\nrefresh=false\n{}", fixture.config),
    )
    .unwrap();
    let output = fixture.run(&["codex"]);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "a\n");
    fixture.run(&["fork", "example", "-t", "codex", "--no-picker"]);
    for profile in ["a", "b"] {
        assert!(fixture.probes("codex", profile).is_empty());
    }
    assert!(!fixture.paths.state_dir.join("usage-refresh.json").exists());
}

#[test]
fn automatic_fork_refreshes_stale_claude_cache_before_choosing_home() {
    let fixture = Fixture::new(&[("source", true), ("target", true)], &[], None);
    let at = Utc::now() - chrono::Duration::hours(1);
    for (profile, used) in [("source", 100), ("target", 23)] {
        fixture.behavior("claude", profile, "usage", used);
        fixture.claude_cache(profile, at, 13);
    }
    let history = fixture.home("claude", "source").join("projects/fixture");
    fs::create_dir_all(&history).unwrap();
    let first = json!({
        "type":"user", "sessionId":"fixture-source", "uuid":"u", "cwd":fixture.temp.path(),
        "timestamp":at, "message":{"role":"user","content":"fixture prompt"}
    });
    let last = json!({
        "type":"assistant", "sessionId":"fixture-source", "uuid":"a", "parentUuid":"u",
        "timestamp":at, "message":{"role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"fixture answer"}]}
    });
    fs::write(
        history.join("fixture-source.jsonl"),
        format!("{first}\n{last}\n"),
    )
    .unwrap();
    let output = fixture.run(&["fork", "fixture-source", "-t", "claude", "--no-picker"]);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "target\n");
    assert_eq!(fixture.probes("claude", "source").len(), 1);
    assert_eq!(fixture.probes("claude", "target").len(), 1);
    assert!(fixture.list(false).contains("23%"));
}

#[test]
fn pre_model_rerun_does_not_refresh_still_stale_candidate_twice() {
    let fixture = Fixture::new(&[("a", true), ("b", true)], &[], None);
    let at = Utc::now() - chrono::Duration::hours(1);
    for profile in ["a", "b"] {
        fixture.claude_cache(profile, at, 13);
    }
    fixture.behavior("claude", "a", "usage", 24);
    // Keep b stale through the first batch so an accidental refresh in the
    // limit preview or rerun would start this binary again.
    fixture.behavior("claude", "b", "unavailable", 0);
    fs::write(fixture.home("claude", "a").join("reject-launch"), "").unwrap();

    let output = fixture.run(&["claude", "--", "-p", "fixture"]);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "b\n");
    assert_eq!(fixture.probes("claude", "a").len(), 1);
    assert_eq!(fixture.probes("claude", "b").len(), 1);
    let events: Vec<Value> = serde_json::from_slice(&fixture.run(&["log", "-j"]).stdout).unwrap();
    assert!(events
        .iter()
        .any(|row| row["kind"] == "hold_learned" && row["profile"] == "a"));
    assert!(
        events.iter().any(|row| {
            row["kind"] == "route"
                && row["destination"] == "b"
                && row["reason"] == "rerun_after_limit_before_model_activity"
        }),
        "{events:?}"
    );
}

#[test]
fn a_full_extra_only_bucket_never_blocks_subscription_routing() {
    let fixture = Fixture::new(&[], &[("a", true)], None);
    fixture.behavior("codex", "a", "extra-only", 100);

    assert_eq!(fixture.run(&["codex"]).stdout, b"a\n");
    let listing = fixture.list(false);
    assert!(
        listing.contains("Week, extra") && listing.contains("100%"),
        "{listing}"
    );
    assert!(!listing.contains("Week, all"), "{listing}");
}

#[test]
fn recreating_a_profile_discards_the_previous_homes_observation() {
    let fixture = Fixture::new(&[], &[("a", true)], None);
    fixture.behavior("codex", "a", "usage", 100);
    assert!(fixture.list(true).contains("FULL"));
    let home = fixture.home("codex", "a");
    fs::rename(&home, fixture.temp.path().join("previous-home")).unwrap();
    fs::create_dir(&home).unwrap();
    fixture.behavior("codex", "a", "usage", 32);

    assert_eq!(fixture.run(&["codex"]).stdout, b"a\n");
    assert_eq!(fixture.probes("codex", "a").len(), 1);
    assert!(fixture.list(false).contains("32%"));
}

#[test]
fn a_recent_sample_with_a_passed_reset_is_refreshed() {
    let fixture = Fixture::new(&[], &[("a", true)], None);
    let reset = Utc::now() - chrono::Duration::seconds(30);
    fixture.codex_rollout_with_reset("a", 1, 90, reset);
    fixture.behavior("codex", "a", "usage", 20);
    assert_eq!(fixture.run(&["codex"]).stdout, b"a\n");
    assert_eq!(fixture.probes("codex", "a").len(), 1);
    assert!(fixture.list(false).contains("20%"));
}

#[test]
fn unavailable_store_lock_keeps_saved_codex_observation_for_routing() {
    let fixture = Fixture::new(&[], &[("a", true), ("b", true)], None);
    fixture.behavior("codex", "a", "usage", 17);
    fixture.behavior("codex", "b", "usage", 48);
    fixture.list(true);
    let mut state = fixture.state();
    state["tools"]["codex"]["a"]["observation"]["observed_at"] =
        json!(Utc::now() - chrono::Duration::hours(1));
    let path = fixture.paths.state_dir.join("usage-refresh.json");
    let prior = state.to_string();
    fs::write(&path, &prior).unwrap();

    fixture.behavior("codex", "a", "usage", 100);
    let lock = fs::File::open(&fixture.paths.state_dir).unwrap();
    lock.lock().unwrap();
    let output = fixture.run(&["codex"]);

    assert_eq!(output.stdout, b"a\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not lock usage refresh state"));
    assert_eq!(fixture.probes("codex", "a").len(), 2);
    assert_eq!(fixture.probes("codex", "b").len(), 1);
    assert_eq!(fs::read_to_string(path).unwrap(), prior);
}
