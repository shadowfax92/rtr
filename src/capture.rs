//! Structured capture of intercepted requests, written as JSON Lines.
//!
//! Records store the *real* header values (the point is to discover the auth
//! header to swap); terminal display redaction lives in [`crate::rewrite`].
//! The sink is cloneable and behind a mutex so the proxy handler can share one
//! across connections; an in-memory variant backs tests.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CaptureRecord {
    pub ts: String,
    pub method: String,
    pub url: String,
    pub host: String,
    pub headers: Vec<(String, String)>,
}

impl CaptureRecord {
    pub fn new(method: String, url: String, host: String, headers: Vec<(String, String)>) -> Self {
        Self {
            ts: now_rfc3339(),
            method,
            url,
            host,
            headers,
        }
    }
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Filesystem-safe local timestamp for per-run directory names.
pub fn file_stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

#[derive(Clone)]
pub struct CaptureSink {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl CaptureSink {
    pub fn to_file(path: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // 0600: capture records hold the real auth headers.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening capture file {}", path.display()))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Box::new(file))),
        })
    }

    /// In-memory sink plus a handle to read what was written (for tests).
    pub fn in_memory() -> (Self, SharedBuf) {
        let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let sink = Self {
            inner: Arc::new(Mutex::new(Box::new(buf.clone()))),
        };
        (sink, buf)
    }

    pub fn record(&self, rec: &CaptureRecord) -> Result<()> {
        let mut guard = self.inner.lock().expect("capture sink poisoned");
        serde_json::to_writer(&mut *guard, rec).context("serializing capture record")?;
        guard.write_all(b"\n").context("writing capture record")?;
        guard.flush().context("flushing capture record")?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    pub fn contents_string(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("buf poisoned")).into_owned()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("buf poisoned").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serializes_to_one_json_line() {
        let rec = CaptureRecord {
            ts: "2026-06-11T10:55:00Z".to_string(),
            method: "POST".to_string(),
            url: "https://api.openai.com/v1/responses".to_string(),
            host: "api.openai.com".to_string(),
            headers: vec![("authorization".to_string(), "Bearer sk-real".to_string())],
        };
        let line = serde_json::to_string(&rec).unwrap();
        assert!(!line.contains('\n'));
        let back: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back["method"], "POST");
        assert_eq!(back["host"], "api.openai.com");
        assert_eq!(back["headers"][0][0], "authorization");
        assert_eq!(back["headers"][0][1], "Bearer sk-real");
        assert_eq!(back["ts"], "2026-06-11T10:55:00Z");
    }

    #[test]
    fn in_memory_sink_writes_jsonl() {
        let (sink, buf) = CaptureSink::in_memory();
        sink.record(&CaptureRecord::new(
            "GET".into(),
            "https://h/a".into(),
            "h".into(),
            vec![],
        ))
        .unwrap();
        sink.record(&CaptureRecord::new(
            "GET".into(),
            "https://h/b".into(),
            "h".into(),
            vec![],
        ))
        .unwrap();
        let contents = buf.contents_string();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"url\":\"https://h/a\""));
        assert!(lines[1].contains("\"url\":\"https://h/b\""));
    }

    #[test]
    fn now_and_stamp_are_nonempty() {
        assert!(now_rfc3339().contains('T'));
        assert_eq!(file_stamp().len(), "YYYYmmdd-HHMMSS".len());
    }
}
