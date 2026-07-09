//! Smoke test for `rtr run`: drives the runner with a trivial tool and an
//! ephemeral proxy port, asserting the proxy boots, output is optionally tee'd,
//! default runs are artifact-free, and the child's exit code propagates.

use rtr::paths::Paths;
use rtr::runner;
use rtr::state::State;
use rtr::usage;
use rtr::{
    config::{self, Config},
    import::{self, ConflictPolicy},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn read_http_head(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn toml_path(path: &std::path::Path) -> String {
    toml::Value::String(path.display().to_string()).to_string()
}

fn empty_skills_source(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let source = tmp.path().join("empty-skills");
    std::fs::create_dir_all(&source).unwrap();
    source
}

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

    let code = runner::run_tool(&paths, "echotool", &[], true)
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
    assert!(!run_dir.join("capture.jsonl").exists());

    use std::os::unix::fs::PermissionsExt;
    let dir_mode = std::fs::metadata(&run_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "run dir perms {dir_mode:o}");
    let out_mode = std::fs::metadata(run_dir.join("output.log"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(out_mode, 0o600, "output.log perms {out_mode:o}");
}

#[tokio::test]
async fn run_tool_applies_legacy_active_profile_rewrites() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        let (mut sock, _) = upstream.accept().await.unwrap();
        let head = read_http_head(&mut sock).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
        let _ = sock.flush().await;
        let _ = tx.send(head);
    });

    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();

    let cfg = format!(
        r#"
[proxy]
port = 0

[tools.legacy]
command = ["curl", "--silent", "--show-error", "http://127.0.0.1:{up_port}/legacy"]
hosts = ["127.0.0.1"]
active = "personal"

[tools.legacy.profiles.personal]
set = {{ Authorization = "Bearer legacy" }}
"#
    );
    std::fs::write(paths.config_file(), cfg).unwrap();

    let code = runner::run_tool(&paths, "legacy", &[], false)
        .await
        .unwrap();
    assert_eq!(code, 0);
    let head = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .expect("upstream did not receive a request")
        .unwrap();
    assert!(
        head.to_lowercase().contains("authorization: bearer legacy"),
        "upstream head: {head}"
    );
    assert!(!paths.runs_dir().join("legacy").exists());
}

#[tokio::test]
async fn subscription_run_uses_profile_runtime_args_and_records_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    let skills_source = empty_skills_source(&tmp);
    std::fs::create_dir_all(&paths.config_dir).unwrap();

    let cfg = format!(
        r#"
[proxy]
port = 0

[tools.codex]
command = ["sh", "-c", "printf 'home=%s\\n' \"$CODEX_HOME\"; printf '%s\\n' \"$@\"; exit 6", "runner", "base"]
hosts = []
skills_source = {}

[tools.codex.profiles.personal]
set = {{}}
"#,
        toml_path(&skills_source)
    );
    std::fs::write(paths.config_file(), cfg).unwrap();

    let code = runner::run_subscription_tool(
        &paths,
        "codex",
        Some("personal"),
        &[
            "--model".to_string(),
            "gpt-5.5".to_string(),
            "-c".to_string(),
            "model_reasoning_effort=xhigh".to_string(),
        ],
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
    assert_eq!(
        out,
        format!(
            "home={}\nbase\n--model\ngpt-5.5\n-c\nmodel_reasoning_effort=xhigh\n",
            paths.profile_home_dir("codex", "personal").display()
        )
    );
    assert!(paths.profile_home_dir("codex", "personal").is_dir());

    let events = usage::read_events(&paths.usage_file()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tool, "codex");
    assert_eq!(events[0].profile, "personal");
    assert_eq!(events[0].exit_code, Some(6));
}

#[tokio::test]
async fn subscription_run_refreshes_configured_skills_source() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    let source = tmp.path().join("shared-skills");
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::create_dir_all(paths.profile_home_dir("codex", "personal").join("skills")).unwrap();
    std::fs::write(source.join("root.md"), "root").unwrap();
    std::fs::write(source.join("nested").join("child.md"), "child").unwrap();
    std::fs::write(
        paths
            .profile_home_dir("codex", "personal")
            .join("skills")
            .join("stale.md"),
        "stale",
    )
    .unwrap();

    let marker = paths.state_dir.join("skills-ok");
    let cfg = format!(
        r#"
[proxy]
port = 0

[tools.codex]
command = ["sh", "-c", "test -f \"$CODEX_HOME/skills/root.md\" && test -f \"$CODEX_HOME/skills/nested/child.md\" && test ! -e \"$CODEX_HOME/skills/stale.md\" && printf ok > {}"]
hosts = []
skills_source = {}

[tools.codex.profiles.personal]
set = {{}}
"#,
        marker.display(),
        toml_path(&source)
    );
    std::fs::write(paths.config_file(), cfg).unwrap();

    let code = runner::run_subscription_tool(&paths, "codex", Some("personal"), &[], false)
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "ok");
    let dest = paths.profile_home_dir("codex", "personal").join("skills");
    assert_eq!(
        std::fs::read_to_string(dest.join("root.md")).unwrap(),
        "root"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("nested").join("child.md")).unwrap(),
        "child"
    );
    assert!(!dest.join("stale.md").exists());
}

#[tokio::test]
async fn subscription_run_rejects_missing_configured_skills_source_before_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::create_dir_all(paths.profile_home_dir("codex", "personal").join("skills")).unwrap();
    std::fs::write(
        paths
            .profile_home_dir("codex", "personal")
            .join("skills")
            .join("stale.md"),
        "stale",
    )
    .unwrap();
    let marker = paths.state_dir.join("launched");
    let missing = tmp.path().join("missing-skills");

    let cfg = format!(
        r#"
[proxy]
port = 0

[tools.codex]
command = ["sh", "-c", "printf launched > {}"]
hosts = []
skills_source = {}

[tools.codex.profiles.personal]
set = {{}}
"#,
        marker.display(),
        toml_path(&missing)
    );
    std::fs::write(paths.config_file(), cfg).unwrap();

    let err = runner::run_subscription_tool(&paths, "codex", Some("personal"), &[], false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("configured skills source"), "got: {err}");
    assert!(!marker.exists());
    assert!(
        paths
            .profile_home_dir("codex", "personal")
            .join("skills")
            .join("stale.md")
            .exists(),
        "configured-source errors should not delete existing skills"
    );
}

#[tokio::test]
async fn subscription_run_sets_claude_config_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    let skills_source = empty_skills_source(&tmp);
    std::fs::create_dir_all(&paths.config_dir).unwrap();

    let cfg = format!(
        r#"
[proxy]
port = 0

[tools.claude]
command = ["sh", "-c", "printf 'home=%s\\n' \"$CLAUDE_CONFIG_DIR\""]
hosts = []
skills_source = {}

[tools.claude.profiles.work]
set = {{}}
"#,
        toml_path(&skills_source)
    );
    std::fs::write(paths.config_file(), cfg).unwrap();

    let code = runner::run_subscription_tool(&paths, "claude", Some("work"), &[], true)
        .await
        .unwrap();
    assert_eq!(code, 0);

    let run_dir = std::fs::read_dir(paths.runs_dir().join("claude"))
        .expect("run dir created")
        .next()
        .unwrap()
        .unwrap()
        .path();
    let out = std::fs::read_to_string(run_dir.join("output.log")).unwrap();
    assert_eq!(
        out,
        format!(
            "home={}\n",
            paths.profile_home_dir("claude", "work").display()
        )
    );
    assert!(paths.profile_home_dir("claude", "work").is_dir());
}

#[tokio::test]
async fn subscription_run_ignores_stored_rewrites_for_native_home_profiles() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    let skills_source = empty_skills_source(&tmp);
    std::fs::create_dir_all(&paths.config_dir).unwrap();

    let cfg = format!(
        r#"
[proxy]
port = 0

[tools.codex]
command = ["sh", "-c", "exit 0"]
hosts = []
skills_source = {}

[tools.codex.profiles.personal]
set = {{ "bad header" = "would fail if parsed", Authorization = "Bearer stale" }}
"#,
        toml_path(&skills_source)
    );
    std::fs::write(paths.config_file(), cfg).unwrap();

    let code = runner::run_subscription_tool(&paths, "codex", Some("personal"), &[], false)
        .await
        .unwrap();
    assert_eq!(code, 0);
}

#[tokio::test]
async fn subscription_run_rejects_forced_disabled_profile() {
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

[tools.codex.profiles.personal]
enabled = false
set = {}
"#;
    std::fs::write(paths.config_file(), cfg).unwrap();

    let err = runner::run_subscription_tool(&paths, "codex", Some("personal"), &[], false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("codex/personal"), "got: {err}");
    assert!(err.contains("disabled"), "got: {err}");
    assert!(!paths.profile_home_dir("codex", "personal").exists());
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
default_preset = "missing"

[tools.codex.profiles.bad]
set = {}

[tools.codex.profiles.next]
set = {}
"#;
    std::fs::write(paths.config_file(), cfg).unwrap();

    let err = runner::run_subscription_tool(&paths, "codex", None, &[], false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("preset config was removed"), "got: {err}");

    let state = State::load(&paths.state_file()).unwrap();
    assert_eq!(state.round_robin_cursor("codex"), 0);
    assert!(!paths.runs_dir().join("codex").exists());
}

#[tokio::test]
async fn subscription_run_uses_spec_hosts_without_creating_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: tmp.path().join("config"),
        state_dir: tmp.path().join("state"),
    };
    let skills_source = empty_skills_source(&tmp);
    std::fs::create_dir_all(&paths.config_dir).unwrap();

    let cfg = format!(
        r#"
[proxy]
port = 0

[tools.codex]
command = ["sh", "-c", "curl --silent --show-error --max-time 1 http://127.0.0.1:1/rtr-offscope >/dev/null 2>&1 || true"]
hosts = ["*"]
skills_source = {}

[tools.codex.profiles.personal]
set = {{ Authorization = "Bearer stale", chatgpt-account-id = "stale" }}
"#,
        toml_path(&skills_source)
    );
    std::fs::write(paths.config_file(), cfg).unwrap();

    let code = runner::run_subscription_tool(&paths, "codex", Some("personal"), &[], false)
        .await
        .unwrap();
    assert_eq!(code, 0);

    assert!(!paths.runs_dir().join("codex").exists());
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
    let skills_source = empty_skills_source(&tmp);

    let mut cfg = Config::parse(config::STARTER_CONFIG).unwrap();
    cfg.proxy.port = 0;
    cfg.tool_mut("codex").unwrap().command =
        vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()];
    cfg.tool_mut("codex").unwrap().skills_source = Some(skills_source);
    config::write_secret_file(&paths.config_file(), &cfg.to_toml().unwrap()).unwrap();

    let capture_path = paths.state_dir.join("capture.jsonl");
    std::fs::write(
        &capture_path,
        r#"{"ts":"2026-07-01T12:00:00Z","method":"GET","url":"https://chatgpt.com/backend-api/codex/models","host":"chatgpt.com","headers":[["accept","*/*"]]}"#,
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

    let code = runner::run_subscription_tool(&paths, "codex", None, &[], false)
        .await
        .unwrap();
    assert_eq!(code, 0);

    let cfg = Config::load(&paths.config_file()).unwrap();
    assert!(cfg
        .tool("codex")
        .unwrap()
        .profiles
        .get("personal")
        .unwrap()
        .set
        .is_empty());

    let events = usage::read_events(&paths.usage_file()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].profile, "personal");
}
