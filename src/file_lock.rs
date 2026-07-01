use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub(crate) fn lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("lock"));
    name.push(".lock");
    path.with_file_name(name)
}

/// Run a filesystem update while holding an exclusive advisory lock file.
pub(crate) fn with_exclusive_lock<R, F>(lock_path: &Path, update: F) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = lock_path.parent() {
        crate::paths::create_private_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(lock_path)
        .with_context(|| format!("opening lock {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    let result = update();
    let unlock = file
        .unlock()
        .with_context(|| format!("unlocking {}", lock_path.display()));
    match (result, unlock) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

/// Write a 0600 file through a temp path and rename so readers never see partial state.
pub(crate) fn write_private_atomic(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        crate::paths::create_private_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .context("atomic write path has no file name")?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(
        ".{}-{}.tmp",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let tmp_path = path.with_file_name(tmp_name);

    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .with_context(|| format!("opening temp file {}", tmp_path.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("writing temp file {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temp file {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}
