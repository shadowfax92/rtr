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

#[test]
fn bypassed_run_removes_inherited_home_without_touching_profile_or_default_home() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let marker = temp.path().join("child-home.txt");
    let user_home = temp.path().join("user-home");
    std::fs::create_dir(&user_home).unwrap();
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["sh", "-c", "printf '%s' \"${{CODEX_HOME-unset}}\" > \"$1\"", "runner", {}]
skills_source = {}

[tools.codex.profiles.personal]
bypass = true
"#,
            toml_path(&marker),
            toml_path(&temp.path().join("must-not-be-read"))
        ),
    );

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rtr"))
        .args(["codex", "--profile", "personal"])
        .env("HOME", &user_home)
        .env("RTR_CONFIG_DIR", &paths.config_dir)
        .env("RTR_STATE_DIR", &paths.state_dir)
        .env("CODEX_HOME", "/inherited/bad-profile")
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        concat!(
            "rtr: bypass codex/personal — launching codex with its default home (no CODEX_HOME; undo: rtr unbypass codex --profile personal)\n",
            "rtr: codex ran in profile 'personal' — resume: rtr codex -p personal resume\n"
        )
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "unset");
    assert!(!paths.profile_home_dir("codex", "personal").exists());
    assert!(!user_home.join(".codex").exists());
    let events = usage::read_events(&paths.usage_file()).unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].bypass);
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
async fn claude_profiles_isolate_config_and_secure_storage_with_shared_skills() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let source = temp.path().join("shared-skills");
    std::fs::create_dir_all(source.join("shared")).unwrap();
    std::fs::write(source.join("shared/SKILL.md"), "shared instructions").unwrap();
    let work_marker = temp.path().join("work-home");
    let personal_marker = temp.path().join("personal-home");
    write_config(
        &paths,
        &format!(
            r#"
[tools.claude]
command = ["sh", "-c", "test \"$CLAUDE_CONFIG_DIR\" = \"$CLAUDE_SECURESTORAGE_CONFIG_DIR\" || exit 19; test -f \"$CLAUDE_CONFIG_DIR/skills/shared/SKILL.md\" || exit 20; if [ \"$1\" = write ]; then printf work > \"$CLAUDE_CONFIG_DIR/.claude.json\"; else test ! -e \"$CLAUDE_CONFIG_DIR/.claude.json\" || exit 21; fi; printf '%s' \"$CLAUDE_CONFIG_DIR\" > \"$2\"", "runner"]
skills_source = {}

[tools.claude.profiles.work]

[tools.claude.profiles.personal]
"#,
            toml_path(&source)
        ),
    );

    let work_code = runner::run_subscription_tool(
        &paths,
        "claude",
        Some("work"),
        &["write".to_string(), work_marker.display().to_string()],
    )
    .await
    .unwrap();
    let personal_code = runner::run_subscription_tool(
        &paths,
        "claude",
        Some("personal"),
        &["check".to_string(), personal_marker.display().to_string()],
    )
    .await
    .unwrap();

    assert_eq!(work_code, 0);
    assert_eq!(personal_code, 0);
    assert_eq!(
        std::fs::read_to_string(work_marker).unwrap(),
        paths
            .profile_home_dir("claude", "work")
            .display()
            .to_string()
    );
    assert_eq!(
        std::fs::read_to_string(personal_marker).unwrap(),
        paths
            .profile_home_dir("claude", "personal")
            .display()
            .to_string()
    );
    assert!(paths
        .profile_home_dir("claude", "work")
        .join(".claude.json")
        .is_file());
    assert!(!paths
        .profile_home_dir("claude", "personal")
        .join(".claude.json")
        .exists());
    for profile in ["work", "personal"] {
        assert_eq!(
            std::fs::read_to_string(
                paths
                    .profile_home_dir("claude", profile)
                    .join("skills/shared/SKILL.md")
            )
            .unwrap(),
            "shared instructions"
        );
    }
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
async fn add_profile_persists_native_home_and_launches_login() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let source = temp.path().join("shared-skills");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("shared.md"), "shared").unwrap();
    let marker = paths.state_dir.join("codex-home.txt");
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["sh", "-c", "printf '%s' \"$CODEX_HOME\" > {}"]
skills_source = {}
"#,
            marker.display(),
            toml_path(&source)
        ),
    );

    let code = runner::add_subscription_profile(&paths, "codex", "personal")
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        paths
            .profile_home_dir("codex", "personal")
            .display()
            .to_string()
    );
    assert_eq!(
        std::fs::read_to_string(
            paths
                .profile_home_dir("codex", "personal")
                .join("skills/shared.md")
        )
        .unwrap(),
        "shared"
    );
    let config = rtr::config::Config::load(&paths.config_file()).unwrap();
    assert!(config
        .tool("codex")
        .unwrap()
        .profiles
        .contains_key("personal"));
}

#[tokio::test]
async fn add_profile_rejects_duplicates_before_home_mutation_or_launch() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let source = temp.path().join("shared-skills");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("new.md"), "new").unwrap();
    let profile_home = paths.profile_home_dir("claude", "work");
    std::fs::create_dir_all(profile_home.join("skills")).unwrap();
    std::fs::write(profile_home.join("skills/stale.md"), "stale").unwrap();
    let marker = temp.path().join("launched");
    let config = format!(
        r#"
[tools.claude]
command = ["sh", "-c", "touch {}"]
skills_source = {}

[tools.claude.profiles.work]
enabled = true
"#,
        marker.display(),
        toml_path(&source)
    );
    write_config(&paths, &config);

    let error = runner::add_subscription_profile(&paths, "claude", "work")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("already exists"), "{error}");
    assert!(error.contains("rtr claude --profile work"), "{error}");
    assert!(!marker.exists());
    assert_eq!(
        std::fs::read_to_string(profile_home.join("skills/stale.md")).unwrap(),
        "stale"
    );
    assert_eq!(
        std::fs::read_to_string(paths.config_file()).unwrap(),
        config
    );
}

#[tokio::test]
async fn add_profile_rejects_unsupported_tools() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let error = runner::add_subscription_profile(&paths, "curl", "work")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("unsupported subscription tool 'curl'"),
        "{error}"
    );
    assert!(error.contains("supported: claude, codex"), "{error}");
}

#[test]
fn rm_command_deletes_only_the_confirmed_profile() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    write_config(
        &paths,
        r#"# keep this comment
[tools.codex]
command = ["codex"]

[tools.codex.profiles.personal]

[tools.codex.profiles.work]
"#,
    );
    for profile in ["personal", "work"] {
        let home = paths.ensure_profile_home_dir("codex", profile).unwrap();
        std::fs::write(home.join("auth.json"), profile).unwrap();
    }

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rtr"))
        .args(["rm", "codex", "--profile", "personal", "--yes"])
        .env("RTR_CONFIG_DIR", &paths.config_dir)
        .env("RTR_STATE_DIR", &paths.state_dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(&format!(
            "Profile home to delete: {}",
            paths.profile_home_dir("codex", "personal").display()
        )),
        "{stdout}"
    );
    let config = std::fs::read_to_string(paths.config_file()).unwrap();
    assert!(config.contains("# keep this comment"), "{config}");
    assert!(!config.contains("profiles.personal"), "{config}");
    assert!(config.contains("profiles.work"), "{config}");
    assert!(!paths.profile_home_dir("codex", "personal").exists());
    assert!(paths
        .profile_home_dir("codex", "work")
        .join("auth.json")
        .is_file());
}

#[test]
fn config_command_prints_only_the_resolved_path() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rtr"))
        .arg("config")
        .env("RTR_CONFIG_DIR", &paths.config_dir)
        .env("RTR_STATE_DIR", &paths.state_dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{}\n", paths.config_file().display())
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn config_edit_passes_the_path_to_editor_and_propagates_status() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    write_config(&paths, "[tools.codex]\ncommand = [\"codex\"]\n");
    let marker = temp.path().join("editor-argument.txt");
    let editor = temp.path().join("editor");
    std::fs::write(
        &editor,
        "#!/bin/sh\nprintf '%s' \"$1\" > \"$RTR_EDITOR_MARKER\"\nexit 23\n",
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o700)).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rtr"))
        .args(["config", "edit"])
        .env("RTR_CONFIG_DIR", &paths.config_dir)
        .env("RTR_STATE_DIR", &paths.state_dir)
        .env_remove("VISUAL")
        .env("EDITOR", &editor)
        .env("RTR_EDITOR_MARKER", &marker)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(23), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        paths.config_file().display().to_string()
    );
}

#[test]
fn config_edit_missing_file_points_to_init_before_editor_lookup() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rtr"))
        .args(["config", "edit"])
        .env("RTR_CONFIG_DIR", &paths.config_dir)
        .env("RTR_STATE_DIR", &paths.state_dir)
        .env_remove("VISUAL")
        .env_remove("EDITOR")
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("rtr init"), "{stderr}");
    assert!(!stderr.contains("EDITOR is set"), "{stderr}");
}

#[tokio::test]
async fn fix_profile_repairs_only_the_selected_home_without_moving_rotation() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let skills = empty_skills_source(temp.path());
    let marker = temp.path().join("fixed-home.txt");
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["sh", "-c", "printf '%s' \"$CODEX_HOME\" > {}; exit 6"]
skills_source = {}

[tools.codex.profiles.personal]
enabled = false

[tools.codex.profiles.work]
"#,
            marker.display(),
            toml_path(&skills)
        ),
    );
    let personal = paths.ensure_profile_home_dir("codex", "personal").unwrap();
    let work = paths.ensure_profile_home_dir("codex", "work").unwrap();
    std::fs::write(personal.join("auth.json"), "credentials").unwrap();
    std::fs::create_dir(personal.join("sessions")).unwrap();
    std::fs::write(personal.join("sessions/thread.jsonl"), "session").unwrap();
    std::fs::write(personal.join("auth.json.lock"), "stale").unwrap();
    std::fs::write(work.join("auth.json.lock"), "other").unwrap();
    let mut state = State::default();
    state.set_round_robin_cursor("codex", 7);
    state.save(&paths.state_file()).unwrap();

    let code = runner::fix_subscription_profile(&paths, "codex", "personal")
        .await
        .unwrap();

    assert_eq!(code, 6);
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        personal.display().to_string()
    );
    assert!(!personal.join("auth.json.lock").exists());
    assert_eq!(
        std::fs::read_to_string(personal.join("auth.json")).unwrap(),
        "credentials"
    );
    assert_eq!(
        std::fs::read_to_string(personal.join("sessions/thread.jsonl")).unwrap(),
        "session"
    );
    assert!(work.join("auth.json.lock").is_file());
    assert_eq!(
        State::load(&paths.state_file())
            .unwrap()
            .round_robin_cursor("codex"),
        7
    );
    let events = usage::read_events(&paths.usage_file()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].profile, "personal");
    assert_eq!(events[0].exit_code, Some(6));
}

#[tokio::test]
async fn fix_profile_rejects_unknown_profile_with_add_suggestion() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    write_config(
        &paths,
        "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.personal]\n",
    );
    let personal = paths.ensure_profile_home_dir("codex", "personal").unwrap();
    std::fs::write(personal.join("auth.json.lock"), "keep").unwrap();

    let error = runner::fix_subscription_profile(&paths, "codex", "missing")
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("rtr add codex --profile missing"), "{error}");
    assert!(!paths.profile_home_dir("codex", "missing").exists());
    assert!(personal.join("auth.json.lock").is_file());
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
async fn disable_and_enable_round_trip_controls_selection_and_keeps_native_state() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let skills = empty_skills_source(temp.path());
    let marker = temp.path().join("homes.txt");
    write_config(
        &paths,
        &format!(
            r#"# hand-written comment
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

    for _ in 0..2 {
        assert_eq!(
            runner::run_subscription_tool(&paths, "codex", None, &[])
                .await
                .unwrap(),
            0
        );
    }
    let credential_marker = paths.profile_home_dir("codex", "a").join("auth.json");
    std::fs::write(&credential_marker, "keep me").unwrap();
    let cursor_before = State::load(&paths.state_file())
        .unwrap()
        .round_robin_cursor("codex");

    let report = rtr::profiles::set_profile_enabled(&paths, "codex", "a", false).unwrap();
    assert!(report.changed);
    assert_eq!(
        State::load(&paths.state_file())
            .unwrap()
            .round_robin_cursor("codex"),
        cursor_before
    );

    assert_eq!(
        runner::run_subscription_tool(&paths, "codex", None, &[])
            .await
            .unwrap(),
        0
    );
    let forced = runner::run_subscription_tool(&paths, "codex", Some("a"), &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(forced.contains("profile 'codex/a' is disabled"), "{forced}");

    assert!(
        rtr::profiles::set_profile_enabled(&paths, "codex", "a", true)
            .unwrap()
            .changed
    );
    assert_eq!(
        runner::run_subscription_tool(&paths, "codex", None, &[])
            .await
            .unwrap(),
        0
    );

    let homes = std::fs::read_to_string(&marker).unwrap();
    let expected: Vec<String> = ["a", "b", "b", "a"]
        .iter()
        .map(|profile| {
            paths
                .profile_home_dir("codex", profile)
                .display()
                .to_string()
        })
        .collect();
    assert_eq!(homes, format!("{}\n", expected.join("\n")));
    assert_eq!(
        std::fs::read_to_string(&credential_marker).unwrap(),
        "keep me"
    );
    let config_text = std::fs::read_to_string(paths.config_file()).unwrap();
    assert!(
        config_text.contains("# hand-written comment"),
        "{config_text}"
    );
    assert!(config_text.contains("enabled = true"), "{config_text}");
}

#[tokio::test]
async fn disabling_the_last_enabled_profile_blocks_runs_until_reenabled() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let skills = empty_skills_source(temp.path());
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["true"]
skills_source = {}

[tools.codex.profiles.only]
"#,
            toml_path(&skills)
        ),
    );

    let report = rtr::profiles::set_profile_enabled(&paths, "codex", "only", false).unwrap();
    assert!(report.changed);
    assert_eq!(report.tool_enabled_remaining, 0);

    let error = runner::run_subscription_tool(&paths, "codex", None, &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("no enabled profiles"), "{error}");
    assert!(usage::read_events(&paths.usage_file()).unwrap().is_empty());

    assert!(
        rtr::profiles::set_profile_enabled(&paths, "codex", "only", true)
            .unwrap()
            .changed
    );
    assert_eq!(
        runner::run_subscription_tool(&paths, "codex", None, &[])
            .await
            .unwrap(),
        0
    );
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

#[tokio::test]
async fn usage_write_failure_does_not_replace_child_exit() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let skills = empty_skills_source(temp.path());
    std::fs::create_dir_all(paths.usage_file()).unwrap();
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["sh", "-c", "exit 7"]
skills_source = {}

[tools.codex.profiles.personal]
"#,
            toml_path(&skills)
        ),
    );

    let code = runner::run_subscription_tool(&paths, "codex", Some("personal"), &[])
        .await
        .unwrap();
    assert_eq!(code, 7);
}

#[test]
fn signal_to_rtr_is_forwarded_and_usage_is_recorded() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let skills = empty_skills_source(temp.path());
    let child_pid_file = temp.path().join("child.pid");
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["sh", "-c", "trap 'exit 42' TERM; echo $$ > {}; while :; do sleep 1; done"]
skills_source = {}

[tools.codex.profiles.personal]
"#,
            child_pid_file.display(),
            toml_path(&skills)
        ),
    );

    let mut rtr = std::process::Command::new(env!("CARGO_BIN_EXE_rtr"))
        .args(["codex", "--profile", "personal"])
        .env("RTR_CONFIG_DIR", &paths.config_dir)
        .env("RTR_STATE_DIR", &paths.state_dir)
        .spawn()
        .unwrap();

    for _ in 0..100 {
        if child_pid_file.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(child_pid_file.exists(), "child did not start");
    let child_pid: i32 = std::fs::read_to_string(&child_pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    assert_eq!(unsafe { libc::kill(rtr.id() as i32, libc::SIGTERM) }, 0);
    let status = rtr.wait().unwrap();
    if status.code() != Some(42) {
        unsafe {
            libc::kill(child_pid, libc::SIGKILL);
        }
    }
    assert_eq!(status.code(), Some(42));

    let events = usage::read_events(&paths.usage_file()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].profile, "personal");
    assert_eq!(events[0].exit_code, Some(42));
}

#[cfg(unix)]
#[test]
fn terminal_interrupt_reaches_child_once_and_foreground_is_restored() {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;

    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let skills = empty_skills_source(temp.path());
    let ready = temp.path().join("ready");
    let interrupts = temp.path().join("interrupts");
    let child_pid_file = temp.path().join("child.pid");
    write_config(
        &paths,
        &format!(
            r#"
[tools.codex]
command = ["sh", "-c", "trap 'printf x >> {}' INT; trap 'exit 42' TERM; echo $$ > {}; touch {}; while :; do sleep 1; done"]
skills_source = {}

[tools.codex.profiles.personal]
"#,
            interrupts.display(),
            child_pid_file.display(),
            ready.display(),
            toml_path(&skills)
        ),
    );

    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: `openpty` initializes the two valid fd outputs; optional metadata is unused.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: ownership of the fresh `openpty` descriptors transfers to these files.
    let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_rtr"));
    command
        .args(["codex", "--profile", "personal"])
        .env("RTR_CONFIG_DIR", &paths.config_dir)
        .env("RTR_STATE_DIR", &paths.state_dir)
        .stdin(std::process::Stdio::from(slave.try_clone().unwrap()))
        .stdout(std::process::Stdio::from(slave.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: this runs after fork and before exec, creating a session and making fd 0 its tty.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY.into(), 0) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut rtr = command.spawn().unwrap();
    drop(slave);

    for _ in 0..100 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready.exists(), "child did not become ready");
    let child_pid: i32 = std::fs::read_to_string(&child_pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::tcgetpgrp(master.as_raw_fd()) }, child_pid);

    master.write_all(&[3]).unwrap();
    for _ in 0..50 {
        if interrupts.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    let received = std::fs::read_to_string(&interrupts).unwrap_or_default();

    assert_eq!(unsafe { libc::kill(-child_pid, libc::SIGTERM) }, 0);
    let mut terminal_restored = false;
    for _ in 0..100 {
        if unsafe { libc::tcgetpgrp(master.as_raw_fd()) } == rtr.id() as i32 {
            terminal_restored = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    drop(master);

    let mut status = None;
    for _ in 0..100 {
        status = rtr.try_wait().unwrap();
        if status.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if status.is_none() {
        unsafe {
            libc::kill(rtr.id() as i32, libc::SIGKILL);
            libc::kill(-child_pid, libc::SIGKILL);
        }
        status = Some(rtr.wait().unwrap());
    }

    assert_eq!(
        received, "x",
        "one terminal Ctrl-C must produce one interrupt"
    );
    assert!(terminal_restored, "rtr did not reclaim the foreground tty");
    assert_eq!(status.unwrap().code(), Some(42));
    assert_eq!(
        usage::read_events(&paths.usage_file()).unwrap()[0].exit_code,
        Some(42)
    );
}
