//! Exercise the log through the real CLI in a disposable state directory.

use chrono::{TimeZone, Utc};
use rtr::routing_log::{append_batch, EventKind, Label, RoutingEvent, Tool};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::sync::Barrier;

const EVENT_TIME: &str = "2026-11-21T17:55:00Z";

struct Fixture {
    temp: tempfile::TempDir,
    paths: rtr::paths::Paths,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = rtr::paths::Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
        };
        Self { temp, paths }
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_rtr"))
            .arg("log")
            .args(args)
            .env("HOME", self.temp.path())
            .env("RTR_CONFIG_DIR", &self.paths.config_dir)
            .env("RTR_STATE_DIR", &self.paths.state_dir)
            .env("TZ", "UTC")
            .current_dir(self.temp.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        output
    }

    fn log_path(&self) -> std::path::PathBuf {
        self.paths.state_dir.join("routing.jsonl")
    }

    fn seed(&self, bytes: impl AsRef<[u8]>) {
        fs::create_dir_all(&self.paths.state_dir).unwrap();
        fs::write(self.log_path(), bytes).unwrap();
    }
}

fn fork(id: &str, tool: &str, source: &str, destination: &str, at: &str) -> Value {
    json!({
        "version": 1,
        "kind": "fork_prepared",
        "at": at,
        "operation_id": id,
        "tool": tool,
        "source": source,
        "destination": destination,
        "in_place": false,
        "prepared_id": "fixture-native-id"
    })
}

fn assert_skipped(stderr: &[u8], expected: usize) {
    let warning = std::str::from_utf8(stderr).unwrap();
    assert_eq!(warning.lines().count(), 1, "{warning}");
    let count = warning
        .split_whitespace()
        .find_map(|word| word.parse::<usize>().ok());
    assert_eq!(count, Some(expected), "{warning}");
}

#[test]
fn first_use_and_zero_count_show_empty_without_creating_state() {
    let fixture = Fixture::new();
    let missing = fixture.run(&["-j"]);
    assert_eq!(missing.stdout, b"[]\n");
    assert!(missing.stderr.is_empty());
    assert!(fixture.run(&[]).stderr.is_empty());
    assert!(!fixture.paths.state_dir.exists());
    assert!(!fixture.paths.config_dir.exists());

    fixture.seed(b"malformed\n");
    let zero = fixture.run(&["-n", "0", "-j"]);
    assert_eq!(zero.stdout, b"[]\n");
    assert!(zero.stderr.is_empty());
    assert_eq!(fs::read(fixture.log_path()).unwrap(), b"malformed\n");
}

#[test]
fn tail_filters_both_profiles_and_preserves_append_order_without_writing() {
    let fixture = Fixture::new();
    let source = fork("fixture-source", "claude", "eng", "ops", EVENT_TIME);
    let destination = fork(
        "fixture-destination",
        "claude",
        "ops",
        "eng",
        "2026-11-21T17:54:00Z",
    );
    let other_tool = fork(
        "fixture-codex",
        "codex",
        "eng",
        "ops",
        "2026-11-21T17:56:00Z",
    );
    let other_profile = fork(
        "fixture-other",
        "claude",
        "alt",
        "ops",
        "2026-11-21T17:57:00Z",
    );
    let torn = fork(
        "fixture-torn",
        "claude",
        "eng",
        "ops",
        "2026-11-21T17:58:00Z",
    );
    let mut unsupported = source.clone();
    unsupported["version"] = json!(0);
    let bytes =
        format!("{unsupported}\n{source}\n{destination}\n{other_tool}\n{other_profile}\n{torn}");
    fixture.seed(bytes.as_bytes());
    // A broken config stands in for an installation where its profiles cannot be resolved.
    fs::create_dir(&fixture.paths.config_dir).unwrap();
    fs::write(fixture.paths.config_file(), "invalid TOML [").unwrap();

    let filtered = fixture.run(&["-n", "2", "-t", "claude", "-p", "eng", "-j"]);
    let actual: Value = serde_json::from_slice(&filtered.stdout).unwrap();
    assert_eq!(actual, json!([source, destination]));
    assert_skipped(&filtered.stderr, 1);

    let plain = fixture.run(&[]);
    let rendered = String::from_utf8(plain.stdout).unwrap();
    let source_index = rendered.find("claude/eng → ops").unwrap();
    let destination_index = rendered.find("claude/ops → eng").unwrap();
    assert!(source_index < destination_index, "{rendered}");
    assert!(rendered.contains("Nov 21 17:55"), "{rendered}");
    assert!(rendered.contains("fork prepared"), "{rendered}");
    assert_skipped(&plain.stderr, 2);
    assert_eq!(fs::read(fixture.log_path()).unwrap(), bytes.as_bytes());
    assert_eq!(fs::read_dir(&fixture.paths.state_dir).unwrap().count(), 1);
}

#[test]
fn reverse_tail_skips_a_large_bad_row_and_defaults_to_twenty_matches() {
    let fixture = Fixture::new();
    let valid: Vec<_> = (0..24)
        .map(|i| fork(&format!("fixture-{i}"), "claude", "eng", "ops", EVENT_TIME))
        .collect();
    let mut bytes = Vec::new();
    for (i, event) in valid.iter().enumerate() {
        if i == 10 {
            bytes.extend(std::iter::repeat_n(b'x', 1024 * 1024));
            bytes.push(b'\n');
        }
        bytes.extend(serde_json::to_vec(event).unwrap());
        bytes.push(b'\n');
    }
    fixture.seed(bytes);

    let output = fixture.run(&["-j"]);
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual, json!(valid[4..]));
    assert_skipped(&output.stderr, 1);
}

fn prepared(operation_id: &str, prepared_id: &str) -> RoutingEvent {
    RoutingEvent::new(
        Utc.with_ymd_and_hms(2026, 11, 21, 17, 55, 0).unwrap(),
        operation_id,
        Tool::Claude,
        EventKind::ForkPrepared {
            source: Label::new("eng"),
            destination: Label::new("ops"),
            in_place: false,
            prepared_id: Some(Label::new(prepared_id)),
        },
    )
}

#[test]
fn concurrent_writers_keep_batches_contiguous_and_state_private() {
    let fixture = Fixture::new();
    let writers = 8;
    let start = Barrier::new(writers);
    std::thread::scope(|threads| {
        for writer in 0..writers {
            let paths = &fixture.paths;
            let start = &start;
            threads.spawn(move || {
                let id = format!("fixture-writer-{writer}");
                let batch = [prepared(&id, "first"), prepared(&id, "second")];
                start.wait();
                append_batch(paths, &batch);
            });
        }
    });

    let output = fixture.run(&["-j"]);
    assert!(output.stderr.is_empty(), "{output:?}");
    let rows: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows.len(), writers * 2);
    let mut ids = std::collections::BTreeSet::new();
    for batch in rows.chunks_exact(2) {
        let id = batch[0]["operation_id"].as_str().unwrap();
        assert!(ids.insert(id), "duplicate batch {id}");
        let mut expected = fork(id, "claude", "eng", "ops", EVENT_TIME);
        expected["prepared_id"] = json!("first");
        assert_eq!(batch[0], expected);
        expected["prepared_id"] = json!("second");
        assert_eq!(batch[1], expected);
    }

    let mode = |path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(fixture.paths.state_dir.clone()), 0o700);
    assert_eq!(mode(fixture.log_path()), 0o600);
    assert_eq!(
        mode(fixture.paths.state_dir.join("routing.jsonl.lock")),
        0o600
    );
}

#[test]
fn append_after_a_torn_tail_preserves_the_prefix_and_only_shows_committed_rows() {
    let fixture = Fixture::new();
    let before = fork("before", "claude", "eng", "ops", EVENT_TIME);
    let torn = fork("torn", "claude", "eng", "ops", "2026-11-21T17:56:00Z");
    let original = format!("{before}\n{torn}");
    fixture.seed(original.as_bytes());
    append_batch(&fixture.paths, &[prepared("after", "native\n\x1b[31m-id")]);

    let output = fixture.run(&["-j"]);
    let rows: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], before);
    assert_eq!(rows[1]["operation_id"], "after");
    let id = rows[1]["prepared_id"].as_str().unwrap();
    assert!(id.starts_with("native"));
    assert!(id.ends_with("[31m-id"));
    assert!(!id.chars().any(char::is_control));
    assert_skipped(&output.stderr, 1);
    let bytes = fs::read(fixture.log_path()).unwrap();
    assert!(bytes.starts_with(original.as_bytes()));
    assert_eq!(bytes.split(|&byte| byte == b'\n').count(), 4);
    assert!(!bytes.contains(&0x1b));
}
