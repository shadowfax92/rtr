//! Backwards-compatible `rtr here` view over the conversation catalog.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};

use crate::conversations::{self, ConversationQuery};
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

/// Find the five newest native sessions whose recorded cwd is exactly `cwd`.
pub fn recent_for_path(paths: &Paths, cwd: &Path, limit: usize) -> Result<Vec<Session>> {
    let catalog = conversations::query(
        paths,
        &ConversationQuery::all().with_cwd(cwd).with_limit(limit),
    )?;
    Ok(catalog
        .conversations
        .into_iter()
        .map(|conversation| Session {
            tool: conversation.tool,
            profile: conversation.profile,
            enabled: conversation.enabled,
            bypass: conversation.bypass,
            id: conversation.id,
            cwd: conversation.cwd,
            updated_at: conversation.updated_at,
        })
        .collect())
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
        // `rtr resume` deliberately opens the session's isolated profile even
        // when that profile is disabled or normally bypassed.
        let resume = format!(
            "rtr resume {} --tool {} --profile {}",
            crate::runner::shell_quote(&session.id),
            session.tool,
            profile
        );
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
    use std::fs::File;

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
            output.contains("rtr resume codex-id --tool codex --profile personal"),
            "{output}"
        );
        assert!(
            output.contains("rtr resume claude-id --tool claude --profile 'work team'"),
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
            output.contains("rtr resume claude-disabled --tool claude --profile disabled"),
            "{output}"
        );
        assert!(
            output.contains("rtr resume codex-bypassed --tool codex --profile bypassed"),
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
