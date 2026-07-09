//! Direct child execution with isolated native homes and synchronized skills.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::config::{Config, Tool};
use crate::paths::Paths;
use crate::selection;
use crate::state::State;
use crate::{tool_specs, usage};

struct PreparedSubscriptionRun {
    profile_name: String,
    child_args: Vec<String>,
    child_env: Vec<(String, std::ffi::OsString)>,
}

#[derive(Debug)]
struct SkillsSource {
    path: PathBuf,
    explicit: bool,
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// Prepare the selected profile's native home, skills, environment, and arguments.
fn prepare_subscription_run(
    paths: &Paths,
    spec: &tool_specs::ToolSpec,
    tool: &Tool,
    profile_name: &str,
    runtime_args: &[String],
) -> Result<PreparedSubscriptionRun> {
    let profile = tool.profiles.get(profile_name).with_context(|| {
        format!(
            "profile '{profile_name}' disappeared for tool '{}'",
            spec.name
        )
    })?;
    if !profile.enabled {
        bail!("profile '{}/{}' is disabled", spec.name, profile_name);
    }
    let child_env = prepare_native_profile_env(paths, spec, tool, profile_name)?;
    Ok(PreparedSubscriptionRun {
        profile_name: profile_name.to_string(),
        child_args: runtime_args.to_vec(),
        child_env,
    })
}

fn prepare_native_profile_env(
    paths: &Paths,
    spec: &tool_specs::ToolSpec,
    tool: &Tool,
    profile_name: &str,
) -> Result<Vec<(String, std::ffi::OsString)>> {
    let home = paths.ensure_profile_home_dir(spec.name, profile_name)?;
    let user_home = crate::home_dir()?;
    sync_profile_skills(spec, tool, &home, &paths.config_dir, &user_home)?;
    Ok(vec![(
        spec.native_home_env.to_string(),
        home.into_os_string(),
    )])
}

/// Refresh the selected native home from the tool's configured skills source.
fn sync_profile_skills(
    spec: &tool_specs::ToolSpec,
    tool: &Tool,
    profile_home: &Path,
    config_dir: &Path,
    home: &Path,
) -> Result<()> {
    crate::file_lock::with_exclusive_lock(&profile_home.join(".skills-sync.lock"), || {
        sync_profile_skills_locked(spec, tool, profile_home, config_dir, home)
    })
}

fn sync_profile_skills_locked(
    spec: &tool_specs::ToolSpec,
    tool: &Tool,
    profile_home: &Path,
    config_dir: &Path,
    home: &Path,
) -> Result<()> {
    let source = skills_source(spec, tool, home, config_dir)?;
    let destination = profile_home.join("skills");

    match std::fs::metadata(&source.path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => bail!(
            "skills source {} must be a directory",
            source.path.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !source.explicit => {
            remove_existing_path(&destination)?;
            return Ok(());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "configured skills source {} does not exist",
                source.path.display()
            );
        }
        Err(err) => return Err(err).with_context(|| format!("stat {}", source.path.display())),
    }

    ensure_distinct_copy_paths(&source.path, &destination)?;
    replace_skills_dir(&source.path, &destination)
}

fn skills_source(
    spec: &tool_specs::ToolSpec,
    tool: &Tool,
    home: &Path,
    config_dir: &Path,
) -> Result<SkillsSource> {
    if let Some(path) = &tool.skills_source {
        let path = expand_home_path(path, home)?;
        let path = if path.is_relative() {
            config_dir.join(path)
        } else {
            path
        };
        return Ok(SkillsSource {
            path,
            explicit: true,
        });
    }

    let mut path = home.to_path_buf();
    for segment in spec.default_skills_source {
        path.push(segment);
    }
    Ok(SkillsSource {
        path,
        explicit: false,
    })
}

fn expand_home_path(path: &Path, home: &Path) -> Result<PathBuf> {
    let raw = path.as_os_str().to_string_lossy();
    if raw == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home.join(rest));
    }
    if raw.starts_with('~') {
        bail!("configured paths support only '~' or '~/' home expansion: {raw}");
    }
    Ok(path.to_path_buf())
}

fn ensure_distinct_copy_paths(source: &Path, destination: &Path) -> Result<()> {
    let source = source
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", source.display()))?;
    let destination_parent = destination
        .parent()
        .with_context(|| format!("{} has no parent", destination.display()))?
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", destination.display()))?;
    if destination_parent.starts_with(&source) {
        bail!(
            "skills source {} must not contain destination {}",
            source.display(),
            destination.display()
        );
    }

    if let Ok(destination) = destination.canonicalize() {
        if source == destination || source.starts_with(&destination) {
            bail!(
                "skills source {} must not be inside destination {}",
                source.display(),
                destination.display()
            );
        }
    }
    Ok(())
}

fn remove_existing_path(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    };

    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
    } else {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
    }
}

fn replace_skills_dir(source: &Path, destination: &Path) -> Result<()> {
    let temp = temporary_skills_path(destination);
    remove_existing_path(&temp)?;

    let result = (|| -> Result<()> {
        crate::paths::create_private_dir_all(&temp)?;
        copy_dir_contents(source, &temp)?;
        remove_existing_path(destination)?;
        std::fs::rename(&temp, destination)
            .with_context(|| format!("renaming {} to {}", temp.display(), destination.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = remove_existing_path(&temp);
    }
    result
}

fn temporary_skills_path(destination: &Path) -> PathBuf {
    let stamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    destination.with_file_name(format!(".skills.{}-{stamp}.tmp", std::process::id()))
}

fn copy_dir_contents(source: &Path, destination: &Path) -> Result<()> {
    for entry in
        std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("reading {}", source.display()))?;
        copy_path(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn copy_path(source: &Path, destination: &Path) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(source).with_context(|| format!("stat {}", source.display()))?;
    if meta.file_type().is_symlink() {
        let target =
            std::fs::read_link(source).with_context(|| format!("readlink {}", source.display()))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, destination).with_context(|| {
            format!("symlink {} -> {}", destination.display(), target.display())
        })?;
        #[cfg(not(unix))]
        bail!(
            "copying symlinked skills is not supported on this platform: {}",
            source.display()
        );
        return Ok(());
    }
    if meta.is_dir() {
        crate::paths::create_private_dir_all(destination)?;
        return copy_dir_contents(source, destination);
    }
    if meta.is_file() {
        std::fs::copy(source, destination).with_context(|| {
            format!("copying {} to {}", source.display(), destination.display())
        })?;
        return Ok(());
    }
    bail!("unsupported skills entry type: {}", source.display());
}

/// Launch one selected profile directly and record its outcome.
pub async fn run_subscription_tool(
    paths: &Paths,
    tool_name: &str,
    forced_profile: Option<&str>,
    runtime_args: &[String],
) -> Result<i32> {
    let spec = tool_specs::get(tool_name)?;
    let config_path = paths.config_file();
    if !config_path.exists() {
        bail!(
            "no config at {} — run `rtr init` first",
            config_path.display()
        );
    }
    let config = Config::load(&config_path)?;
    let tool = config.tool(spec.name)?.clone();
    if tool.command.is_empty() {
        bail!("tool '{}' has an empty command", spec.name);
    }

    let state_path = paths.state_file();
    let prepared = if let Some(profile_name) = forced_profile {
        prepare_subscription_run(paths, spec, &tool, profile_name, runtime_args)?
    } else {
        State::update_locked(&state_path, |state| {
            let profile_name = selection::select_profile(spec.name, &tool, state, None)?;
            prepare_subscription_run(paths, spec, &tool, &profile_name, runtime_args)
        })?
    };

    let result = execute_tool(&tool, prepared.child_args, prepared.child_env).await;
    let child_exit_code = result.as_ref().ok().copied();
    usage::append_event(
        &paths.usage_file(),
        &usage::new_event(spec.name, &prepared.profile_name, child_exit_code),
    )?;
    result
}

async fn execute_tool(
    tool: &Tool,
    child_args: Vec<String>,
    child_env: Vec<(String, std::ffi::OsString)>,
) -> Result<i32> {
    let mut command = Command::new(&tool.command[0]);
    command.args(&tool.command[1..]).args(child_args);
    for (key, value) in child_env {
        command.env(key, value);
    }
    command.kill_on_drop(true);
    let status = command
        .status()
        .await
        .with_context(|| format!("spawning '{}'", tool.command[0]))?;
    Ok(exit_code(status))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toml_path(path: &Path) -> String {
        toml::Value::String(path.display().to_string()).to_string()
    }

    #[test]
    fn skills_source_defaults_to_tool_home_skills() {
        let config = Config::parse("[tools.codex]\ncommand=[\"codex\"]\n").unwrap();
        let source = skills_source(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            Path::new("/home/me"),
            Path::new("/config"),
        )
        .unwrap();
        assert!(!source.explicit);
        assert_eq!(source.path, PathBuf::from("/home/me/.codex/skills"));
    }

    #[test]
    fn skills_source_expands_configured_home_path() {
        let config =
            Config::parse("[tools.codex]\ncommand=[\"codex\"]\nskills_source=\"~/.skills\"\n")
                .unwrap();
        let source = skills_source(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            Path::new("/home/me"),
            Path::new("/config"),
        )
        .unwrap();
        assert!(source.explicit);
        assert_eq!(source.path, PathBuf::from("/home/me/.skills"));
    }

    #[test]
    fn skills_source_resolves_relative_path_from_config_dir() {
        let config =
            Config::parse("[tools.codex]\ncommand=[\"codex\"]\nskills_source=\"skills\"\n")
                .unwrap();
        let source = skills_source(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            Path::new("/home/me"),
            Path::new("/config/rtr"),
        )
        .unwrap();
        assert_eq!(source.path, PathBuf::from("/config/rtr/skills"));
    }

    #[test]
    fn skills_source_rejects_unsupported_home_expansion() {
        let config = Config::parse(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source=\"~someone/skills\"\n",
        )
        .unwrap();
        let error = skills_source(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            Path::new("/home/me"),
            Path::new("/config"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains('~'), "{error}");
    }

    #[test]
    fn sync_profile_skills_overwrites_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::create_dir_all(profile_home.join("skills")).unwrap();
        std::fs::write(source.join("root.md"), "root").unwrap();
        std::fs::write(source.join("nested/child.md"), "child").unwrap();
        std::fs::write(profile_home.join("skills/stale.md"), "stale").unwrap();

        let config = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n",
            toml_path(&source)
        ))
        .unwrap();
        sync_profile_skills(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            Path::new("/home/me"),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(profile_home.join("skills/root.md")).unwrap(),
            "root"
        );
        assert_eq!(
            std::fs::read_to_string(profile_home.join("skills/nested/child.md")).unwrap(),
            "child"
        );
        assert!(!profile_home.join("skills/stale.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn sync_profile_skills_keeps_destination_when_copy_fails() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(profile_home.join("skills")).unwrap();
        std::fs::write(profile_home.join("skills/stale.md"), "stale").unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(source.join("unsupported"))
            .status()
            .unwrap()
            .success());

        let config = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n",
            toml_path(&source)
        ))
        .unwrap();
        assert!(sync_profile_skills(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            Path::new("/home/me"),
        )
        .is_err());
        assert_eq!(
            std::fs::read_to_string(profile_home.join("skills/stale.md")).unwrap(),
            "stale"
        );
    }

    #[test]
    fn sync_profile_skills_removes_destination_when_default_source_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(profile_home.join("skills")).unwrap();
        std::fs::write(profile_home.join("skills/stale.md"), "stale").unwrap();
        let config = Config::parse("[tools.codex]\ncommand=[\"codex\"]\n").unwrap();

        sync_profile_skills(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            &dir.path().join("home"),
        )
        .unwrap();
        assert!(!profile_home.join("skills").exists());
    }
}
