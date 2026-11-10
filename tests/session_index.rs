//! Concurrent processes share only RTR's disposable cache, never native writes.
mod support;
use rtr::paths::Paths;
use serde_json::{json, Value};
use std::{
    fs,
    process::{Command, Stdio},
};

#[test]
fn simultaneous_catalog_builds_publish_complete_private_cache_and_reuse_it() {
    let temp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: temp.path().join("config"),
        state_dir: temp.path().join("state"),
    };
    fs::create_dir_all(&paths.config_dir).unwrap();
    fs::write(
        paths.config_file(),
        "[tools.claude]\ncommand=[\"false\"]\n[tools.claude.profiles.test]\n",
    )
    .unwrap();
    let project = paths
        .profile_home_dir("claude", "test")
        .join("projects/project");
    fs::create_dir_all(&project).unwrap();
    for i in 0..40 {
        let line=json!({"type":"user","sessionId":format!("synthetic-{i}"),"cwd":"/synthetic/project","message":{"content":format!("Synthetic question {i}")}}).to_string()+"\n";
        fs::write(project.join(format!("synthetic-{i}.jsonl")), line).unwrap();
    }
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_rtr"))
            .args(["sessions", "--json"])
            .env("RTR_CONFIG_DIR", &paths.config_dir)
            .env("RTR_STATE_DIR", &paths.state_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn();
    let second = spawn();
    for child in [first, second, spawn()] {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let catalog: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(catalog["conversations"].as_array().unwrap().len(), 40);
        assert_eq!(catalog["diagnostics"], json!([]));
    }
    let manifest: Value = serde_json::from_slice(
        &fs::read(paths.state_dir.join("session-index/catalog.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["entries"].as_object().unwrap().len(), 40);
    // Two independent terminal processes share the new mapped storage. Starting
    // from JSON-only checkpoints forces snapshot publication, not just reuse.
    {
        let barrier = std::sync::Barrier::new(2);
        let run = || {
            barrier.wait();
            support::drive_picker(
                &paths,
                temp.path(),
                &["sessions"],
                "40 / 40 sessions",
                b"\x03",
            );
        };
        std::thread::scope(|scope| {
            let a = scope.spawn(run);
            let b = scope.spawn(run);
            a.join().unwrap();
            b.join().unwrap();
        });
    }
    let snapshot = fs::read(paths.state_dir.join("session-index/picker-v5.bin")).unwrap();
    assert_eq!(&snapshot[..8], b"RTRPK005");
    let header_len = u64::from_le_bytes(snapshot[8..16].try_into().unwrap()) as usize;
    let header: Value = serde_json::from_slice(&snapshot[16..16 + header_len]).unwrap();
    assert_eq!(header["entries"].as_array().unwrap().len(), 40);
    // Source bytes and mtimes remain untouched by both builders.
    for i in 0..40 {
        assert_eq!(fs::metadata(project.join(format!("synthetic-{i}.jsonl"))).unwrap().len(),json!({"type":"user","sessionId":format!("synthetic-{i}"),"cwd":"/synthetic/project","message":{"content":format!("Synthetic question {i}")}}).to_string().len() as u64+1);
    }
}
