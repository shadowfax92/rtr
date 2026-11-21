//! Exercise the CLI against synthetic transcripts and the public hold index.
//! Launch fixtures use a shell recorder in private homes.

use chrono::{DateTime, Duration, Utc};
use rtr::{
    limits::{
        evaluate, Blocker, Evaluation, HoldScope, MeterKey, MeterScope, MeterWindow, ModelFamily,
        ProfileLimits,
    },
    paths::Paths,
    quota::{read_holds, HoldEvidence},
};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct Fixture {
    temp: tempfile::TempDir,
    paths: Paths,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
        };
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::create_dir_all(&paths.state_dir).unwrap();
        fs::write(
            paths.config_file(),
            "[tools.claude]\ncommand=['false']\ncopy=[]\n[tools.claude.profiles.eng]\n",
        )
        .unwrap();
        Self { temp, paths }
    }

    fn quota_path(&self) -> PathBuf {
        self.paths.state_dir.join("quota.json")
    }

    fn cache_full_fable(&self, at: DateTime<Utc>, reset: DateTime<Utc>, windows: &[&str]) {
        let home = self.paths.profile_home_dir("claude", "eng");
        fs::create_dir_all(&home).unwrap();
        let limits: Vec<_> = windows
            .iter()
            .map(|kind| {
                json!({
                    "kind":kind, "percent":100, "resets_at":reset,
                    "scope":{"model":{"id":null,"display_name":"Fable"}}
                })
            })
            .collect();
        let cache = json!({"cachedUsageUtilization":{
            "fetchedAtMs":at.timestamp_millis(),
            "utilization":{"limits":limits}
        }});
        fs::write(home.join(".claude.json"), cache.to_string()).unwrap();
    }

    fn transcript(&self, id: &str) -> PathBuf {
        let path = self
            .paths
            .profile_home_dir("claude", "eng")
            .join(format!("projects/fixture/{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(&path).unwrap();
        path
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rtr"));
        command
            .args(args)
            .env("RTR_CONFIG_DIR", &self.paths.config_dir)
            .env("RTR_STATE_DIR", &self.paths.state_dir)
            .env("HOME", self.temp.path())
            .current_dir(self.temp.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert_success(&output);
        output
    }

    fn session(&self, id: &str) -> Value {
        serde_json::from_slice(&self.run(&["session", id, "-t", "claude", "-j"]).stdout).unwrap()
    }

    fn log(&self) -> Vec<Value> {
        serde_json::from_slice(&self.run(&["log", "-t", "claude", "-p", "eng", "-j"]).stdout)
            .unwrap()
    }

    fn evidence(&self) -> HoldEvidence {
        read_holds(&self.paths)
            .unwrap()
            .remove(&("claude".into(), "eng".into()))
            .unwrap_or_default()
    }

    fn use_launch_recorder(&self) {
        fs::write(
            self.paths.config_file(),
            r#"[routing]
refresh = false
[tools.claude]
command = ['sh', '-c', 'printf "OK\n"']
args = ['--model', 'claude-opus-5-5']
copy = []
[tools.claude.profiles.eng]
"#,
        )
        .unwrap();
    }

    fn owned_history(&self, id: &str, row: &Value) {
        let file = self.transcript(id);
        append(&file, row);
        let origins_path = self.paths.state_dir.join("fork-origins.json");
        let mut origins = fs::read(&origins_path)
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .unwrap_or_else(|| json!({"claude":{"eng":{}}}));
        // Exact byte provenance lets the fixture use historical timestamps
        // without changing the filesystem's creation time.
        origins["claude"]["eng"][id] = json!({"path":file,"bytes":0,"at":row["timestamp"]});
        fs::write(origins_path, origins.to_string()).unwrap();
    }

    fn launch(&self) {
        assert_eq!(self.run(&["claude", "--", "-p", "x"]).stdout, b"OK\n");
    }

    fn hold_events(&self) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|event| event["kind"] == "hold_learned" || event["kind"] == "hold_cleared")
            .collect()
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn append(path: &Path, row: &Value) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{row}").unwrap();
}

fn rejection(kind: &str, at: DateTime<Utc>, reset: Option<DateTime<Utc>>) -> Value {
    json!({"type":"assistant", "apiError":"usage_limit_reached", "timestamp":at,
           "message":{"model":"<synthetic>", "content":"fixture limit"},
           "quotaLimits":{"rateLimitType":kind, "resetsAt":reset.map(|at| at.timestamp())}})
}

fn fable_rejection(at: DateTime<Utc>, reset: Option<DateTime<Utc>>) -> Value {
    let mut row = rejection("weekly_scoped", at, reset);
    row["quotaLimits"]["scope"] = json!({"model":{"id":null,"display_name":"Fable"}});
    row
}

fn success(model: &str, at: DateTime<Utc>) -> Value {
    json!({"type":"assistant", "timestamp":at, "message":{"model":model}})
}

#[test]
fn expired_rejection_cannot_reappear_when_a_legacy_writer_drops_scoped_state() {
    let f = Fixture::new();
    f.use_launch_recorder();
    let at = Utc::now() - Duration::days(5);
    f.owned_history(
        "expired",
        &rejection("seven_day", at, Some(at + Duration::days(2))),
    );
    let legacy = json!({"entries":{"claude":{"eng":{
        "limit":null,"native_kind":null,"success_at":at-Duration::hours(1),"cleared_at":null
    }}}});
    fs::write(f.quota_path(), legacy.to_string()).unwrap();

    f.launch();
    // An installed pre-scoped `rtr session` writer reserializes only the
    // flat fields. The expired hold has no legacy projection to preserve.
    let mut rewritten: Value = serde_json::from_slice(&fs::read(f.quota_path()).unwrap()).unwrap();
    rewritten.as_object_mut().unwrap().remove("schema_version");
    rewritten["entries"]["claude"]["eng"]
        .as_object_mut()
        .unwrap()
        .remove("scoped");
    assert_eq!(rewritten, legacy);
    fs::write(f.quota_path(), rewritten.to_string()).unwrap();
    f.launch();

    assert!(f.hold_events().is_empty());
    assert_eq!(
        fs::read_to_string(f.quota_path()).unwrap(),
        legacy.to_string()
    );
    assert_eq!(f.session("expired")["limit"]["active"], false);
    assert!(f.hold_events().is_empty());
}

#[test]
fn expired_rejection_and_later_owned_recovery_emit_no_hold_events() {
    let f = Fixture::new();
    f.use_launch_recorder();
    let at = Utc::now() - Duration::hours(2);
    f.owned_history(
        "expired",
        &rejection("seven_day", at, Some(at + Duration::hours(1))),
    );
    f.launch();

    let recovered_at = at + Duration::minutes(90);
    f.owned_history("recovery", &success("claude-opus-5-5", recovered_at));
    // On the real launch this newer record was learned at child exit. The
    // exact-session command merges that same owned evidence before replay.
    f.session("recovery");
    f.launch();
    assert!(f.hold_events().is_empty());
    assert!(f.evidence().holds.is_empty());
    assert_eq!(
        f.evidence().success_watermarks.models[&ModelFamily::Opus],
        recovered_at
    );
}

#[test]
fn newer_expired_reset_suppresses_an_older_fallback_hold_without_logging() {
    for (previously_learned, delay) in [(false, 30), (true, 30), (false, 0), (true, 0)] {
        let f = Fixture::new();
        f.use_launch_recorder();
        let at = Utc::now() - Duration::minutes(2);
        f.owned_history("estimated", &rejection("seven_day", at, None));
        if previously_learned {
            f.session("estimated");
        }
        let before = f.hold_events();
        f.owned_history(
            "actual",
            &rejection(
                "seven_day",
                at + Duration::seconds(delay),
                Some(at + Duration::minutes(1)),
            ),
        );

        f.launch();
        f.launch();
        assert_eq!(f.hold_events(), before);
    }
}

#[test]
fn fresh_hold_is_learned_once_and_already_retired_rejections_leave_state_unchanged() {
    let f = Fixture::new();
    f.use_launch_recorder();
    let at = Utc::now() - Duration::minutes(2);
    let reset = at + Duration::hours(1);
    f.owned_history(
        "recovery",
        &success("claude-opus-5-5", at + Duration::minutes(1)),
    );
    f.session("recovery");
    let recovered = fs::read(f.quota_path()).unwrap();
    f.owned_history(
        "retired-by-success",
        &rejection("seven_day", at, Some(reset)),
    );
    f.launch();
    assert_eq!(fs::read(f.quota_path()).unwrap(), recovered);

    f.run(&["clear-limit", "claude", "eng"]);
    let cleared = fs::read(f.quota_path()).unwrap();
    // The Opus success cannot retire Fable. This scope needs the clear.
    f.owned_history("retired-by-clear", &fable_rejection(at, Some(reset)));
    f.launch();
    assert_eq!(fs::read(f.quota_path()).unwrap(), cleared);
    assert!(f.hold_events().is_empty());

    f.owned_history("fresh", &fable_rejection(Utc::now(), Some(reset)));
    f.launch();
    f.launch();
    let log = f.hold_events();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["kind"], "hold_learned");
    assert_eq!(log[0]["scope"], "weekly Fable");
}

fn verdict(evidence: &HoldEvidence, model: ModelFamily) -> Evaluation {
    evaluate(
        &ProfileLimits {
            holds: evidence.holds.clone(),
            success_watermarks: evidence.success_watermarks.clone(),
            cleared_at: evidence.cleared_at,
            ..Default::default()
        },
        model,
        Utc::now(),
    )
}

#[test]
fn legacy_read_migrate_clear_and_replay_keep_the_v1_session_contract() {
    let f = Fixture::new();
    let path = f.transcript("fixture-hold");
    let at = Utc::now();
    let prior_success = at - Duration::minutes(2);
    let legacy = json!({"entries":{"claude":{"eng":{
        "limit":{"kind":"five_hour","at":at - Duration::minutes(1),"resets_at":at + Duration::minutes(5),"active":true,"message":"legacy"},
        "native_kind":"five_hour","success_at":prior_success,"cleared_at":null
    }}}});
    let legacy_bytes = legacy.to_string();
    fs::write(f.quota_path(), &legacy_bytes).unwrap();
    let old = f.evidence();
    assert_eq!(old.success_watermarks.common, Some(prior_success));
    assert!(old.success_watermarks.models.is_empty());
    assert_eq!(
        old.holds[0].applies_to,
        HoldScope::Meter(MeterKey {
            window: MeterWindow::FiveHour,
            scope: MeterScope::AllModels
        })
    );
    assert_eq!(fs::read_to_string(f.quota_path()).unwrap(), legacy_bytes);

    let reset = DateTime::from_timestamp((at + Duration::hours(1)).timestamp(), 0).unwrap();
    append(&path, &fable_rejection(at, Some(reset)));
    let expected = json!({"schema_version":1,"tool":"claude","profile":"eng","id":"fixture-hold","cwd":"",
        "model":null,"effort":null,"limit":{"kind":"weekly_scoped","at":at,"resets_at":reset,"active":true,"message":"fixture limit"}});
    assert_eq!(f.session("fixture-hold"), expected);
    let migrated = fs::read_to_string(f.quota_path()).unwrap();
    assert_eq!(f.session("fixture-hold"), expected);
    assert_eq!(fs::read_to_string(f.quota_path()).unwrap(), migrated);
    let disk: Value = serde_json::from_str(&migrated).unwrap();
    assert_eq!(disk["schema_version"], 2);
    let projection = &disk["entries"]["claude"]["eng"];
    assert_eq!(projection["limit"]["resets_at"], json!(reset));
    assert_eq!(projection["native_kind"], "weekly_scoped");
    assert_eq!(f.evidence().holds.len(), 2);
    assert!(f
        .evidence()
        .holds
        .iter()
        .all(|hold| hold.applies_to != HoldScope::Profile));

    f.run(&["clear-limit", "claude", "eng"]);
    let cleared = fs::read_to_string(f.quota_path()).unwrap();
    assert_eq!(f.session("fixture-hold"), expected);
    assert_eq!(fs::read_to_string(f.quota_path()).unwrap(), cleared);
    let evidence = f.evidence();
    assert!(evidence.holds.is_empty());
    assert_eq!(evidence.success_watermarks.common, Some(prior_success));
    assert!(evidence.cleared_at.unwrap() > at);
    let log = f.log();
    assert_eq!(log.len(), 3);
    assert_eq!(log[0]["kind"], "hold_learned");
    assert_eq!(log[0]["scope"], "weekly Fable");
    assert_eq!(log[0]["observed_at"], json!(at));
    assert_eq!(log[0]["reset_at"], json!(reset));
    assert_eq!(log[1]["scope"], "5h all");
    assert_eq!(log[2]["scope"], "weekly Fable");
    for row in &log[1..] {
        assert_eq!(row["kind"], "hold_cleared");
        assert_eq!(row["reason"], "manual");
        assert_eq!(row["observed_at"], json!(evidence.cleared_at));
        assert_eq!(row["operation_id"], log[1]["operation_id"]);
    }
}

#[test]
fn manual_clear_skips_expired_holds_and_logs_nothing_on_replay() {
    for include_expired in [false, true] {
        let f = Fixture::new();
        let path = f.transcript("fixture-hold");
        let at = Utc::now();
        if include_expired {
            let expired = json!({"entries":{"claude":{"eng":{
                "limit":{"kind":"five_hour","at":at - Duration::hours(2),
                         "resets_at":at - Duration::hours(1),"active":true,"message":"legacy"},
                "native_kind":"five_hour","success_at":null,"cleared_at":null
            }}}});
            fs::write(f.quota_path(), expired.to_string()).unwrap();
        }
        append(&path, &fable_rejection(at, Some(at + Duration::hours(1))));
        f.session("fixture-hold");
        assert_eq!(f.evidence().holds.len(), 1 + usize::from(include_expired));

        f.run(&["clear-limit", "claude", "eng"]);
        let log = f.log();
        assert_eq!(log.len(), 2, "{log:?}");
        assert_eq!(log[1]["kind"], "hold_cleared");
        assert_eq!(log[1]["scope"], "weekly Fable");
        assert!(f.evidence().holds.is_empty());
        let first_clear = f.evidence().cleared_at.unwrap();

        f.run(&["clear-limit", "claude", "eng"]);
        assert_eq!(f.log(), log);
        assert!(f.evidence().cleared_at.unwrap() > first_clear);
    }
}

#[test]
fn manual_clear_logs_a_cached_full_scope_once_even_with_a_hold() {
    for include_hold in [false, true] {
        let f = Fixture::new();
        let path = f.transcript("fixture-hold");
        let at = Utc::now();
        let reset = at + Duration::hours(1);
        f.cache_full_fable(at - Duration::seconds(1), reset, &["weekly_scoped"]);
        if include_hold {
            append(&path, &fable_rejection(at, Some(reset)));
            f.session("fixture-hold");
        }

        f.run(&["clear-limit", "claude", "eng"]);
        let log = f.log();
        assert_eq!(log.len(), 1 + usize::from(include_hold), "{log:?}");
        let event = log.last().unwrap();
        assert_eq!(event["kind"], "hold_cleared");
        assert_eq!(event["reason"], "manual");
        assert_eq!(event["scope"], "weekly Fable");
        assert_eq!(event["observed_at"], json!(f.evidence().cleared_at));
        assert!(f.evidence().holds.is_empty());

        f.run(&["clear-limit", "claude", "eng"]);
        assert_eq!(f.log(), log);
    }
}

#[test]
fn manual_clear_skips_full_fable_sample_superseded_by_success() {
    let f = Fixture::new();
    let path = f.transcript("recovered");
    let at = Utc::now();
    f.cache_full_fable(
        at - Duration::seconds(1),
        at + Duration::hours(1),
        &["weekly_scoped"],
    );
    append(&path, &success("claude-fable-5-2", at));
    f.session("recovered");
    assert_eq!(
        f.evidence().success_watermarks.models[&ModelFamily::Fable],
        at
    );

    f.run(&["clear-limit", "claude", "eng"]);
    assert!(f.log().is_empty());
    assert!(f.evidence().cleared_at.unwrap() > at);
}

#[test]
fn manual_clear_distinguishes_native_and_standard_windows() {
    let f = Fixture::new();
    let at = Utc::now();
    // "session" maps to the standard 5h window. "5h" is an opaque native
    // window label, even though it has the same short display text.
    f.cache_full_fable(at, at + Duration::hours(1), &["session", "5h"]);

    f.run(&["clear-limit", "claude", "eng"]);
    let log = f.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(log.iter().all(|row| row["kind"] == "hold_cleared"));
    assert_eq!(log[0]["scope"], "5h Fable");
    assert_eq!(log[1]["scope"], "native 5h Fable");
}

#[test]
fn manual_clear_logs_a_full_codex_subscription_after_task_success() {
    let f = Fixture::new();
    fs::write(
        f.paths.config_file(),
        "[tools.codex]\ncommand=['false']\ncopy=[]\n[tools.codex.profiles.eng]\n",
    )
    .unwrap();
    let at = Utc::now();
    let rollout = f
        .paths
        .profile_home_dir("codex", "eng")
        .join("sessions/rollout-meter.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let header = json!({"type":"session_meta","payload":{"id":"fixture-meter"}});
    let sample = json!({"type":"event_msg","timestamp":at - Duration::minutes(1),
    "payload":{"type":"token_count","rate_limits":[
        {"limit_id":"codex","primary":{
            "window_minutes":300,"used_percent":100,"resets_at":(at + Duration::hours(1)).timestamp()
        }},
        {"limit_id":"premium","primary":{
            "window_minutes":10080,"used_percent":100,"resets_at":(at + Duration::hours(1)).timestamp()
        }}
    ]}});
    fs::write(rollout, format!("{header}\n{sample}\n")).unwrap();
    let prior_success = json!({"entries":{"codex":{"eng":{"success_at":at}}}});
    fs::write(f.quota_path(), prior_success.to_string()).unwrap();

    f.run(&["clear-limit", "codex", "eng"]);
    let log: Vec<Value> = serde_json::from_slice(&f.run(&["log", "-j"]).stdout).unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0]["kind"], "hold_cleared");
    assert_eq!(log[0]["tool"], "codex");
    assert_eq!(log[0]["scope"], "5h all");
    assert_eq!(log[0]["reason"], "manual");
}

#[test]
fn manual_clear_warns_when_codex_provenance_cannot_be_read() {
    let f = Fixture::new();
    fs::write(
        f.paths.config_file(),
        "[tools.codex]\ncommand=['false']\ncopy=[]\n[tools.codex.profiles.eng]\n",
    )
    .unwrap();
    fs::write(f.paths.state_dir.join("fork-origins.json"), "{broken").unwrap();
    let result = f.run(&["clear-limit", "codex", "eng"]);
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("could not read Codex fork provenance")
    );
    let log: Vec<Value> = serde_json::from_slice(&f.run(&["log", "-j"]).stdout).unwrap();
    assert!(log.is_empty());
    let holds = read_holds(&f.paths).unwrap();
    assert!(holds[&("codex".into(), "eng".into())].cleared_at.is_some());
}

#[test]
fn recovery_uses_the_success_records_model_and_timestamp() {
    let f = Fixture::new();
    let path = f.transcript("scoped");
    let at = Utc::now();
    let reset = at + Duration::hours(1);
    append(&path, &fable_rejection(at, Some(reset)));
    f.session("scoped");
    assert!(matches!(
        verdict(&f.evidence(), ModelFamily::Fable),
        Evaluation::Out(_)
    ));
    assert_eq!(
        verdict(&f.evidence(), ModelFamily::Opus),
        Evaluation::Unknown
    );

    let opus_at = Utc::now();
    let mut opus = success("claude-opus-5-5", opus_at);
    opus["requestedModel"] = json!("claude-fable-5-2[1m]");
    append(&path, &opus);
    f.session("scoped");
    let evidence = f.evidence();
    assert_eq!(
        evidence.success_watermarks.models[&ModelFamily::Opus],
        opus_at
    );
    assert!(!evidence
        .success_watermarks
        .models
        .contains_key(&ModelFamily::Fable));
    assert!(matches!(
        verdict(&evidence, ModelFamily::Fable),
        Evaluation::Out(_)
    ));
    assert_eq!(f.log().len(), 1);
    let disk: Value = serde_json::from_slice(&fs::read(f.quota_path()).unwrap()).unwrap();
    let projection = &disk["entries"]["claude"]["eng"];
    assert!(projection["success_at"].is_null());
    assert_eq!(projection["limit"]["at"], json!(at));

    let fable_at = Utc::now();
    append(&path, &success("claude-fable-5-2", fable_at));
    append(&path, &success("claude-opus-5-5", Utc::now()));
    assert_eq!(f.session("scoped")["model"], "claude-opus-5-5");
    let evidence = f.evidence();
    assert_eq!(
        evidence.success_watermarks.models[&ModelFamily::Fable],
        fable_at
    );
    assert_eq!(verdict(&evidence, ModelFamily::Fable), Evaluation::Unknown);
    let log = f.log();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1]["kind"], "hold_cleared");
    assert_eq!(log[1]["scope"], "weekly Fable");
    assert_eq!(log[1]["reason"], "recovery");
    assert_eq!(log[1]["observed_at"], json!(fable_at));
}

#[test]
fn common_window_and_ambiguous_unknown_reset_compete_for_the_profile() {
    let f = Fixture::new();
    let five_hour = f.transcript("five-hour");
    let ambiguous = f.transcript("ambiguous");
    let recovered = f.transcript("recovered");
    let at = Utc::now();
    append(
        &five_hour,
        &rejection("five_hour", at, Some(at + Duration::minutes(4))),
    );
    let mut ambiguous_row = fable_rejection(at, None);
    ambiguous_row["quotaLimits"]["rateLimitType"] = json!("seven_day_overage_included");
    append(&ambiguous, &ambiguous_row);
    f.session("five-hour");
    f.session("ambiguous");
    let Evaluation::Out(blockers) = verdict(&f.evidence(), ModelFamily::Opus) else {
        panic!("Opus should be blocked by both scopes");
    };
    assert_eq!(blockers.len(), 2);
    assert!(blockers.iter().any(|blocker| matches!(blocker, Blocker::Hold(hold) if hold.applies_to == HoldScope::Profile && hold.reset_at.is_none() && hold.until() == at + Duration::minutes(15))));
    let disk: Value = serde_json::from_slice(&fs::read(f.quota_path()).unwrap()).unwrap();
    assert_eq!(
        disk["entries"]["claude"]["eng"]["native_kind"],
        "seven_day_overage_included"
    );
    let log = f.log();
    assert_eq!(log[1]["reset_at"], Value::Null);
    assert_eq!(log[1]["hold_until"], json!(at + Duration::minutes(15)));
    let plain = String::from_utf8(f.run(&["log"]).stdout).unwrap();
    assert!(plain.contains("held until"), "{plain}");

    append(&recovered, &success("unclassified", at));
    f.session("recovered");
    assert_eq!(f.evidence().holds.len(), 2, "rejections win on ties");
    append(&recovered, &success("unclassified", Utc::now()));
    f.session("recovered");
    assert!(f.evidence().holds.is_empty());
    f.session("ambiguous");
    assert!(f.evidence().holds.is_empty());
    assert_eq!(f.log().len(), 4);
}

#[test]
fn inherited_future_history_cannot_learn_or_recover_a_hold() {
    let f = Fixture::new();
    let held = f.transcript("held");
    let copied = f.transcript("copied");
    let at = Utc::now();
    append(&held, &fable_rejection(at, None));
    f.session("held");
    let original = f.evidence();

    // The copied success has a plausible time. The copied error is dated
    // past file birth. Neither falls after the exact copy boundary.
    append(&copied, &success("claude-fable-5-2", Utc::now()));
    append(
        &copied,
        &rejection("five_hour", at + Duration::hours(1), None),
    );
    let origin = json!({"claude":{"eng":{"copied":{"path":copied,"at":at,"bytes":fs::metadata(&copied).unwrap().len()}}}});
    fs::write(
        f.paths.state_dir.join("fork-origins.json"),
        origin.to_string(),
    )
    .unwrap();
    let session = f.session("copied");
    assert_eq!(session["model"], "claude-fable-5-2");
    assert!(session["limit"].is_null());
    assert_eq!(f.evidence(), original);
    assert_eq!(f.log().len(), 1);

    append(&copied, &success("claude-fable-5-2", Utc::now()));
    f.session("copied");
    assert!(f.evidence().holds.is_empty());
    assert_eq!(f.log().len(), 2);
}

#[test]
fn concurrent_writers_keep_both_scopes_and_cannot_undo_a_clear() {
    let f = Fixture::new();
    let scoped = f.transcript("fable");
    let common = f.transcript("common");
    let at = Utc::now();
    append(&scoped, &fable_rejection(at, None));
    append(&common, &rejection("seven_day", at, None));
    let learn_fable = ["session", "fable", "-j"];
    let learn_common = ["session", "common", "-j"];
    let clear = ["clear-limit", "claude", "eng"];
    let concurrently = |commands: &[&[&str]]| {
        let children: Vec<_> = commands
            .iter()
            .map(|args| f.command(args).spawn().unwrap())
            .collect();
        for child in children {
            assert_success(&child.wait_with_output().unwrap());
        }
    };
    concurrently(&[&learn_fable, &learn_common]);
    assert_eq!(f.evidence().holds.len(), 2);
    assert_eq!(f.log().len(), 2);
    concurrently(&[&learn_fable, &clear, &learn_common]);
    assert!(f.evidence().holds.is_empty());
    assert_eq!(
        f.log()
            .iter()
            .filter(|row| row["kind"] == "hold_learned")
            .count(),
        2
    );
    assert_eq!(
        f.log()
            .iter()
            .filter(|row| row["kind"] == "hold_cleared")
            .count(),
        2
    );
}

#[test]
fn routing_log_failure_warns_after_the_hold_has_been_saved() {
    let f = Fixture::new();
    let path = f.transcript("held");
    append(&path, &fable_rejection(Utc::now(), None));
    fs::create_dir(f.paths.state_dir.join("routing.jsonl")).unwrap();
    let result = f.run(&["session", "held", "-j"]);
    assert!(String::from_utf8_lossy(&result.stderr).contains("routing log could not be saved"));
    assert_eq!(f.evidence().holds.len(), 1);
    let replay = f.run(&["session", "held", "-j"]);
    assert!(replay.stderr.is_empty());
}
