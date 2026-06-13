//! Helpers for turning captured auth-like headers into profile rewrites.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::capture::CaptureRecord;
use crate::config::Config;
use crate::paths::Paths;
use crate::rewrite::{is_secret_header, redact_value};

#[derive(Debug, Clone, Default)]
pub struct AuthHeaderFilter {
    pub host: Option<String>,
    pub header: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthHeaderSummary {
    pub host: String,
    pub header: String,
    pub count: usize,
    pub latest_ts: String,
    pub latest_value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub config_path: PathBuf,
    pub capture_path: PathBuf,
    pub tool: String,
    pub profile: String,
    pub host: String,
    pub header: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthHeaderCandidate {
    ts: String,
    host: String,
    method: String,
    url: String,
    header: String,
    value: String,
}

/// Resolve the capture file to inspect, defaulting to the newest run for a tool.
pub fn resolve_capture_path(
    paths: &Paths,
    tool: &str,
    capture_path: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = capture_path {
        return Ok(path);
    }

    let runs = paths.runs_dir().join(tool);
    let mut captures = Vec::new();
    for entry in std::fs::read_dir(&runs)
        .with_context(|| format!("reading runs for tool '{tool}' under {}", runs.display()))?
    {
        let path = entry?.path().join("capture.jsonl");
        if path.is_file() {
            captures.push(path);
        }
    }
    captures.sort();
    captures
        .pop()
        .with_context(|| format!("no capture.jsonl files found for tool '{tool}'"))
}

/// Read auth-like headers from one capture file and summarize by host/header.
pub fn auth_headers_for_capture(
    capture_path: impl Into<PathBuf>,
    filter: &AuthHeaderFilter,
) -> Result<Vec<AuthHeaderSummary>> {
    let candidates = read_candidates(&capture_path.into(), filter)?;
    Ok(summarize_candidates(&candidates))
}

/// Render auth header summaries for terminal output, redacting by default.
pub fn render_auth_headers(rows: &[AuthHeaderSummary], show_secrets: bool) -> String {
    use std::fmt::Write as _;

    if rows.is_empty() {
        return "No auth-like headers found.\n".to_string();
    }

    let mut out = String::new();
    let _ = writeln!(out, "host\theader\tcount\tlatest\tvalue");
    for row in rows {
        let value = if show_secrets {
            row.latest_value.clone()
        } else {
            redact_value(&row.header, &row.latest_value)
        };
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}",
            row.host, row.header, row.count, row.latest_ts, value
        );
    }
    out
}

/// Import one resolved captured auth-like header into a profile's rewrite set.
pub fn import_auth_header(
    paths: &Paths,
    tool: &str,
    profile: &str,
    capture_path: Option<PathBuf>,
    filter: &AuthHeaderFilter,
    create_profile: bool,
) -> Result<ImportSummary> {
    let capture_path = resolve_capture_path(paths, tool, capture_path)?;
    let candidates = read_candidates(&capture_path, filter)?;
    let mut latest = latest_candidates(&candidates);
    match latest.len() {
        0 => bail!("no auth-like headers matched in {}", capture_path.display()),
        1 => {}
        _ => {
            let matches = latest
                .iter()
                .map(|c| format!("{}/{}", c.host, c.header))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "multiple auth headers matched in {}: {matches}; use --host and/or --header",
                capture_path.display()
            );
        }
    }
    let selected = latest.remove(0);

    let config_path = paths.config_file();
    let mut cfg = Config::load(&config_path)?;
    let tool_cfg = cfg
        .tools
        .get_mut(tool)
        .with_context(|| format!("no tool named '{tool}' in config.toml"))?;
    let profile_cfg = if create_profile {
        tool_cfg.profiles.entry(profile.to_string()).or_default()
    } else {
        tool_cfg
            .profiles
            .get_mut(profile)
            .with_context(|| format!("tool '{tool}' has no profile '{profile}'"))?
    };
    profile_cfg
        .set
        .insert(selected.header.clone(), selected.value.clone());
    let text = cfg.to_toml()?;
    crate::config::write_secret_file(&config_path, &text)?;

    Ok(ImportSummary {
        config_path,
        capture_path,
        tool: tool.to_string(),
        profile: profile.to_string(),
        host: selected.host,
        header: selected.header,
    })
}

fn read_candidates(path: &Path, filter: &AuthHeaderFilter) -> Result<Vec<AuthHeaderCandidate>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening capture file {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();

    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading {} line {}", path.display(), idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: CaptureRecord = serde_json::from_str(&line)
            .with_context(|| format!("parsing {} line {}", path.display(), idx + 1))?;
        for (header, value) in rec.headers {
            if !is_secret_header(&header) {
                continue;
            }
            let candidate = AuthHeaderCandidate {
                ts: rec.ts.clone(),
                host: rec.host.clone(),
                method: rec.method.clone(),
                url: rec.url.clone(),
                header,
                value,
            };
            if filter_matches(&candidate, filter) {
                out.push(candidate);
            }
        }
    }
    Ok(out)
}

fn filter_matches(candidate: &AuthHeaderCandidate, filter: &AuthHeaderFilter) -> bool {
    if let Some(host) = &filter.host {
        if !candidate.host.eq_ignore_ascii_case(host) {
            return false;
        }
    }
    if let Some(header) = &filter.header {
        if !candidate.header.eq_ignore_ascii_case(header) {
            return false;
        }
    }
    true
}

fn summarize_candidates(candidates: &[AuthHeaderCandidate]) -> Vec<AuthHeaderSummary> {
    let mut groups: BTreeMap<(String, String), AuthHeaderSummary> = BTreeMap::new();
    for candidate in candidates {
        let key = (
            candidate.host.to_ascii_lowercase(),
            candidate.header.to_ascii_lowercase(),
        );
        groups
            .entry(key)
            .and_modify(|row| {
                row.count += 1;
                row.latest_ts = candidate.ts.clone();
                row.latest_value = candidate.value.clone();
                row.host = candidate.host.clone();
                row.header = candidate.header.clone();
            })
            .or_insert_with(|| AuthHeaderSummary {
                host: candidate.host.clone(),
                header: candidate.header.clone(),
                count: 1,
                latest_ts: candidate.ts.clone(),
                latest_value: candidate.value.clone(),
            });
    }
    groups.into_values().collect()
}

fn latest_candidates(candidates: &[AuthHeaderCandidate]) -> Vec<AuthHeaderCandidate> {
    let mut groups: BTreeMap<(String, String), AuthHeaderCandidate> = BTreeMap::new();
    for candidate in candidates {
        groups.insert(
            (
                candidate.host.to_ascii_lowercase(),
                candidate.header.to_ascii_lowercase(),
            ),
            candidate.clone(),
        );
    }
    groups.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CaptureRecord;
    use crate::paths::Paths;

    fn write_capture(records: &[CaptureRecord]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.jsonl");
        let mut text = String::new();
        for rec in records {
            text.push_str(&serde_json::to_string(rec).unwrap());
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn rec(ts: &str, host: &str, header: &str, value: &str) -> CaptureRecord {
        CaptureRecord {
            ts: ts.to_string(),
            method: "POST".to_string(),
            url: format!("https://{host}/v1"),
            host: host.to_string(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                (header.to_string(), value.to_string()),
            ],
        }
    }

    #[test]
    fn lists_auth_like_headers_grouped_by_host_and_header() {
        let (_dir, path) = write_capture(&[
            rec(
                "2026-06-11T21:00:00Z",
                "chatgpt.com",
                "authorization",
                "Bearer OLD",
            ),
            rec(
                "2026-06-11T21:01:00Z",
                "chatgpt.com",
                "authorization",
                "Bearer NEW",
            ),
            rec("2026-06-11T21:02:00Z", "chatgpt.com", "x-plain", "visible"),
        ]);

        let rows = auth_headers_for_capture(&path, &AuthHeaderFilter::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].host, "chatgpt.com");
        assert_eq!(rows[0].header, "authorization");
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].latest_ts, "2026-06-11T21:01:00Z");
        assert_eq!(rows[0].latest_value, "Bearer NEW");

        let out = render_auth_headers(&rows, false);
        assert!(out.contains("Bearer «redacted»"), "{out}");
        assert!(!out.contains("Bearer NEW"), "{out}");
    }

    #[test]
    fn import_requires_filters_when_multiple_auth_headers_match() {
        let (_cap_dir, capture_path) = write_capture(&[
            rec(
                "2026-06-11T21:00:00Z",
                "chatgpt.com",
                "authorization",
                "Bearer CODEX",
            ),
            rec(
                "2026-06-11T21:01:00Z",
                "api.anthropic.com",
                "authorization",
                "Bearer CLAUDE",
            ),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: tmp.path().join("config"),
            state_dir: tmp.path().join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.config_file(),
            r#"
[tools.codex]
command = ["codex"]
[tools.codex.profiles.codex-1]
set = {}
"#,
        )
        .unwrap();

        let err = import_auth_header(
            &paths,
            "codex",
            "codex-1",
            Some(capture_path),
            &AuthHeaderFilter::default(),
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("multiple auth headers"), "got: {err}");
        assert!(err.contains("--host"), "got: {err}");
    }

    #[test]
    fn import_writes_selected_header_to_profile() {
        let (_cap_dir, capture_path) = write_capture(&[
            rec(
                "2026-06-11T21:00:00Z",
                "chatgpt.com",
                "authorization",
                "Bearer OLD",
            ),
            rec(
                "2026-06-11T21:01:00Z",
                "chatgpt.com",
                "authorization",
                "Bearer NEW",
            ),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: tmp.path().join("config"),
            state_dir: tmp.path().join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.config_file(),
            r#"
[tools.codex]
command = ["codex"]
[tools.codex.profiles.codex-1]
set = {}
"#,
        )
        .unwrap();

        let summary = import_auth_header(
            &paths,
            "codex",
            "codex-1",
            Some(capture_path.clone()),
            &AuthHeaderFilter {
                host: Some("chatgpt.com".to_string()),
                header: Some("authorization".to_string()),
            },
            false,
        )
        .unwrap();

        assert_eq!(summary.capture_path, capture_path);
        assert_eq!(summary.host, "chatgpt.com");
        assert_eq!(summary.header, "authorization");
        let cfg = crate::config::Config::load(&paths.config_file()).unwrap();
        let got = cfg
            .tool("codex")
            .unwrap()
            .profiles
            .get("codex-1")
            .unwrap()
            .set
            .get("authorization")
            .map(String::as_str);
        assert_eq!(got, Some("Bearer NEW"));
    }

    #[test]
    fn import_errors_on_missing_profile() {
        let (_cap_dir, capture_path) = write_capture(&[rec(
            "2026-06-11T21:00:00Z",
            "chatgpt.com",
            "authorization",
            "Bearer TOKEN",
        )]);
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: tmp.path().join("config"),
            state_dir: tmp.path().join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.config_file(),
            r#"
[tools.codex]
command = ["codex"]
"#,
        )
        .unwrap();

        let err = import_auth_header(
            &paths,
            "codex",
            "missing",
            Some(capture_path),
            &AuthHeaderFilter::default(),
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("profile 'missing'"), "got: {err}");
    }

    #[test]
    fn import_can_create_profile_explicitly() {
        let (_cap_dir, capture_path) = write_capture(&[rec(
            "2026-06-11T21:00:00Z",
            "api.anthropic.com",
            "authorization",
            "Bearer TOKEN",
        )]);
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: tmp.path().join("config"),
            state_dir: tmp.path().join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.config_file(),
            r#"
[tools.claude]
command = ["claude"]
"#,
        )
        .unwrap();

        import_auth_header(
            &paths,
            "claude",
            "claude-1",
            Some(capture_path),
            &AuthHeaderFilter {
                host: Some("api.anthropic.com".to_string()),
                header: Some("authorization".to_string()),
            },
            true,
        )
        .unwrap();

        let cfg = crate::config::Config::load(&paths.config_file()).unwrap();
        let got = cfg
            .tool("claude")
            .unwrap()
            .profiles
            .get("claude-1")
            .unwrap()
            .set
            .get("authorization")
            .map(String::as_str);
        assert_eq!(got, Some("Bearer TOKEN"));
    }
}
