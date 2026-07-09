use std::path::{Path, PathBuf};

use rtr::paths::Paths;
use rtr::runner;
use rtr::state::State;
use rtr::usage;

fn toml_path(path: &Path) -> String {
    toml::Value::String(path.display().to_string()).to_string()
}

fn test_paths(root: &Path) -> Paths {
    Paths {
        config_dir: root.join("config"),
        state_dir: root.join("state"),
    }
}

fn empty_skills_source(root: &Path) -> PathBuf {
    let source = root.join("skills");
    std::fs::create_dir_all(&source).unwrap();
    source
}

fn write_config(paths: &Paths, text: &str) {
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(paths.config_file(), text).unwrap();
}

#[tokio::test]
async fn direct_run_forwards_native_home_arguments_exit_and_usage_without_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let skills = empty_skills_source(temp.path());
    let marker = temp.path().join("child.txt");
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["sh", "-c", "printf 'home=%s\n' \"$CODEX_HOME\" > {}; printf '%s\n' \"$@\" >> {}; exit 6", "runner", "base"]
skills_source = {}

[tools.codex.profiles.personal]
"#,
            marker.display(),
            marker.display(),
            toml_path(&skills)
        ),
    );

    let code = runner::run_subscription_tool(
        &paths,
        "codex",
        Some("personal"),
        &["--model".into(), "gpt-5.5".into()],
    )
    .await
    .unwrap();
    assert_eq!(code, 6);
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        format!(
            "home={}\nbase\n--model\ngpt-5.5\n",
            paths.profile_home_dir("codex", "personal").display()
        )
    );

    let events = usage::read_events(&paths.usage_file()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tool, "codex");
    assert_eq!(events[0].profile, "personal");
    assert_eq!(events[0].exit_code, Some(6));
    assert!(!paths.state_dir.join("runs").exists());
}

#[tokio::test]
async fn claude_run_sets_claude_config_dir_and_refreshes_skills() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let source = temp.path().join("shared-skills");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(source.join("root.md"), "root").unwrap();
    std::fs::write(source.join("nested/child.md"), "child").unwrap();
    let profile_home = paths.profile_home_dir("claude", "work");
    std::fs::create_dir_all(profile_home.join("skills")).unwrap();
    std::fs::write(profile_home.join("skills/stale.md"), "stale").unwrap();
    let marker = temp.path().join("claude-home.txt");
    write_config(
        &paths,
        &format!(
            r#"
[tools.claude]
command = ["sh", "-c", "printf '%s' \"$CLAUDE_CONFIG_DIR\" > {}"]
skills_source = {}

[tools.claude.profiles.work]
"#,
            marker.display(),
            toml_path(&source)
        ),
    );

    assert_eq!(
        runner::run_subscription_tool(&paths, "claude", Some("work"), &[])
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        profile_home.display().to_string()
    );
    assert_eq!(
        std::fs::read_to_string(profile_home.join("skills/root.md")).unwrap(),
        "root"
    );
    assert_eq!(
        std::fs::read_to_string(profile_home.join("skills/nested/child.md")).unwrap(),
        "child"
    );
    assert!(!profile_home.join("skills/stale.md").exists());
}

#[tokio::test]
async fn automatic_runs_rotate_profiles_and_record_each_selection() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let skills = empty_skills_source(temp.path());
    let marker = temp.path().join("homes.txt");
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["sh", "-c", "printf '%s\n' \"$CODEX_HOME\" >> {}"]
skills_source = {}

[tools.codex.profiles.a]
[tools.codex.profiles.b]
"#,
            marker.display(),
            toml_path(&skills)
        ),
    );

    assert_eq!(
        runner::run_subscription_tool(&paths, "codex", None, &[])
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        runner::run_subscription_tool(&paths, "codex", None, &[])
            .await
            .unwrap(),
        0
    );
    let homes = std::fs::read_to_string(marker).unwrap();
    assert_eq!(
        homes,
        format!(
            "{}\n{}\n",
            paths.profile_home_dir("codex", "a").display(),
            paths.profile_home_dir("codex", "b").display()
        )
    );
    assert_eq!(
        usage::read_events(&paths.usage_file())
            .unwrap()
            .into_iter()
            .map(|event| event.profile)
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[tokio::test]
async fn preflight_error_does_not_advance_rotation_or_launch_child() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let missing = temp.path().join("missing-skills");
    let marker = temp.path().join("launched");
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["sh", "-c", "touch {}"]
skills_source = {}

[tools.codex.profiles.a]
[tools.codex.profiles.b]
"#,
            marker.display(),
            toml_path(&missing)
        ),
    );

    let error = runner::run_subscription_tool(&paths, "codex", None, &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("configured skills source"), "{error}");
    assert!(!marker.exists());
    assert_eq!(
        State::load(&paths.state_file())
            .unwrap()
            .round_robin_cursor("codex"),
        0
    );
    assert!(usage::read_events(&paths.usage_file()).unwrap().is_empty());
}

#[tokio::test]
async fn disabled_forced_profile_is_rejected_before_home_creation() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    write_config(
        &paths,
        r#"
[tools.codex]
command = ["true"]

[tools.codex.profiles.personal]
enabled = false
"#,
    );

    let error = runner::run_subscription_tool(&paths, "codex", Some("personal"), &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("codex/personal"), "{error}");
    assert!(error.contains("disabled"), "{error}");
    assert!(!paths.profile_home_dir("codex", "personal").exists());
}

#[tokio::test]
async fn spawn_errors_are_actionable_and_recorded_as_unknown_exit() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let skills = empty_skills_source(temp.path());
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["rtr-test-command-that-does-not-exist"]
skills_source = {}

[tools.codex.profiles.personal]
"#,
            toml_path(&skills)
        ),
    );

    let error = runner::run_subscription_tool(&paths, "codex", Some("personal"), &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("spawning 'rtr-test-command-that-does-not-exist'"),
        "{error}"
    );
    let events = usage::read_events(&paths.usage_file()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].exit_code, None);
}

#[tokio::test]
async fn signal_terminated_children_use_shell_exit_codes() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let skills = empty_skills_source(temp.path());
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["sh", "-c", "kill -TERM $$"]
skills_source = {}

[tools.codex.profiles.personal]
"#,
            toml_path(&skills)
        ),
    );

    let code = runner::run_subscription_tool(&paths, "codex", Some("personal"), &[])
        .await
        .unwrap();
    assert_eq!(code, 143);
    assert_eq!(
        usage::read_events(&paths.usage_file()).unwrap()[0].exit_code,
        Some(143)
    );
}

#[tokio::test]
async fn missing_config_points_to_init() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let error = runner::run_subscription_tool(&paths, "codex", None, &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("rtr init"), "{error}");
}
