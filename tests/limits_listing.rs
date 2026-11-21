//! Exercise the listing through the CLI against disposable config and native
//! homes, using a real terminal width as well as redirected stdout.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    os::fd::FromRawFd,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use chrono::{DateTime, Duration, Utc};
use ratatui::text::Span;
use serde_json::{json, Value};

const CONFIG: &str = r#"
[tools.claude]
command = ["false"]
copy = []
[tools.claude.profiles.active]
share_percent = 70
[tools.claude.profiles.off]
enabled = false
bypass = true
[tools.claude.profiles.skip]
share_percent = 30
bypass = true

[tools.codex]
command = ["false"]
copy = []
[tools.codex.profiles.dual]
[tools.codex.profiles.empty]
"#;

struct Fixture {
    root: tempfile::TempDir,
    reset: DateTime<Utc>,
    expired: DateTime<Utc>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let observed = now - Duration::minutes(6);
        let reset = now + Duration::hours(2);
        let expired = now - Duration::hours(2);
        let fixture = Self {
            root,
            reset,
            expired,
        };
        fixture.put("config/config.toml", CONFIG);
        fixture.claude(
            "active",
            observed,
            json!({
                "five_hour": {"utilization": 0, "resets_at": null},
                "seven_day": {"utilization": 99.9, "resets_at": reset},
                "limits": [{
                    "kind": "weekly_scoped", "scope": {"model": {"display_name": "Fable"}},
                    "percent": 100, "resets_at": reset
                }]
            }),
        );
        fixture.claude(
            "off",
            now - Duration::hours(4),
            json!({
                "seven_day": {"utilization": 64, "resets_at": expired}
            }),
        );
        fixture.claude(
            "skip",
            observed,
            json!({
                "seven_day": {"utilization": 73, "resets_at": reset}
            }),
        );

        let codex_observations = [
            (
                observed,
                json!({
                    "limit_id": "codex",
                    "primary": {"used_percent": 45, "window_minutes": 10080, "resets_at": reset.timestamp()},
                    "secondary": {"used_percent": 5, "window_minutes": 300, "resets_at": reset.timestamp()},
                    "credits": {"has_credits": true, "balance": "42.25"}
                }),
            ),
            (
                now - Duration::minutes(30),
                json!({
                    "limit_id": "extra",
                    "primary": {"used_percent": 31, "window_minutes": 61, "resets_at": null}
                }),
            ),
        ];
        let mut rows = vec![json!({"type":"session_meta", "payload":{"id":"fixture"}})];
        rows.extend(codex_observations.map(|(at, limits)| {
            json!({
                "timestamp": at, "type": "event_msg",
                "payload": {"type": "token_count", "rate_limits": limits}
            })
        }));
        fixture.put(
            "state/homes/codex/dual/sessions/rollout-fixture.jsonl",
            &json_lines(&rows),
        );
        fixture.put(
            "state/quota.json",
            &json!({"entries": {
                "claude": {
                    "active": {"native_kind": "seven_day", "limit": {
                        "kind": "weekly", "at": observed, "resets_at": reset,
                        "active": true, "message": "fixture"
                    }},
                    "off": {"limit": {
                        "kind": "usage", "at": observed, "resets_at": null,
                        "active": true, "message": "fixture"
                    }}
                }
            }})
            .to_string(),
        );
        fixture.put("state/usage.jsonl", &json_lines(&[
            json!({"ts": now, "tool": "claude", "profile": "active", "exit_code": 0}),
            json!({"ts": "2000-01-01T12:00:00Z", "tool": "claude", "profile": "active", "exit_code": 1}),
            json!({"ts": "2000-01-01T12:00:00Z", "tool": "claude", "profile": "old", "exit_code": 1}),
        ]));
        fixture.put("state/state.toml", "");
        fixture.put("state/routing.jsonl", "");
        fixture
    }

    fn put(&self, name: &str, data: &str) {
        let path = self.root.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn claude(&self, profile: &str, observed: DateTime<Utc>, utilization: Value) {
        self.put(&format!("state/homes/claude/{profile}/.claude.json"), &json!({
            "cachedUsageUtilization": {"fetchedAtMs": observed.timestamp_millis(), "utilization": utilization}
        }).to_string());
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rtr"));
        command
            .args(["ls"])
            .args(args)
            .env("HOME", self.root.path())
            .env("RTR_CONFIG_DIR", self.root.path().join("config"))
            .env("RTR_STATE_DIR", self.root.path().join("state"))
            .env("TZ", "UTC")
            .env("TERM", "xterm")
            .env("NO_COLOR", "1");
        command
    }
}

fn json_lines(rows: &[Value]) -> String {
    rows.iter().map(|value| format!("{value}\n")).collect()
}

fn terminal(mut command: Command, width: u16) -> String {
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: width,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty initializes both descriptors. No name buffer or termios
    // override is needed. The Files each take ownership of one descriptor.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        },
        0
    );
    let mut master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    command.env("COLUMNS", "200");
    let child = command
        .stdin(slave.try_clone().unwrap())
        .stdout(slave)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(command);
    let mut bytes = Vec::new();
    if let Err(error) = master.read_to_end(&mut bytes) {
        // On Linux, closing the last slave descriptor ends the stream with EIO.
        assert_eq!(error.raw_os_error(), Some(libc::EIO), "{error}");
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(bytes).unwrap().replace("\r\n", "\n")
}

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path));
        } else {
            result.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    result
}

fn plain_layout(text: &str, width: usize) {
    assert!(!text.contains('\x1b'), "{text:?}");
    for line in text.lines() {
        assert!(Span::raw(line).width() <= width, "{width}: {line}");
    }
}

fn readings(text: &str, fixture: &Fixture) {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for metric in [
        " 0% reset ?",
        " 99.9%",
        "FULL",
        "was 64%",
        " 5%",
        " 45%",
        " 31%",
    ] {
        // A narrow meter prints its unknown reset on the continuation.
        assert_eq!(flat.matches(metric).count(), 1, "{metric}: {text}");
    }
    assert!(!text.contains("73%"), "{text}");
    assert!(text.contains("native default / unknown"), "{text}");
    assert!(text.contains("no window reported"), "{text}");
    assert!(text.contains("disabled; limited"), "{text}");
    assert!(text.contains("99.9% !"), "{text}");
    assert!(text.contains("HELD Week, all"), "{text}");
    assert!(text.contains("HELD profile · retry after"), "{text}");
    assert!(flat.contains("cache stale"), "{text}");
    assert!(flat.contains("rollout stale"), "{text}");
    assert_eq!(flat.matches(" rollout").count(), 3, "{text}");
    assert_eq!(flat.matches(" cache").count(), 2, "{text}");
    assert_eq!(flat.matches(" transcript").count(), 2, "{text}");
    assert!(flat.contains("meters from isolated home"), "{text}");
    assert!(text.contains("61m, extra"), "{text}");
    assert!(text.contains("credits 42.25"), "{text}");
    assert!(
        text.contains(&fixture.reset.format("%b %-d %H:%M").to_string()),
        "{text}"
    );
    assert!(
        text.contains(&fixture.expired.format("%b %-d %H:%M").to_string()),
        "{text}"
    );
    assert_eq!(text.matches("█████ FULL").count(), 1, "{text}");
    for line in text.lines().filter(|line| line.contains("99.9%")) {
        assert!(line.contains("█░ 99.9%"), "{line}");
    }
}

#[test]
fn listing_preserves_windows_policy_and_history_at_terminal_and_pipe_widths() {
    let fixture = Fixture::new();
    let before = files(fixture.root.path());
    for width in [160, 120, 80, 44] {
        let today = terminal(fixture.command(&["--color", "never"]), width);
        let all = terminal(fixture.command(&["-a"]), width);
        for text in [&today, &all] {
            plain_layout(text, usize::from(width));
            readings(text, &fixture);
            if width == 160 {
                assert!(
                    text.lines()
                        .any(|line| line.starts_with("PROFILE") && line.contains("Week, Fable")),
                    "{text}"
                );
            }
            if width <= 80 {
                assert!(text.contains("claude/active  enabled; limited"), "{text}");
                assert!(text.contains("Week, Fable  █████ FULL"), "{text}");
                assert!(text.contains("5h  ?"), "{text}");
            }
        }
        assert!(!today.contains("Removed profiles"), "{today}");
        assert!(all.contains("Removed profiles · recorded usage"), "{all}");
        if width <= 80 {
            assert!(today.contains("runs 1 · failed 0"), "{today}");
            assert!(all.contains("runs 2 · failed 1"), "{all}");
            assert!(all.contains("claude/old  removed"), "{all}");
        }
    }
    let output = fixture
        .command(&["--color", "never"])
        .env("COLUMNS", "44")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let piped = String::from_utf8(output.stdout).unwrap();
    plain_layout(&piped, 44);
    assert!(piped.contains("claude/active  enabled; limited"), "{piped}");
    readings(&piped, &fixture);
    let output = fixture
        .command(&["--color", "never"])
        .env_remove("COLUMNS")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let unmeasured = String::from_utf8(output.stdout).unwrap();
    let active: Vec<_> = unmeasured
        .lines()
        .filter(|line| line.contains("active"))
        .collect();
    assert_eq!(active.len(), 1, "{unmeasured}");
    assert!(
        active[0].contains("99.9% !") && active[0].contains("HELD Week, all"),
        "{unmeasured}"
    );
    assert!(
        unmeasured
            .lines()
            .any(|line| { line.starts_with("PROFILE") && line.contains("61m, extra") }),
        "{unmeasured}"
    );
    readings(&unmeasured, &fixture);
    assert_eq!(before, files(fixture.root.path()));
}

#[test]
fn expired_values_are_dimmed_with_color_and_distinct_in_plain_output() {
    let fixture = Fixture::new();
    let colored = terminal(fixture.command(&["--color", "always"]), 44);
    assert!(
        colored.contains("\x1b[2mWeek, all  ███░░ was 64%\x1b[0m"),
        "{colored:?}"
    );
    let plain = terminal(fixture.command(&[]), 44);
    assert!(plain.contains("Week, all  ███░░ was 64%"), "{plain}");
    assert!(!plain.contains('\x1b'), "{plain:?}");
}

#[test]
fn very_narrow_blocks_preserve_distinct_profile_identities() {
    let fixture = Fixture::new();
    let config = format!(
        "{CONFIG}\n[tools.codex.profiles.\"work team\"]\n[tools.codex.profiles.\"work  team\"]\n"
    );
    fixture.put("config/config.toml", &config);
    let output = terminal(fixture.command(&["--color", "never"]), 20);
    plain_layout(&output, 20);
    for identity in ["codex/work team", "codex/work  team"] {
        let suffix = output.split_once(identity).unwrap().1;
        assert!(
            suffix.starts_with("\n  state enabled\n  home isolated\n  share "),
            "{suffix}"
        );
    }
}
