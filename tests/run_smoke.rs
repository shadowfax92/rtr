//! Smoke test for `rtr run`: drives the runner with a trivial tool and an
//! ephemeral proxy port, asserting the proxy boots, output is tee'd, captures
//! land, and the child's exit code propagates.

use rtr::paths::Paths;
use rtr::runner;

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

    let code = runner::run_tool(&paths, "echotool", &[], false, true)
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
    assert!(run_dir.join("capture.jsonl").exists(), "capture.jsonl missing");

    // The run dir and capture file hold real tokens in normal use: owner-only.
    use std::os::unix::fs::PermissionsExt;
    let dir_mode = std::fs::metadata(&run_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "run dir perms {dir_mode:o}");
    let cap_mode = std::fs::metadata(run_dir.join("capture.jsonl"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(cap_mode, 0o600, "capture.jsonl perms {cap_mode:o}");
}
