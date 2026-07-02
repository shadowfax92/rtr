//! Filesystem locations for rtr config, state, CA, and per-run artifacts.
//!
//! Config lives under `$RTR_CONFIG_DIR` (default `$HOME/.config/rtr`); run logs
//! and the active-profile state live under `$RTR_STATE_DIR` (default
//! `$HOME/.local/state/rtr`). The overrides exist mainly so tests can point at a
//! temp dir without touching the real home.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Recursively create a directory tree owned-only (0700). Used for every dir
/// that holds secrets (config, CA, per-run captures) so an overridden
/// `RTR_*_DIR` under a world-traversable path can't expose them.
pub fn create_private_dir_all(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

/// Resolve a base directory: an explicit override wins, else `home` joined with
/// the default `suffix` segments.
fn resolve(override_dir: Option<PathBuf>, home: &Path, suffix: &[&str]) -> PathBuf {
    match override_dir {
        Some(dir) => dir,
        None => {
            let mut p = home.to_path_buf();
            for s in suffix {
                p.push(s);
            }
            p
        }
    }
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let config_dir = resolve(
            std::env::var_os("RTR_CONFIG_DIR").map(PathBuf::from),
            &home,
            &[".config", "rtr"],
        );
        let state_dir = resolve(
            std::env::var_os("RTR_STATE_DIR").map(PathBuf::from),
            &home,
            &[".local", "state", "rtr"],
        );
        Ok(Self {
            config_dir,
            state_dir,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.toml")
    }

    pub fn usage_file(&self) -> PathBuf {
        self.state_dir.join("usage.jsonl")
    }

    pub fn ca_dir(&self) -> PathBuf {
        self.config_dir.join("ca")
    }

    pub fn ca_cert(&self) -> PathBuf {
        self.ca_dir().join("rtr-ca.cert.pem")
    }

    pub fn ca_key(&self) -> PathBuf {
        self.ca_dir().join("rtr-ca.key.pem")
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.state_dir.join("runs")
    }

    /// Directory for one run's artifacts: `state/runs/<tool>/<stamp>/`.
    pub fn run_dir(&self, tool: &str, stamp: &str) -> PathBuf {
        self.runs_dir().join(tool).join(stamp)
    }

    pub fn homes_dir(&self) -> PathBuf {
        self.state_dir.join("homes")
    }

    /// Native config/auth home for one first-class tool profile.
    pub fn profile_home_dir(&self, tool: &str, profile: &str) -> PathBuf {
        self.homes_dir()
            .join(safe_path_segment(tool))
            .join(safe_path_segment(profile))
    }
}

fn safe_path_segment(value: &str) -> String {
    if value.is_empty() {
        return "%00".to_string();
    }
    if value != "." && value != ".." && value.bytes().all(is_safe_segment_byte) {
        return value.to_string();
    }

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if is_safe_segment_byte(byte) && !(value == "." || value == "..") {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

fn is_safe_segment_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_override() {
        let home = Path::new("/home/x");
        let got = resolve(
            Some(PathBuf::from("/tmp/custom")),
            home,
            &[".config", "rtr"],
        );
        assert_eq!(got, PathBuf::from("/tmp/custom"));
    }

    #[test]
    fn resolve_falls_back_to_home_suffix() {
        let home = Path::new("/home/x");
        let got = resolve(None, home, &[".config", "rtr"]);
        assert_eq!(got, PathBuf::from("/home/x/.config/rtr"));
    }

    #[test]
    fn derived_paths_join_correctly() {
        let p = Paths {
            config_dir: PathBuf::from("/c"),
            state_dir: PathBuf::from("/s"),
        };
        assert_eq!(p.config_file(), PathBuf::from("/c/config.toml"));
        assert_eq!(p.state_file(), PathBuf::from("/s/state.toml"));
        assert_eq!(p.ca_cert(), PathBuf::from("/c/ca/rtr-ca.cert.pem"));
        assert_eq!(p.ca_key(), PathBuf::from("/c/ca/rtr-ca.key.pem"));
        assert_eq!(
            p.run_dir("codex", "20260611-105500"),
            PathBuf::from("/s/runs/codex/20260611-105500")
        );
    }

    #[test]
    fn profile_home_dir_uses_readable_safe_segments() {
        let p = Paths {
            config_dir: PathBuf::from("/c"),
            state_dir: PathBuf::from("/s"),
        };
        assert_eq!(
            p.profile_home_dir("codex", "personal"),
            PathBuf::from("/s/homes/codex/personal")
        );
    }

    #[test]
    fn profile_home_dir_encodes_unsafe_profile_segments() {
        let p = Paths {
            config_dir: PathBuf::from("/c"),
            state_dir: PathBuf::from("/s"),
        };

        assert_eq!(
            p.profile_home_dir("codex", "../work profile")
                .strip_prefix("/s/homes/codex")
                .unwrap(),
            Path::new("..%2Fwork%20profile")
        );
        assert_eq!(
            p.profile_home_dir("codex", "uni❤️")
                .strip_prefix("/s/homes/codex")
                .unwrap(),
            Path::new("uni%E2%9D%A4%EF%B8%8F")
        );
    }

    #[test]
    fn profile_home_dir_is_deterministic() {
        let p = Paths {
            config_dir: PathBuf::from("/c"),
            state_dir: PathBuf::from("/s"),
        };
        assert_eq!(
            p.profile_home_dir("claude", "work/team"),
            p.profile_home_dir("claude", "work/team")
        );
    }
}
