//! Discover resumable native sessions for the current directory.

use std::cmp::Reverse;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::config::Config;
use crate::paths::Paths;

const SESSION_LIMIT: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub tool: String,
    pub profile: String,
    pub enabled: bool,
    pub bypass: bool,
    pub id: String,
    pub cwd: PathBuf,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
struct ProfileState {
    enabled: bool,
    bypass: bool,
}

/// Find the five newest native sessions whose recorded cwd is exactly `cwd`.
pub fn recent_for_path(paths: &Paths, cwd: &Path, limit: usize) -> Result<Vec<Session>> {
    let config = Config::load(&paths.config_file())?;
    let mut sessions = Vec::new();

    for (tool_name, tool) in &config.tools {
        for (profile_name, profile) in &tool.profiles {
            let home = paths.profile_home_dir(tool_name, profile_name);
            let state = ProfileState {
                enabled: profile.enabled,
                bypass: profile.bypass,
            };
            match tool_name.as_str() {
                "claude" => scan_claude_home(&home, profile_name, state, cwd, &mut sessions)?,
                "codex" => scan_codex_home(&home, profile_name, state, cwd, &mut sessions)?,
                _ => {}
            }
        }
    }

    sessions.sort_by_key(|session| {
        (
            Reverse(session.updated_at),
            session.tool.clone(),
            session.profile.clone(),
            session.id.clone(),
        )
    });
    sessions.truncate(limit);
    Ok(sessions)
}

pub fn render(sessions: &[Session], cwd: &Path, now: DateTime<Utc>) -> String {
    if sessions.is_empty() {
        return format!("No Claude or Codex sessions found for {}.\n", cwd.display());
    }

    let tool_width = sessions
        .iter()
        .map(|session| session.tool.len())
        .max()
        .unwrap_or(0)
        .max("AGENT".len());
    let profile_width = sessions
        .iter()
        .map(|session| session.profile.len())
        .max()
        .unwrap_or(0)
        .max("PROFILE".len());
    let when_width = sessions
        .iter()
        .map(|session| relative_time(now, session.updated_at).len())
        .max()
        .unwrap_or(0)
        .max("UPDATED".len());
    let session_width = sessions
        .iter()
        .map(|session| session.id.len())
        .max()
        .unwrap_or(0)
        .max("SESSION".len());

    let mut output = format!("Recent sessions in {}\n", cwd.display());
    let _ = writeln!(
        output,
        "{:<tool_width$}  {:<profile_width$}  {:<when_width$}  {:<session_width$}  RESUME",
        "AGENT", "PROFILE", "UPDATED", "SESSION"
    );
    for session in sessions {
        let when = relative_time(now, session.updated_at);
        let profile = crate::runner::shell_quote(&session.profile);
        let mut commands = Vec::new();
        if !session.enabled {
            commands.push(format!("rtr enable {} --profile {profile}", session.tool));
        }
        if session.bypass {
            commands.push(format!("rtr unbypass {} --profile {profile}", session.tool));
        }
        commands.push(format!(
            "rtr {} -p {} {} {}",
            session.tool,
            profile,
            crate::tool_specs::get(&session.tool)
                .expect("session tool comes from a native tool scanner")
                .resume_args
                .join(" "),
            crate::runner::shell_quote(&session.id)
        ));
        let resume = commands.join(" && ");
        let _ = writeln!(
            output,
            "{:<tool_width$}  {:<profile_width$}  {:<when_width$}  {:<session_width$}  {resume}",
            session.tool, session.profile, when, session.id
        );
    }
    output
}

pub fn print_here(paths: &Paths) -> Result<()> {
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let sessions = recent_for_path(paths, &cwd, SESSION_LIMIT)?;
    print!("{}", render(&sessions, &cwd, Utc::now()));
    Ok(())
}

fn scan_claude_home(
    home: &Path,
    profile: &str,
    state: ProfileState,
    target_cwd: &Path,
    sessions: &mut Vec<Session>,
) -> Result<()> {
    let projects = home.join("projects");
    for project in read_dirs_if_exists(&projects)? {
        for path in read_jsonl_files(&project)? {
            if let Some(session) = parse_claude_session(&path, profile, state, target_cwd)? {
                sessions.push(session);
            }
        }
    }
    Ok(())
}

fn scan_codex_home(
    home: &Path,
    profile: &str,
    state: ProfileState,
    target_cwd: &Path,
    sessions: &mut Vec<Session>,
) -> Result<()> {
    for path in read_jsonl_files_recursive(&home.join("sessions"))? {
        if let Some(session) = parse_codex_session(&path, profile, state, target_cwd)? {
            sessions.push(session);
        }
    }
    Ok(())
}

fn parse_claude_session(
    path: &Path,
    profile: &str,
    state: ProfileState,
    target_cwd: &Path,
) -> Result<Option<Session>> {
    let mut id = None;
    let mut cwd = None;
    let mut updated_at = None;

    visit_json_lines(path, |record| {
        update_latest(&mut updated_at, record.get("timestamp"));
        if let Some(value) = record
            .get("sessionId")
            .or_else(|| record.get("session_id"))
            .and_then(Value::as_str)
        {
            id = Some(value.to_string());
        }
        if let Some(value) = record.get("cwd").and_then(Value::as_str) {
            cwd = Some(PathBuf::from(value));
        }
        cwd.as_deref()
            .is_none_or(|recorded| paths_match(recorded, target_cwd))
    })?;

    Ok(build_session(
        "claude", profile, state, id, cwd, updated_at, target_cwd,
    ))
}

fn parse_codex_session(
    path: &Path,
    profile: &str,
    state: ProfileState,
    target_cwd: &Path,
) -> Result<Option<Session>> {
    let mut id = None;
    let mut cwd = None;
    let mut updated_at = None;

    visit_json_lines(path, |record| {
        update_latest(&mut updated_at, record.get("timestamp"));
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            return true;
        }
        let Some(payload) = record.get("payload") else {
            return true;
        };
        if let Some(value) = payload.get("id").and_then(Value::as_str) {
            id = Some(value.to_string());
        }
        if let Some(value) = payload.get("cwd").and_then(Value::as_str) {
            cwd = Some(PathBuf::from(value));
        }
        update_latest(&mut updated_at, payload.get("timestamp"));
        cwd.as_deref()
            .is_none_or(|recorded| paths_match(recorded, target_cwd))
    })?;

    Ok(build_session(
        "codex", profile, state, id, cwd, updated_at, target_cwd,
    ))
}

fn build_session(
    tool: &str,
    profile: &str,
    state: ProfileState,
    id: Option<String>,
    cwd: Option<PathBuf>,
    updated_at: Option<DateTime<Utc>>,
    target_cwd: &Path,
) -> Option<Session> {
    let cwd = cwd?;
    if !paths_match(&cwd, target_cwd) {
        return None;
    }
    Some(Session {
        tool: tool.to_string(),
        profile: profile.to_string(),
        enabled: state.enabled,
        bypass: state.bypass,
        id: id?,
        cwd,
        updated_at: updated_at?,
    })
}

fn paths_match(recorded: &Path, current: &Path) -> bool {
    recorded == current
        || recorded
            .canonicalize()
            .ok()
            .zip(current.canonicalize().ok())
            .is_some_and(|(recorded, current)| recorded == current)
}

fn update_latest(latest: &mut Option<DateTime<Utc>>, value: Option<&Value>) {
    let Some(parsed) = value
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
    else {
        return;
    };
    if latest.is_none_or(|current| parsed > current) {
        *latest = Some(parsed);
    }
}

fn visit_json_lines(path: &Path, mut visit: impl FnMut(&Value) -> bool) -> Result<()> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(record) = serde_json::from_str(&line) else {
            continue;
        };
        if !visit(&record) {
            break;
        }
    }
    Ok(())
}

fn read_dirs_if_exists(path: &Path) -> Result<Vec<PathBuf>> {
    read_entries_if_exists(path, true)
}

fn read_jsonl_files(path: &Path) -> Result<Vec<PathBuf>> {
    read_entries_if_exists(path, false)
}

fn read_entries_if_exists(path: &Path, directories: bool) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", path.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("reading file type for {}", entry.path().display()))?;
        let matches = if directories {
            file_type.is_dir()
        } else {
            file_type.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        };
        if matches {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

fn read_jsonl_files_recursive(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", directory.display()));
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("reading file type for {}", entry.path().display()))?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file()
                && entry.path().extension().is_some_and(|ext| ext == "jsonl")
            {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}

fn relative_time(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let elapsed = now.signed_duration_since(then).max(Duration::zero());
    let seconds = elapsed.num_seconds();
    if seconds < 5 {
        "just now".to_string()
    } else if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 60 * 60 {
        format!("{}m ago", elapsed.num_minutes())
    } else if seconds < 24 * 60 * 60 {
        format!("{}h ago", elapsed.num_hours())
    } else {
        format!("{}d ago", elapsed.num_days())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn test_paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
        }
    }

    fn write_config(paths: &Paths) {
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.config_file(),
            r#"
[tools.claude]
command = ["claude"]
[tools.claude.profiles.work]
[tools.claude.profiles.personal]
[tools.claude.profiles.disabled]
enabled = false

[tools.codex]
command = ["codex"]
[tools.codex.profiles.work]
[tools.codex.profiles.personal]
[tools.codex.profiles.bypassed]
bypass = true
"#,
        )
        .unwrap();
    }

    fn write_claude_session(
        paths: &Paths,
        profile: &str,
        project: &str,
        id: &str,
        cwd: &Path,
        timestamps: &[&str],
    ) {
        let directory = paths
            .profile_home_dir("claude", profile)
            .join("projects")
            .join(project);
        std::fs::create_dir_all(&directory).unwrap();
        let records = timestamps
            .iter()
            .map(|timestamp| {
                serde_json::json!({
                    "type": "user",
                    "sessionId": id,
                    "cwd": cwd,
                    "timestamp": timestamp,
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let path = directory.join(format!("{id}.jsonl"));
        std::fs::write(&path, records).unwrap();
        set_modified(&path, timestamps.last().unwrap());
    }

    fn write_codex_session(
        paths: &Paths,
        profile: &str,
        id: &str,
        cwd: &Path,
        started_at: &str,
        updated_at: &str,
    ) -> PathBuf {
        let directory = paths
            .profile_home_dir("codex", profile)
            .join("sessions/2026/07/31");
        std::fs::create_dir_all(&directory).unwrap();
        let records = [
            serde_json::json!({
                "timestamp": started_at,
                "type": "session_meta",
                "payload": {"id": id, "cwd": cwd, "timestamp": started_at},
            })
            .to_string(),
            serde_json::json!({"timestamp": updated_at, "type": "event_msg"}).to_string(),
        ];
        let path = directory.join(format!("rollout-{id}.jsonl"));
        std::fs::write(&path, records.join("\n")).unwrap();
        set_modified(&path, updated_at);
        path
    }

    fn set_modified(path: &Path, timestamp: &str) {
        let modified: std::time::SystemTime = DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .with_timezone(&Utc)
            .into();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }

    fn utc(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 31, hour, minute, 0).unwrap()
    }

    #[test]
    fn filters_current_path_orders_newest_first_limits_five_and_reads_both_agents() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        write_config(&paths);
        let cwd = temp.path().join("project");
        let other = temp.path().join("other");

        write_claude_session(
            &paths,
            "work",
            "project",
            "claude-old",
            &cwd,
            &["2026-07-31T09:00:00Z", "2026-07-31T09:15:00Z"],
        );
        write_claude_session(
            &paths,
            "personal",
            "other",
            "wrong-path",
            &other,
            &["2026-07-31T12:00:00Z"],
        );
        for (index, minute) in [20, 30, 40, 50, 55].into_iter().enumerate() {
            write_codex_session(
                &paths,
                if index % 2 == 0 { "work" } else { "personal" },
                &format!("codex-{index}"),
                &cwd,
                "2026-07-31T09:00:00Z",
                &format!("2026-07-31T09:{minute:02}:00Z"),
            );
        }

        let sessions = recent_for_path(&paths, &cwd, 5).unwrap();
        assert_eq!(sessions.len(), 5);
        assert_eq!(sessions[0].id, "codex-4");
        assert_eq!(sessions[4].id, "codex-0");
        assert!(sessions
            .windows(2)
            .all(|pair| pair[0].updated_at >= pair[1].updated_at));
        assert!(sessions.iter().all(|session| session.cwd == cwd));
        assert!(sessions.iter().any(|session| session.profile == "work"));
        assert!(sessions.iter().any(|session| session.profile == "personal"));

        let all = recent_for_path(&paths, &cwd, 10).unwrap();
        assert!(all.iter().any(|session| session.tool == "claude"));
        assert!(all.iter().any(|session| session.tool == "codex"));
        assert!(!all.iter().any(|session| session.id == "wrong-path"));
    }

    #[test]
    fn renders_relative_time_and_profile_bound_session_resume_commands() {
        let cwd = Path::new("/work/project");
        let sessions = vec![
            Session {
                tool: "codex".into(),
                profile: "personal".into(),
                enabled: true,
                bypass: false,
                id: "codex-id".into(),
                cwd: cwd.into(),
                updated_at: utc(11, 58),
            },
            Session {
                tool: "claude".into(),
                profile: "work team".into(),
                enabled: true,
                bypass: false,
                id: "claude-id".into(),
                cwd: cwd.into(),
                updated_at: utc(10, 0),
            },
        ];

        let output = render(&sessions, cwd, utc(12, 0));
        assert!(output.contains("2m ago"), "{output}");
        assert!(output.contains("2h ago"), "{output}");
        assert!(
            output.contains("rtr codex -p personal resume codex-id"),
            "{output}"
        );
        assert!(
            output.contains("rtr claude -p 'work team' --resume claude-id"),
            "{output}"
        );
    }

    #[test]
    fn native_timestamps_win_over_file_mtime_for_ordering() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        write_config(&paths);
        let cwd = temp.path().join("project");
        let older = write_codex_session(
            &paths,
            "work",
            "older-record",
            &cwd,
            "2026-07-31T09:00:00Z",
            "2026-07-31T10:00:00Z",
        );
        let newer = write_codex_session(
            &paths,
            "personal",
            "newer-record",
            &cwd,
            "2026-07-31T09:00:00Z",
            "2026-07-31T11:00:00Z",
        );
        set_modified(&older, "2026-07-31T14:00:00Z");
        set_modified(&newer, "2026-07-31T08:00:00Z");

        let sessions = recent_for_path(&paths, &cwd, 5).unwrap();
        assert_eq!(sessions[0].id, "newer-record");
        assert_eq!(sessions[0].updated_at, utc(11, 0));
        assert_eq!(sessions[1].id, "older-record");
        assert_eq!(sessions[1].updated_at, utc(10, 0));
    }

    #[test]
    fn resume_commands_restore_disabled_and_bypassed_profile_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        write_config(&paths);
        let cwd = temp.path().join("project");
        write_claude_session(
            &paths,
            "disabled",
            "project",
            "claude-disabled",
            &cwd,
            &["2026-07-31T11:00:00Z"],
        );
        write_codex_session(
            &paths,
            "bypassed",
            "codex-bypassed",
            &cwd,
            "2026-07-31T10:00:00Z",
            "2026-07-31T10:30:00Z",
        );

        let sessions = recent_for_path(&paths, &cwd, 5).unwrap();
        let claude = sessions
            .iter()
            .find(|session| session.id == "claude-disabled")
            .unwrap();
        assert!(!claude.enabled);
        assert!(!claude.bypass);
        let codex = sessions
            .iter()
            .find(|session| session.id == "codex-bypassed")
            .unwrap();
        assert!(codex.enabled);
        assert!(codex.bypass);

        let output = render(&sessions, &cwd, utc(12, 0));
        assert!(
            output.contains(
                "rtr enable claude --profile disabled && rtr claude -p disabled --resume claude-disabled"
            ),
            "{output}"
        );
        assert!(
            output.contains(
                "rtr unbypass codex --profile bypassed && rtr codex -p bypassed resume codex-bypassed"
            ),
            "{output}"
        );
    }

    #[test]
    fn empty_and_malformed_native_history_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        write_config(&paths);
        let malformed = paths
            .profile_home_dir("claude", "work")
            .join("projects/project/broken.jsonl");
        std::fs::create_dir_all(malformed.parent().unwrap()).unwrap();
        std::fs::write(&malformed, "not-json\n{\"cwd\":\"/project\"}").unwrap();

        let sessions = recent_for_path(&paths, Path::new("/project"), 5).unwrap();
        assert!(sessions.is_empty());
        assert_eq!(
            render(&sessions, Path::new("/project"), utc(12, 0)),
            "No Claude or Codex sessions found for /project.\n"
        );
    }
}
