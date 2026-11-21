//! Exercises the fork and launch CLI with synthetic histories and a recording
//! child that captures native argv and the selected home.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use rtr::{config::Config, paths::Paths};
use serde_json::{json, Value};

struct Fixture {
    temp: tempfile::TempDir,
    paths: Paths,
    tool: &'static str,
}

impl Fixture {
    fn new(tool: &'static str, defaults: &[&str], wrapper_args: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
        };
        fs::create_dir_all(&paths.config_dir).unwrap();
        let recorder = temp.path().join("record.sh");
        let home_var = if tool == "claude" {
            "CLAUDE_CONFIG_DIR"
        } else {
            "CODEX_HOME"
        };
        fs::write(
            &recorder,
            format!(
                "printf '%s\\0' \"$@\" > \"$RTR_TEST_RECORD/argv\"\nprintf '%s' \"${home_var}\" > \"$RTR_TEST_RECORD/native-home\"\n"
            ),
        )
        .unwrap();
        let command: Vec<_> = ["sh", recorder.to_str().unwrap()]
            .into_iter()
            .chain(wrapper_args.iter().copied())
            .map(|arg| toml::Value::String(arg.into()))
            .collect();
        let defaults = defaults
            .iter()
            .map(|arg| toml::Value::String((*arg).into()))
            .collect();
        fs::write(
            paths.config_file(),
            format!(
                "[tools.{tool}]\ncommand={}\nargs={}\ncopy=[]\n[tools.{tool}.profiles.a]\n[tools.{tool}.profiles.b]\n",
                toml::Value::Array(command),
                toml::Value::Array(defaults)
            ),
        )
        .unwrap();
        Self { temp, paths, tool }
    }

    fn home(&self, profile: &str) -> PathBuf {
        self.paths.profile_home_dir(self.tool, profile)
    }

    fn source(&self, inherited: bool) -> PathBuf {
        let cwd = self.temp.path();
        let (filename, first, last) = if self.tool == "claude" {
            (
                "projects/test/fixture-source.jsonl",
                json!({"type":"user","sessionId":"fixture-source","uuid":"u","cwd":cwd,"message":{"role":"user","content":"fixture prompt"}}),
                json!({"type":"assistant","sessionId":"fixture-source","uuid":"a","parentUuid":"u","message":{"model":"source-model","content":[{"type":"text","text":"done"}]},"effort":"high"}),
            )
        } else {
            (
                "sessions/rollout-fixture-source.jsonl",
                json!({"type":"session_meta","payload":{"id":"fixture-source","cwd":cwd,"history_mode":"legacy"}}),
                json!({"type":"turn_context","payload":{"model":"source-model","effort":"high","turn_id":"turn"}}),
            )
        };
        let path = self.home("a").join(filename);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut contents = format!("{first}\n");
        if inherited {
            contents.push_str(&format!("{last}\n"));
        }
        fs::write(&path, contents).unwrap();
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_rtr"))
            .args(args)
            .current_dir(self.temp.path())
            .env("HOME", self.temp.path())
            .env("RTR_TEST_RECORD", self.temp.path())
            .env("RTR_CONFIG_DIR", &self.paths.config_dir)
            .env("RTR_STATE_DIR", &self.paths.state_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn preview(&self, runtime: &[&str]) -> Value {
        self.preview_selector("fixture-source", runtime)
    }

    fn preview_selector(&self, selector: &str, runtime: &[&str]) -> Value {
        let args = [
            "fork",
            selector,
            "-t",
            self.tool,
            "--no-picker",
            "-n",
            "-j",
            "--",
        ];
        let output = self.run(&[&args[..], runtime].concat());
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn fork(&self, runtime: &[&str]) -> (Value, Output) {
        let report = self.temp.path().join("report.json");
        let args = [
            "fork",
            "fixture-source",
            "-t",
            self.tool,
            "--no-picker",
            "--report",
            report.to_str().unwrap(),
            "--",
        ];
        let output = self.run(&[&args[..], runtime].concat());
        (
            serde_json::from_slice(&fs::read(report).unwrap()).unwrap(),
            output,
        )
    }

    fn argv(&self) -> Vec<String> {
        let args = fs::read_to_string(self.temp.path().join("argv")).unwrap();
        args.split_terminator('\0').map(str::to_owned).collect()
    }

    fn child_home(&self) -> PathBuf {
        PathBuf::from(fs::read_to_string(self.temp.path().join("native-home")).unwrap())
    }

    fn state_bytes(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn visit(path: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    entries.insert(path.clone(), None);
                    visit(&path, entries);
                } else {
                    entries.insert(path.clone(), Some(fs::read(path).unwrap()));
                }
            }
        }
        let mut entries = BTreeMap::new();
        visit(&self.paths.state_dir, &mut entries);
        entries
    }
}

#[test]
fn preview_report_and_recording_child_agree() {
    let fixture = Fixture::new(
        "claude",
        &["--model", "config-model", "--effort", "low"],
        &[],
    );
    fixture.source(true);
    let before = fixture.state_bytes();
    let runtime = ["--model=claude-fable-5-2"];
    let preview = fixture.preview(&runtime);
    assert_eq!(
        preview,
        json!({
            "schema_version": 1,
            "destination": {"tool":"claude","profile":"b"},
            "in_place": false,
            "model": "claude-fable-5-2",
            "effort": "high",
        })
    );
    assert_eq!(fixture.state_bytes(), before);
    assert!(!fixture.temp.path().join("argv").exists());

    let (report, output) = fixture.fork(&runtime);
    for key in [
        "schema_version",
        "destination",
        "in_place",
        "model",
        "effort",
    ] {
        assert_eq!(report[key], preview[key], "{key}");
    }
    assert_eq!(report["source"]["id"], "fixture-source");
    let copied_id = report["id"].as_str().unwrap();
    assert_ne!(copied_id, "fixture-source");
    assert_eq!(
        fixture.argv(),
        [
            "--resume",
            copied_id,
            "--effort",
            "high",
            "--model=claude-fable-5-2"
        ]
    );
    assert_eq!(fixture.child_home(), fixture.home("b"));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("model claude-fable-5-2 · effort high")
    );
}

#[test]
fn cold_preview_by_native_title_leaves_state_byte_identical() {
    for tool in ["claude", "codex"] {
        let fixture = Fixture::new(tool, &[], &[]);
        let transcript = fixture.source(true);
        if tool == "claude" {
            let mut records = fs::read_to_string(&transcript).unwrap();
            records
                .push_str("{\"type\":\"custom-title\",\"customTitle\":\"Native fixture title\"}\n");
            fs::write(transcript, records).unwrap();
        } else {
            fs::write(
                fixture.home("a").join("session_index.jsonl"),
                "{\"id\":\"fixture-source\",\"thread_name\":\"Native fixture title\"}\n",
            )
            .unwrap();
        }
        let before = fixture.state_bytes();
        let preview = fixture.preview_selector("Native fixture title", &[]);
        assert_eq!(preview["destination"]["profile"], "b");
        assert_eq!(preview["model"], "source-model");
        assert_eq!(fixture.state_bytes(), before);
        assert!(!fixture.temp.path().join("argv").exists());
    }
}

#[test]
fn explicit_then_inherited_then_configured_model() {
    for tool in ["claude", "codex"] {
        let fixture = Fixture::new(tool, &["--model", "config-model"], &[]);
        fixture.source(false);
        assert_eq!(fixture.preview(&[])["model"], "config-model");
        fixture.source(true);
        assert_eq!(fixture.preview(&[])["model"], "source-model");
        assert_eq!(
            fixture.preview(&["--model", "explicit-model"])["model"],
            "explicit-model"
        );

        fixture.run(&["resume", "fixture-source", "-t", tool]);
        assert_eq!(fixture.child_home(), fixture.home("a"));
        assert!(fixture.argv().iter().any(|arg| arg == "source-model"));
        fixture.run(&[tool, "--model=explicit-launch"]);
        assert_eq!(fixture.argv(), ["--model=explicit-launch"]);
    }
}

#[test]
fn duplicates_stop_at_the_native_delimiter_and_wrapper_args_stay_opaque() {
    let fixture = Fixture::new(
        "claude",
        &["--model", "config-model", "--effort", "low"],
        &["--model=wrapper", "--effort", "wrapper-effort"],
    );
    fixture.source(true);
    let runtime = [
        "--model",
        "earlier",
        "--model=raw\nmodel",
        "--effort",
        "low",
        "--effort=max",
        "--",
        "--model=literal",
        "--model",
        "literal-second",
        "--effort=literal",
    ];
    let (report, output) = fixture.fork(&runtime);
    assert_eq!(report["model"], "raw\nmodel");
    assert_eq!(report["effort"], "max");
    let copied_id = report["id"].as_str().unwrap();
    assert_eq!(
        fixture.argv(),
        [
            "--model=wrapper",
            "--effort",
            "wrapper-effort",
            "--resume",
            copied_id,
            "--model=raw\nmodel",
            "--effort=max",
            "--",
            "--model=literal",
            "--model",
            "literal-second",
            "--effort=literal"
        ]
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("model raw model · effort max"));
}

#[test]
fn native_delimiter_in_defaults_still_leaves_room_for_the_fork_identity() {
    let fixture = Fixture::new(
        "claude",
        &["--model", "config-model", "--", "--model=prompt"],
        &[],
    );
    fixture.source(true);
    let (report, _) = fixture.fork(&["--model=also-prompt", "--resume", "literal-id"]);
    assert_eq!(report["model"], "source-model");
    assert_eq!(report["effort"], "high");
    assert_eq!(
        fixture.argv(),
        [
            "--resume",
            report["id"].as_str().unwrap(),
            "--model",
            "source-model",
            "--effort",
            "high",
            "--",
            "--model=prompt",
            "--model=also-prompt",
            "--resume",
            "literal-id",
        ]
    );
}

#[test]
fn codex_checks_inherited_model_in_the_selected_home_only() {
    let fixture = Fixture::new(
        "codex",
        &["-c", "model='config-model'", "--config=service_tier='fast'"],
        &[],
    );
    fixture.source(true);
    fs::create_dir_all(fixture.home("b")).unwrap();
    fs::write(
        fixture.home("b").join("models_cache.json"),
        r#"{"models":[{}, {"slug":""}, {"slug":"  "}]}"#,
    )
    .unwrap();
    assert_eq!(fixture.preview(&[])["model"], "source-model");
    for (profile, slug) in [("a", "source-model"), ("b", "config-model")] {
        let home = fixture.home(profile);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("models_cache.json"),
            json!({"models":[{"slug":slug}]}).to_string(),
        )
        .unwrap();
    }
    assert_eq!(fixture.preview(&[])["model"], "config-model");
    assert_eq!(
        fixture.preview(&["-m=explicit-model"])["model"],
        "explicit-model"
    );
    let (report, _) = fixture.fork(&[]);
    assert_eq!(report["model"], "config-model");
    assert_eq!(report["effort"], "high");
    assert_eq!(fixture.child_home(), fixture.home("b"));
    assert_eq!(
        fixture.argv(),
        [
            "-c",
            "model='config-model'",
            "--config=service_tier='fast'",
            "resume",
            report["id"].as_str().unwrap(),
            "-c",
            "model_reasoning_effort=\"high\""
        ]
    );

    fixture.run(&["resume", "fixture-source", "-t", "codex"]);
    assert_eq!(fixture.child_home(), fixture.home("a"));
    assert!(fixture.argv().iter().any(|arg| arg == "source-model"));
}

#[test]
fn codex_preview_and_spawn_use_the_cache_installed_by_startup_copy() {
    let fixture = Fixture::new("codex", &["--model=config-model"], &[]);
    fixture.source(true);
    fs::write(
        fixture.paths.config_dir.join("replacement.json"),
        r#"{"models":[{"slug":"config-model"}]}"#,
    )
    .unwrap();
    let config_file = fixture.paths.config_file();
    let config = fs::read_to_string(&config_file).unwrap().replace(
        "copy=[]",
        "copy=[{source='replacement.json',destination='models_cache.json'}]",
    );
    fs::write(config_file, config).unwrap();

    let before = fixture.state_bytes();
    let preview = fixture.preview(&[]);
    assert_eq!(preview["model"], "config-model");
    assert_eq!(fixture.state_bytes(), before);
    let (report, _) = fixture.fork(&[]);
    assert_eq!(report["model"], preview["model"]);
    assert_eq!(fixture.child_home(), fixture.home("b"));
    assert_eq!(
        fixture.argv(),
        [
            "--model=config-model",
            "resume",
            report["id"].as_str().unwrap(),
            "-c",
            "model_reasoning_effort=\"high\"",
        ]
    );
}

#[test]
fn codex_scalar_and_config_model_forms_share_precedence() {
    let fixture = Fixture::new(
        "codex",
        &["-m", "config-model", "-c", "service_tier='fast'"],
        &[],
    );
    fixture.source(true);
    let runtime = [
        "-m=earlier",
        "-cmodel='middle'",
        "--model=latest-scalar",
        "--config=model='latest-config'",
        "--config=model_reasoning_effort='medium'",
        "-c=model_reasoning_effort='xhigh'",
        "-c",
        "service_tier='flex'",
        "--",
        "-mliteral",
    ];
    let (report, _) = fixture.fork(&runtime);
    assert_eq!(report["model"], "latest-config");
    assert_eq!(report["effort"], "xhigh");
    assert_eq!(
        fixture.argv(),
        [
            "resume",
            report["id"].as_str().unwrap(),
            "--config=model='latest-config'",
            "-c=model_reasoning_effort='xhigh'",
            "-c",
            "service_tier='flex'",
            "--",
            "-mliteral"
        ]
    );
}

#[test]
fn legacy_config_inherits_routing_policy_and_invalid_fallback_names_its_key() {
    let legacy = "[tools.claude]\ncommand=['claude']\n";
    let config = Config::parse(legacy).unwrap();
    assert!(config.routing.claude.auto_drop);
    assert_eq!(config.routing.claude.fallback_model, "claude-opus-5-5");
    let off = Config::parse(&format!("{legacy}\n[routing.claude]\nauto_drop=false\n")).unwrap();
    assert!(!off.routing.claude.auto_drop);
    assert_eq!(off.routing.claude.fallback_model, "claude-opus-5-5");

    for variant in ["opus", "claude-opus-5-5[1m]", "claude-opus-4-6"] {
        let parsed = Config::parse(&format!(
            "{legacy}\n[routing.claude]\nfallback_model='{variant}'\n"
        ))
        .unwrap();
        assert!(parsed.routing.claude.auto_drop);
        assert_eq!(parsed.routing.claude.fallback_model, variant);
    }
    for invalid in [
        "",
        "claude-fable-5-2",
        "fable",
        "bogus-opus",
        "claude-opus-extra",
        "claude-opus-9-9-00000000",
        "opus[bogus]",
    ] {
        let error = Config::parse(&format!(
            "{legacy}\n[routing.claude]\nfallback_model='{invalid}'\n"
        ))
        .unwrap_err();
        assert!(
            error.to_string().contains("routing.claude.fallback_model"),
            "{error:#}"
        );
    }
}
