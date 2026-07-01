//! Smoke test for `rtr run`: drives the runner with a trivial tool and an
//! ephemeral proxy port, asserting the proxy boots, output is tee'd, captures
//! land, and the child's exit code propagates.

use rtr::paths::Paths;
use rtr::runner;
use rtr::state::State;
use rtr::usage;
use rtr::{
    config::{self, Config},
    import::{self, ConflictPolicy},
};

#[tokio::test]
async fn run_tool_tees_output_and_propagates_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();

    // port = 0 -> ephemeral bind, no collision across parallel tests.
    let cfg = r#"
[proxy]
port = 0

[tools.echotool]
command = ["sh", "-c", "echo hello-from-child; echo errline 1>&2; exit 3"]
hosts = []
"#;
    std::fs::write(paths.config_file(), cfg).unwrap();

    let code = runner::run_tool(&paths, "echotool", &[], false, true)
        .await
        .unwrap();
    assert_eq!(code, 3, "child exit code should propagate");

    let runs = paths.runs_dir().join("echotool");
    let run_dir = std::fs::read_dir(&runs)
        .expect("run dir created")
        .next()
        .unwrap()
        .unwrap()
        .path();

    let out = std::fs::read_to_string(run_dir.join("output.log")).unwrap();
    assert!(out.contains("hello-from-child"), "output.log: {out}");
    assert!(out.contains("errline"), "output.log: {out}");
    assert!(
        run_dir.join("capture.jsonl").exists(),
        "capture.jsonl missing"
    );

    // The run dir and capture file hold real tokens in normal use: owner-only.
    use std::os::unix::fs::PermissionsExt;
    let dir_mode = std::fs::metadata(&run_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "run dir perms {dir_mode:o}");
    let cap_mode = std::fs::metadata(run_dir.join("capture.jsonl"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(cap_mode, 0o600, "capture.jsonl perms {cap_mode:o}");
}

#[tokio::test]
async fn subscription_run_uses_profile_preset_args_and_records_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();

    let cfg = r#"
[proxy]
port = 0

[tools.codex]
command = ["sh", "-c", "printf '%s\\n' \"$@\"; exit 6", "runner", "base"]
hosts = []
default_preset = "p"

[tools.codex.presets.p]
args = ["preset"]

[tools.codex.profiles.personal]
set = { Authorization = "Bearer token", chatgpt-account-id = "acct" }
"#;
    std::fs::write(paths.config_file(), cfg).unwrap();

    let code = runner::run_subscription_tool(
        &paths,
        "codex",
        Some("personal"),
        None,
        &["extra".to_string()],
        false,
        true,
    )
    .await
    .unwrap();
    assert_eq!(code, 6);

    let run_dir = std::fs::read_dir(paths.runs_dir().join("codex"))
        .expect("run dir created")
        .next()
        .unwrap()
        .unwrap()
        .path();
    let out = std::fs::read_to_string(run_dir.join("output.log")).unwrap();
    assert_eq!(out, "base\npreset\nextra\n");

    let events = usage::read_events(&paths.usage_file()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tool, "codex");
    assert_eq!(events[0].profile, "personal");
    assert_eq!(events[0].preset.as_deref(), Some("p"));
    assert_eq!(events[0].exit_code, Some(6));
}

#[tokio::test]
async fn subscription_run_rejects_profile_missing_required_rewrites() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();

    let cfg = r#"
[proxy]
port = 0

[tools.codex]
command = ["sh", "-c", "exit 0"]
hosts = []

[tools.codex.profiles.incomplete]
set = { Authorization = "Bearer token" }
"#;
    std::fs::write(paths.config_file(), cfg).unwrap();

    let err =
        runner::run_subscription_tool(&paths, "codex", Some("incomplete"), None, &[], false, false)
            .await
            .unwrap_err()
            .to_string();
    assert!(err.contains("codex/incomplete"), "got: {err}");
    assert!(err.contains("chatgpt-account-id"), "got: {err}");
    assert!(!paths.runs_dir().join("codex").exists());
}

#[tokio::test]
async fn subscription_run_does_not_persist_round_robin_on_preflight_error() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();

    let cfg = r#"
[proxy]
port = 0

[tools.codex]
command = ["sh", "-c", "exit 0"]
hosts = []

[tools.codex.profiles.bad]
set = { Authorization = "Bearer token" }

[tools.codex.profiles.next]
set = { Authorization = "Bearer token", chatgpt-account-id = "acct" }
"#;
    std::fs::write(paths.config_file(), cfg).unwrap();

    let err = runner::run_subscription_tool(&paths, "codex", None, None, &[], false, false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("codex/bad"), "got: {err}");

    let state = State::load(&paths.state_file()).unwrap();
    assert_eq!(state.round_robin_cursor("codex"), 0);
    assert!(!paths.runs_dir().join("codex").exists());
}

#[tokio::test]
async fn subscription_run_uses_spec_hosts_even_when_config_is_wildcard() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();

    let cfg = r#"
[proxy]
port = 0

[tools.codex]
command = ["sh", "-c", "curl --silent --show-error --max-time 1 http://127.0.0.1:1/rtr-offscope >/dev/null 2>&1 || true"]
hosts = ["*"]

[tools.codex.profiles.personal]
set = { Authorization = "Bearer token", chatgpt-account-id = "acct" }
"#;
    std::fs::write(paths.config_file(), cfg).unwrap();

    let code =
        runner::run_subscription_tool(&paths, "codex", Some("personal"), None, &[], false, false)
            .await
            .unwrap();
    assert_eq!(code, 0);

    let run_dir = std::fs::read_dir(paths.runs_dir().join("codex"))
        .expect("run dir created")
        .next()
        .unwrap()
        .unwrap()
        .path();
    let capture = std::fs::read_to_string(run_dir.join("capture.jsonl")).unwrap();
    assert!(
        capture.trim().is_empty(),
        "capture should be empty: {capture}"
    );
}

#[tokio::test]
async fn starter_imported_profile_runs_unforced() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();

    let mut cfg = Config::parse(config::STARTER_CONFIG).unwrap();
    cfg.proxy.port = 0;
    cfg.tool_mut("codex").unwrap().command =
        vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()];
    config::write_secret_file(&paths.config_file(), &cfg.to_toml().unwrap()).unwrap();

    let capture_path = paths.state_dir.join("capture.jsonl");
    std::fs::write(
        &capture_path,
        r#"{"ts":"2026-07-01T12:00:00Z","method":"GET","url":"https://chatgpt.com/backend-api/codex/models","host":"chatgpt.com","headers":[["authorization","Bearer token"],["chatgpt-account-id","acct"]]}"#,
    )
    .unwrap();
    import::run_import_profile(
        &paths,
        "codex",
        "personal",
        &capture_path,
        ConflictPolicy::Reject,
        false,
    )
    .unwrap();

    let code = runner::run_subscription_tool(&paths, "codex", None, None, &[], false, false)
        .await
        .unwrap();
    assert_eq!(code, 0);

    let events = usage::read_events(&paths.usage_file()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].profile, "personal");
}
