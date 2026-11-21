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
[tools.claude.profiles.quiet]
enabled = false
[tools.claude.profiles.ready]
enabled = false
[tools.claude.profiles.spare]
enabled = false

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
    far_reset: DateTime<Utc>,
    success: DateTime<Utc>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let observed = now - Duration::minutes(6);
        let reset = now + Duration::hours(2);
        let expired = now - Duration::hours(2);
        let far_reset = now + Duration::days(7);
        let success = now - Duration::minutes(2);
        let fixture = Self {
            root,
            reset,
            expired,
            far_reset,
            success,
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
                    "primary": {"used_percent": 45, "window_minutes": 10080, "resets_at": far_reset.timestamp()},
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
            (
                now - Duration::days(8),
                json!({
                    "limit_id": "expired-extra",
                    "primary": {"used_percent": 8, "window_minutes": 10080, "resets_at": (now - Duration::days(7) - Duration::minutes(5)).timestamp()},
                    "secondary": {"used_percent": 9, "window_minutes": 300, "resets_at": (now - Duration::days(7) - Duration::hours(12)).timestamp()}
                }),
            ),
            (
                now - Duration::days(8),
                json!({
                    "limit_id": "recent-extra",
                    "primary": {"used_percent": 21, "window_minutes": 10080, "resets_at": (now - Duration::days(7) + Duration::minutes(5)).timestamp()}
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
                    }},
                    "ready": {"native_kind": "seven_day_overage_included", "limit": {
                        "kind": "weekly", "at": observed, "resets_at": reset,
                        "active": true, "message": "fixture"
                    }},
                    "spare": {"cleared_at": success, "limit": {
                        "kind": "usage", "at": observed, "resets_at": reset,
                        "active": true, "message": "fixture"
                    }}
                }
            }})
            .to_string(),
        );
        let owned = "state/homes/claude/ready/projects/test/fixture-success.jsonl";
        fixture.put(
            owned,
            &json_lines(&[json!({
                "type": "assistant", "sessionId": "fixture-success", "timestamp": success,
                "message": {"model": "claude-opus-5-5", "content": [{"type": "text", "text": "fixture"}]}
            })]),
        );
        fixture.put(
            "state/fork-origins.json",
            &json!({"claude":{"ready":{"fixture-success":{
                "path": fixture.root.path().join(owned), "bytes": 0, "at": success
            }}}})
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

fn bars(row: &str) -> Vec<usize> {
    row.split_whitespace()
        .filter(|field| field.chars().all(|cell| matches!(cell, '█' | '░')))
        .map(|bar| bar.chars().count())
        .collect()
}

fn readings(text: &str, fixture: &Fixture, compact: bool) {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for metric in [
        " 0% reset ?",
        " 99.9%",
        "FULL",
        "was 64%",
        " 5%",
        " 45%",
        " 31%",
        "was 21%",
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
    assert_eq!(
        flat.matches(" rollout").count(),
        if compact { 3 } else { 4 },
        "{text}"
    );
    assert_eq!(flat.matches(" cache").count(), 2, "{text}");
    assert_eq!(flat.matches(" transcript").count(), 2, "{text}");
    assert!(flat.contains("meters from isolated home"), "{text}");
    assert!(text.contains("61m, extra"), "{text}");
    assert!(text.contains("credits 42.25"), "{text}");
    assert_eq!(
        flat.matches("2 expired extra readings hidden").count(),
        1,
        "{text}"
    );
    assert!(!text.contains("expired-extra"), "{text}");
    assert!(text.contains("recent-extra"), "{text}");
    let superseded = format!(
        "profile hold superseded by success {} · prior reset {}",
        fixture.success.format("%b %-d %H:%M"),
        fixture.reset.format("%a %H:%M")
    );
    assert_eq!(flat.matches(&superseded).count(), 1, "{text}");
    assert_eq!(flat.matches("HELD profile").count(), 1, "{text}");
    assert!(
        text.contains(&fixture.reset.format("%a %H:%M").to_string()),
        "{text}"
    );
    assert!(
        text.contains(&fixture.expired.format("%b %-d %H:%M").to_string()),
        "{text}"
    );
    assert!(
        text.contains(&fixture.far_reset.format("%b %-d %H:%M").to_string()),
        "{text}"
    );
    assert_eq!(text.matches("█ FULL").count(), 1, "{text}");
    for line in text.lines().filter(|line| line.contains("99.9%")) {
        assert!(line.contains("█░ 99.9%"), "{line}");
    }
}

#[test]
fn listing_preserves_windows_policy_and_history_at_terminal_and_pipe_widths() {
    let fixture = Fixture::new();
    let before = files(fixture.root.path());
    for width in [160, 120, 100, 99, 80, 44] {
        let today = terminal(fixture.command(&["ls", "--color", "never"]), width);
        let all = terminal(fixture.command(&["ls", "-a"]), width);
        for text in [&today, &all] {
            plain_layout(text, usize::from(width));
            readings(text, &fixture, width >= 100);
            if width >= 100 {
                let headers: Vec<_> = text
                    .lines()
                    .filter(|line| line.starts_with("PROFILE"))
                    .collect();
                assert_eq!(
                    headers.len(),
                    if text.contains("Removed profiles") {
                        3
                    } else {
                        2
                    },
                    "{text}"
                );
                assert!(
                    headers[0].contains("HOME") && headers[0].contains("Week, Fable"),
                    "{text}"
                );
                assert!(
                    !headers[1].contains("HOME") && !headers[1].contains("extra"),
                    "{text}"
                );
                assert!(!headers[0].contains("FAILED"), "{text}");
                for profile in [
                    "active", "off", "quiet", "ready", "skip", "spare", "dual", "empty",
                ] {
                    assert_eq!(
                        text.lines()
                            .filter(|line| line.split_whitespace().next() == Some(profile)
                                && !line.starts_with(' '))
                            .count(),
                        1,
                        "{profile}: {text}",
                    );
                }
                assert!(
                    text.lines()
                        .any(|line| line.starts_with("  dual ·") && line.contains("61m, extra")),
                    "{text}"
                );
                let active = text
                    .lines()
                    .find(|line| line.starts_with("active "))
                    .unwrap();
                let expected = match width {
                    160 => 10,
                    120 => 8,
                    _ => 9,
                };
                assert_eq!(bars(active), [expected; 3], "{active}");
                let dual = text.lines().find(|line| line.starts_with("dual ")).unwrap();
                assert_eq!(bars(dual), [10; 2], "{dual}");
            } else {
                assert!(text.contains("claude/active  enabled; limited"), "{text}");
                assert!(text.contains("Week, Fable  █████ FULL"), "{text}");
                assert!(text.contains("5h  ?"), "{text}");
            }
        }
        assert!(!today.contains("Removed profiles"), "{today}");
        assert!(all.contains("Removed profiles · recorded usage"), "{all}");
        if width < 100 {
            assert!(today.contains("runs 1 · failed 0"), "{today}");
            assert!(all.contains("runs 2 · failed 1"), "{all}");
            assert!(all.contains("claude/old  removed"), "{all}");
        } else {
            let active_runs = |text: &str, counts: &str| {
                let row = text
                    .lines()
                    .find(|line| line.starts_with("active "))
                    .unwrap();
                assert!(row.split_whitespace().any(|field| field == counts), "{row}");
            };
            active_runs(&today, "1/0");
            active_runs(&all, "1/1");
        }
    }
    let output = fixture
        .command(&["ls", "--color", "never"])
        .env("COLUMNS", "44")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let piped = String::from_utf8(output.stdout).unwrap();
    plain_layout(&piped, 44);
    assert!(piped.contains("claude/active  enabled; limited"), "{piped}");
    readings(&piped, &fixture, false);
    let output = fixture
        .command(&["ls", "--color", "never"])
        .env_remove("COLUMNS")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let unmeasured = String::from_utf8(output.stdout).unwrap();
    for profile in [
        "active", "off", "quiet", "ready", "skip", "spare", "dual", "empty",
    ] {
        assert_eq!(
            unmeasured
                .lines()
                .filter(|line| line.split_whitespace().next() == Some(profile))
                .count(),
            1,
            "{unmeasured}"
        );
    }
    let active = unmeasured
        .lines()
        .find(|line| line.starts_with("active "))
        .unwrap();
    assert!(
        active.contains("99.9% !") && active.contains("HELD Week, all"),
        "{unmeasured}"
    );
    assert_eq!(bars(active), [10; 3], "{active}");
    assert!(
        unmeasured
            .lines()
            .any(|line| { line.starts_with("PROFILE") && line.contains("61m, extra") }),
        "{unmeasured}"
    );
    readings(&unmeasured, &fixture, false);
    assert_eq!(before, files(fixture.root.path()));
}

#[test]
fn terminal_can_fit_a_comparison_with_three_cell_bars() {
    let fixture = Fixture::new();
    fixture.put(
        "config/config.toml",
        &CONFIG.replace("profiles.quiet", "profiles.quiet-profile-with-long"),
    );
    let text = terminal(fixture.command(&["ls", "--color", "never"]), 100);
    plain_layout(&text, 100);
    let active = text
        .lines()
        .find(|line| line.starts_with("active "))
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(bars(active), [3; 3], "{active}");
}

#[test]
fn learned_recovery_keeps_the_superseded_hold_visible() {
    let fixture = Fixture::new();
    let learned = fixture
        .command(&[
            "session",
            "fixture-success",
            "-t",
            "claude",
            "-p",
            "ready",
            "-j",
        ])
        .output()
        .unwrap();
    assert!(learned.status.success(), "{learned:?}");
    let quota: Value =
        serde_json::from_slice(&fs::read(fixture.root.path().join("state/quota.json")).unwrap())
            .unwrap();
    let ready = &quota["entries"]["claude"]["ready"];
    assert!(ready["limit"].is_null(), "{ready}");
    assert_eq!(ready["scoped"]["holds"], json!([]));
    // Remove the recent success from discovery to prove the listing can
    // explain the recovery from saved quota state alone.
    fs::rename(
        fixture
            .root
            .path()
            .join("state/homes/claude/ready/projects/test/fixture-success.jsonl"),
        fixture.root.path().join("fixture-success.jsonl"),
    )
    .unwrap();
    let before = files(fixture.root.path());
    let text = terminal(fixture.command(&["ls", "--color", "never"]), 120);
    assert!(
        text.contains(&format!(
            "ready · profile hold superseded by success {}",
            fixture.success.format("%b %-d %H:%M")
        )),
        "{text}"
    );
    assert_eq!(before, files(fixture.root.path()));
}

#[test]
fn width_limited_pipe_counts_hidden_readings_from_a_bypassed_home() {
    let fixture = Fixture::new();
    fixture.put(
        "config/config.toml",
        &CONFIG.replace(
            "[tools.codex.profiles.dual]",
            "[tools.codex.profiles.dual]\nenabled = false\nbypass = true",
        ),
    );
    let now = Utc::now();
    fixture.put(
        "state/homes/codex/dual/sessions/rollout-fixture.jsonl",
        &json_lines(&[
            json!({"type":"session_meta", "payload":{"id":"fixture"}}),
            json!({
                "timestamp": now - Duration::days(9),
                "type": "event_msg",
                "payload": {"type": "token_count", "rate_limits": {
                    "limit_id": "expired-only",
                    "primary": {
                        "used_percent": 40, "window_minutes": 10080,
                        "resets_at": (now - Duration::days(8)).timestamp()
                    }
                }}
            }),
        ]),
    );
    let output = fixture
        .command(&["ls", "--color", "never"])
        .env("COLUMNS", "160")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    plain_layout(&text, 160);
    assert_eq!(
        text.matches("1 expired extra reading hidden").count(),
        1,
        "{text}"
    );
    assert!(!text.contains("expired-only"), "{text}");
}

#[test]
fn expired_values_are_dimmed_with_color_and_distinct_in_plain_output() {
    let fixture = Fixture::new();
    let colored = terminal(fixture.command(&["ls", "--color", "always"]), 44);
    assert!(
        colored.contains("\x1b[2mWeek, all  ███░░ was 64%\x1b[0m"),
        "{colored:?}"
    );
    let plain = terminal(fixture.command(&["ls"]), 44);
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
    let output = terminal(fixture.command(&["ls", "--color", "never"]), 20);
    plain_layout(&output, 20);
    for identity in ["codex/work team", "codex/work  team"] {
        let suffix = output.split_once(identity).unwrap().1;
        assert!(
            suffix.starts_with("\n  state enabled\n  home isolated\n  share "),
            "{suffix}"
        );
    }
}

#[test]
fn owned_fable_success_supersedes_the_same_full_sample_in_listing_and_preview() {
    let fixture = Fixture::new();
    fixture.put(
        "config/config.toml",
        r#"
[tools.claude]
command = ["false"]
copy = []
[tools.claude.profiles.original]
[tools.claude.profiles.recovered]
"#,
    );
    let observed = Utc::now() - Duration::hours(3);
    let success = observed + Duration::hours(1);
    for profile in ["original", "recovered"] {
        fixture.claude(
            profile,
            observed,
            json!({
                "limits": [{
                    "kind": "weekly_scoped",
                    "scope": {"model": {"display_name": "Fable"}},
                    "percent": 100, "resets_at": fixture.reset
                }]
            }),
        );
    }
    let mut origins = json!({});
    for (profile, session, at) in [
        ("original", "fixture-source", observed - Duration::hours(1)),
        ("recovered", "fixture-success", success),
    ] {
        let name = format!("state/homes/claude/{profile}/projects/test/{session}.jsonl");
        let head = json_lines(&[json!({
            "type": "user", "sessionId": session, "uuid": "u", "cwd": fixture.root.path(),
            "timestamp": at, "message": {"role": "user", "content": "fixture"}
        })]);
        let tail = json_lines(&[json!({
            "type": "assistant", "sessionId": session, "uuid": "a", "parentUuid": "u",
            "timestamp": at, "message": {
                "model": "claude-fable-5", "content": [{"type": "text", "text": "fixture"}]
            }
        })]);
        fixture.put(&name, &format!("{head}{tail}"));
        // Only the response lies beyond the copy boundary. Its timestamp
        // predates fixture creation, as an owned response in a saved fork can.
        origins["claude"][profile][session] = json!({
            "path": fixture.root.path().join(name), "bytes": head.len(), "at": at
        });
    }
    fixture.put("state/fork-origins.json", &origins.to_string());
    let before = files(fixture.root.path());

    let output = terminal(fixture.command(&["ls", "--color", "never"]), 120);
    let original = output
        .lines()
        .find(|line| line.starts_with("original "))
        .unwrap();
    let recovered = output
        .lines()
        .find(|line| line.starts_with("recovered "))
        .unwrap();
    assert!(original.contains("FULL"), "{original}");
    assert!(
        recovered.contains(&format!("superseded {}", success.format("%a %H:%M"))),
        "{recovered}"
    );
    assert!(!recovered.contains("FULL"), "{recovered}");

    let preview = fixture
        .command(&[
            "fork",
            "fixture-source",
            "-t",
            "claude",
            "--no-picker",
            "-n",
            "-j",
        ])
        .current_dir(fixture.root.path())
        .output()
        .unwrap();
    assert!(preview.status.success(), "{preview:?}");
    let plan: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(plan["destination"]["profile"], "recovered");
    assert_eq!(before, files(fixture.root.path()));
}
