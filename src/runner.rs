//! Direct child execution with isolated native homes and synchronized skills.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio::process::Command;
use tokio::signal::unix::{signal, Signal, SignalKind};

use crate::config::{self, Config, CopyMapping, Tool};
use crate::paths::Paths;
use crate::selection;
use crate::state::State;
use crate::{tool_specs, usage};

#[derive(Debug)]
struct PreparedSubscriptionRun {
    profile_name: String,
    child_args: Vec<String>,
    child_env: Vec<(String, std::ffi::OsString)>,
    child_env_remove: Vec<String>,
    child_cwd: Option<PathBuf>,
    bypass: bool,
}

#[derive(Debug)]
struct SkillsSource {
    path: PathBuf,
    explicit: bool,
}

#[derive(Debug)]
struct ResolvedCopyMapping {
    source: PathBuf,
    destination: PathBuf,
    source_resolved: PathBuf,
    destination_resolved: PathBuf,
}

#[derive(Debug)]
struct InstalledCopy {
    destination: PathBuf,
    backup: PathBuf,
    had_destination: bool,
}

struct ChildSignals {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
    quit: Signal,
}

/// Restore rtr's foreground terminal ownership after its child exits.
struct ForegroundTerminal {
    fd: i32,
    original_process_group: i32,
    restored: bool,
}

impl ForegroundTerminal {
    fn handoff(process_group: i32) -> Result<Option<Self>> {
        let fd = libc::STDIN_FILENO;
        // SAFETY: these calls only read terminal state for a valid process fd.
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(None);
        }
        let original_process_group = unsafe { libc::tcgetpgrp(fd) };
        if original_process_group < 0 {
            return Err(std::io::Error::last_os_error())
                .context("reading foreground process group");
        }
        // A background invocation must remain under the shell's job control.
        if original_process_group != unsafe { libc::getpgrp() } {
            return Ok(None);
        }
        set_foreground_process_group(fd, process_group)?;
        Ok(Some(Self {
            fd,
            original_process_group,
            restored: false,
        }))
    }

    fn restore(&mut self) -> Result<()> {
        if !self.restored {
            set_foreground_process_group(self.fd, self.original_process_group)?;
            self.restored = true;
        }
        Ok(())
    }
}

impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("rtr: could not restore foreground terminal: {error:#}");
        }
    }
}

impl ChildSignals {
    fn new() -> Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("installing SIGINT handler")?,
            terminate: signal(SignalKind::terminate()).context("installing SIGTERM handler")?,
            hangup: signal(SignalKind::hangup()).context("installing SIGHUP handler")?,
            quit: signal(SignalKind::quit()).context("installing SIGQUIT handler")?,
        })
    }

    async fn recv(&mut self) -> i32 {
        tokio::select! {
            Some(()) = self.interrupt.recv() => libc::SIGINT,
            Some(()) = self.terminate.recv() => libc::SIGTERM,
            Some(()) = self.hangup.recv() => libc::SIGHUP,
            Some(()) = self.quit.recv() => libc::SIGQUIT,
        }
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// Prepare the selected profile's launch policy, environment, and arguments.
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
    let (child_env, child_env_remove) = if profile.bypass {
        (Vec::new(), native_home_env_keys(spec))
    } else {
        (
            prepare_native_profile_env(paths, spec, tool, profile_name)?,
            Vec::new(),
        )
    };
    Ok(PreparedSubscriptionRun {
        profile_name: profile_name.to_string(),
        child_args: runtime_args.to_vec(),
        child_env,
        child_env_remove,
        child_cwd: None,
        bypass: profile.bypass,
    })
}

fn native_home_env_keys(spec: &tool_specs::ToolSpec) -> Vec<String> {
    let mut keys = vec![spec.native_home_env.to_string()];
    if let Some(key) = spec.native_secure_storage_env {
        keys.push(key.to_string());
    }
    keys
}

fn prepare_native_profile_env(
    paths: &Paths,
    spec: &tool_specs::ToolSpec,
    tool: &Tool,
    profile_name: &str,
) -> Result<Vec<(String, std::ffi::OsString)>> {
    let home = paths.ensure_profile_home_dir(spec.name, profile_name)?;
    let user_home = crate::home_dir()?;
    sync_profile_startup(spec, tool, &home, &paths.config_dir, &user_home)?;
    let mut env = vec![(
        spec.native_home_env.to_string(),
        home.clone().into_os_string(),
    )];
    if let Some(key) = spec.native_secure_storage_env {
        env.push((key.to_string(), home.into_os_string()));
    }
    Ok(env)
}

/// Refresh the selected native home using either explicit copy mappings or the
/// backwards-compatible skills synchronization policy.
fn sync_profile_startup(
    spec: &tool_specs::ToolSpec,
    tool: &Tool,
    profile_home: &Path,
    config_dir: &Path,
    home: &Path,
) -> Result<()> {
    crate::file_lock::with_exclusive_lock(&profile_home.join(".skills-sync.lock"), || {
        if let Some(mappings) = &tool.copy {
            sync_copy_mappings_locked(spec, mappings, profile_home, config_dir, home)
        } else {
            sync_profile_skills_locked(spec, tool, profile_home, config_dir, home)
        }
    })
}

fn sync_copy_mappings_locked(
    spec: &tool_specs::ToolSpec,
    mappings: &[CopyMapping],
    profile_home: &Path,
    config_dir: &Path,
    home: &Path,
) -> Result<()> {
    let resolved = resolve_copy_mappings(mappings, profile_home, config_dir, home)?;
    let mut staged = Vec::with_capacity(resolved.len());

    for (index, mapping) in resolved.iter().enumerate() {
        match stage_copy_path(
            &mapping.source,
            &mapping.destination,
            spec.rebase_external_skill_symlinks,
        ) {
            Ok(path) => staged.push((path, mapping.destination.clone())),
            Err(error) => {
                for (path, _) in &staged {
                    let _ = remove_existing_path(path);
                }
                return Err(error).with_context(|| {
                    format!(
                        "staging tools.{}.copy[{index}] from {} to {}",
                        spec.name,
                        mapping.source.display(),
                        mapping.destination.display()
                    )
                });
            }
        }
    }

    if let Err(error) = install_staged_paths(&staged) {
        for (remaining, _) in &staged {
            let _ = remove_existing_path(remaining);
        }
        return Err(error).with_context(|| format!("installing tools.{}.copy mappings", spec.name));
    }
    Ok(())
}

fn resolve_copy_mappings(
    mappings: &[CopyMapping],
    profile_home: &Path,
    config_dir: &Path,
    home: &Path,
) -> Result<Vec<ResolvedCopyMapping>> {
    let profile_home_resolved = profile_home
        .canonicalize()
        .with_context(|| format!("canonicalizing profile home {}", profile_home.display()))?;
    let mut resolved = Vec::with_capacity(mappings.len());

    for (index, mapping) in mappings.iter().enumerate() {
        let source = resolve_copy_source(&mapping.source, config_dir, home)
            .with_context(|| format!("resolving copy[{index}].source"))?;
        let metadata = std::fs::metadata(&source).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("configured copy source {} does not exist", source.display())
            } else {
                anyhow::Error::new(error).context(format!("stat copy source {}", source.display()))
            }
        })?;
        if !metadata.is_dir() && !metadata.is_file() {
            bail!("unsupported copy source type: {}", source.display());
        }
        let source_resolved = source
            .canonicalize()
            .with_context(|| format!("canonicalizing copy source {}", source.display()))?;

        let destination = resolve_copy_destination(&mapping.destination, profile_home)
            .with_context(|| format!("resolving copy[{index}].destination"))?;
        let destination_resolved = resolve_destination_entry(&destination)?;
        if !destination_resolved.starts_with(&profile_home_resolved) {
            bail!(
                "copy[{index}] destination {} escapes profile home {}",
                destination.display(),
                profile_home.display()
            );
        }

        resolved.push(ResolvedCopyMapping {
            source,
            destination,
            source_resolved,
            destination_resolved,
        });
    }

    for (index, mapping) in resolved.iter().enumerate() {
        for (other_index, other) in resolved.iter().enumerate() {
            if paths_overlap(&mapping.source_resolved, &other.destination_resolved) {
                bail!(
                    "copy[{index}] source {} overlaps copy[{other_index}] destination {}",
                    mapping.source.display(),
                    other.destination.display()
                );
            }
            if index < other_index
                && paths_overlap(&mapping.destination_resolved, &other.destination_resolved)
            {
                bail!(
                    "copy[{index}] destination {} overlaps copy[{other_index}] destination {}",
                    mapping.destination.display(),
                    other.destination.display()
                );
            }
        }
    }
    Ok(resolved)
}

fn resolve_copy_source(path: &Path, config_dir: &Path, home: &Path) -> Result<PathBuf> {
    let path = expand_home_path(path, home)?;
    Ok(if path.is_relative() {
        config_dir.join(path)
    } else {
        path
    })
}

fn resolve_copy_destination(path: &Path, profile_home: &Path) -> Result<PathBuf> {
    let raw = path.as_os_str().to_string_lossy();
    let relative = if raw == "~" {
        PathBuf::new()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        PathBuf::from(rest)
    } else if raw.starts_with('~') {
        bail!("copy destinations support only '~' or '~/' profile-home expansion: {raw}");
    } else {
        path.to_path_buf()
    };

    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(segment) => normalized.push(segment),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                bail!("copy destination must not contain '..': {}", path.display())
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                bail!(
                    "copy destination must be relative to the profile home: {}",
                    path.display()
                )
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        bail!("copy destination must not be the profile home itself");
    }
    if normalized.starts_with(".skills-sync.lock") {
        bail!("copy destination must not replace rtr's synchronization lock");
    }
    Ok(profile_home.join(normalized))
}

fn canonicalize_with_missing(path: &Path) -> Result<PathBuf> {
    let mut current = path;
    let mut missing = Vec::new();
    loop {
        match current.canonicalize() {
            Ok(mut canonical) => {
                for segment in missing.iter().rev() {
                    canonical.push(segment);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let segment = current
                    .file_name()
                    .with_context(|| format!("{} has no existing ancestor", path.display()))?;
                missing.push(segment.to_os_string());
                current = current
                    .parent()
                    .with_context(|| format!("{} has no parent", current.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("canonicalizing destination {}", path.display()));
            }
        }
    }
}

fn resolve_destination_entry(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent", path.display()))?;
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    Ok(canonicalize_with_missing(parent)?.join(name))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
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
            if spec.name == "codex" {
                replace_codex_user_skills(None, &destination, spec.rebase_external_skill_symlinks)?;
            } else {
                remove_existing_path(&destination)?;
            }
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

    if spec.name == "codex" && codex_inherits_skills_source(&source.path, home)? {
        return replace_codex_user_skills(None, &destination, spec.rebase_external_skill_symlinks);
    }

    ensure_distinct_copy_paths(&source.path, &destination)?;
    if spec.name == "codex" {
        replace_codex_user_skills(
            Some(&source.path),
            &destination,
            spec.rebase_external_skill_symlinks,
        )
    } else {
        replace_skills_dir(
            &source.path,
            &destination,
            spec.rebase_external_skill_symlinks,
        )
    }
}

fn codex_inherits_skills_source(source: &Path, home: &Path) -> Result<bool> {
    let inherited = home.join(".agents/skills");
    if lexical_normalize(source).starts_with(lexical_normalize(&inherited)) {
        return Ok(true);
    }
    let inherited = match inherited.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("canonicalizing {}", inherited.display()));
        }
    };
    let source = source
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", source.display()))?;
    Ok(source.starts_with(inherited))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() && !path.is_absolute() {
                    normalized.push("..");
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
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

fn stage_copy_path(
    source: &Path,
    destination: &Path,
    rebase_external_symlinks: bool,
) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .with_context(|| format!("{} has no parent", destination.display()))?;
    crate::paths::create_private_dir_all(parent)?;
    let staged = temporary_copy_path(destination, "tmp");
    remove_existing_path(&staged)?;

    let result = (|| -> Result<()> {
        let metadata = std::fs::symlink_metadata(source)
            .with_context(|| format!("stat copy source {}", source.display()))?;
        if metadata.file_type().is_symlink() {
            copy_root_symlink(source, &staged, rebase_external_symlinks)?;
        } else if metadata.is_dir() {
            crate::paths::create_private_dir_all(&staged)?;
            copy_dir_contents(source, &staged, rebase_external_symlinks, false)?;
        } else if metadata.is_file() {
            std::fs::copy(source, &staged)
                .with_context(|| format!("copying {} to {}", source.display(), staged.display()))?;
        } else {
            bail!("unsupported copy source type: {}", source.display());
        }
        Ok(())
    })();

    if let Err(error) = result {
        let _ = remove_existing_path(&staged);
        return Err(error);
    }
    Ok(staged)
}

fn copy_root_symlink(source: &Path, destination: &Path, rebase: bool) -> Result<()> {
    let target = std::fs::read_link(source)
        .with_context(|| format!("readlink copy source {}", source.display()))?;
    let target = if target.is_absolute() || !rebase {
        target
    } else {
        let target_path = source
            .parent()
            .with_context(|| format!("{} has no parent", source.display()))?
            .join(&target);
        match target_path.canonicalize() {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => target,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("resolving copy symlink target for {}", source.display())
                });
            }
        }
    };
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, destination)
        .with_context(|| format!("symlink {} -> {}", destination.display(), target.display()))?;
    #[cfg(not(unix))]
    bail!(
        "copying symlinks is not supported on this platform: {}",
        source.display()
    );
    Ok(())
}

fn temporary_copy_path(destination: &Path, kind: &str) -> PathBuf {
    let stamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "copy".into());
    destination.with_file_name(format!(".{name}.rtr-{}-{stamp}.{kind}", std::process::id()))
}

fn replace_skills_dir(
    source: &Path,
    destination: &Path,
    rebase_external_symlinks: bool,
) -> Result<()> {
    let temp = temporary_skills_path(destination);
    remove_existing_path(&temp)?;

    let result = (|| -> Result<()> {
        crate::paths::create_private_dir_all(&temp)?;
        copy_dir_contents(source, &temp, rebase_external_symlinks, false)?;
        install_staged_path(&temp, destination)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = remove_existing_path(&temp);
    }
    result
}

fn install_staged_path(staged: &Path, destination: &Path) -> Result<()> {
    install_staged_paths(&[(staged.to_path_buf(), destination.to_path_buf())])
}

fn install_staged_paths(staged: &[(PathBuf, PathBuf)]) -> Result<()> {
    let mut installed = Vec::with_capacity(staged.len());
    for (staged_path, destination) in staged {
        let copy = match backup_copy_destination(destination) {
            Ok(copy) => copy,
            Err(error) => return Err(error_with_rollback(error, &installed)),
        };
        installed.push(copy);

        if let Err(install_error) = std::fs::rename(staged_path, destination) {
            let install_error = anyhow::Error::new(install_error).context(format!(
                "installing staged copy {} at {}",
                staged_path.display(),
                destination.display()
            ));
            return Err(error_with_rollback(install_error, &installed));
        }
    }

    let mut cleanup_failures = Vec::new();
    for copy in &installed {
        if copy.had_destination {
            if let Err(error) = remove_existing_path(&copy.backup) {
                cleanup_failures.push(format!(
                    "removing backup {} for {}: {error:#}",
                    copy.backup.display(),
                    copy.destination.display()
                ));
            }
        }
    }
    if cleanup_failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", cleanup_failures.join("; "))
    }
}

fn backup_copy_destination(destination: &Path) -> Result<InstalledCopy> {
    let backup = temporary_copy_path(destination, "backup");
    remove_existing_path(&backup)?;
    let had_destination = match std::fs::symlink_metadata(destination) {
        Ok(_) => {
            std::fs::rename(destination, &backup).with_context(|| {
                format!(
                    "backing up {} to {}",
                    destination.display(),
                    backup.display()
                )
            })?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("stat {}", destination.display()));
        }
    };
    Ok(InstalledCopy {
        destination: destination.to_path_buf(),
        backup,
        had_destination,
    })
}

fn error_with_rollback(error: anyhow::Error, installed: &[InstalledCopy]) -> anyhow::Error {
    match rollback_installed_copies(installed) {
        Ok(()) => error,
        Err(rollback_error) => {
            anyhow::anyhow!("{error:#}; rollback failed: {rollback_error:#}")
        }
    }
}

fn rollback_installed_copies(installed: &[InstalledCopy]) -> Result<()> {
    let mut failures = Vec::new();
    for copy in installed.iter().rev() {
        if let Err(error) = remove_existing_path(&copy.destination) {
            failures.push(format!(
                "removing new destination {}: {error:#}",
                copy.destination.display()
            ));
        }
        if copy.had_destination {
            if let Err(error) = std::fs::rename(&copy.backup, &copy.destination) {
                failures.push(format!(
                    "restoring {} from {}: {error}",
                    copy.destination.display(),
                    copy.backup.display()
                ));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", failures.join("; "))
    }
}

fn replace_codex_user_skills(
    source: Option<&Path>,
    destination: &Path,
    rebase_external_symlinks: bool,
) -> Result<()> {
    let system = match std::fs::symlink_metadata(destination) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            let system = destination.join(".system");
            std::fs::symlink_metadata(&system).ok().map(|_| system)
        }
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("stat {}", destination.display()));
        }
    };

    if source.is_none() && system.is_none() {
        return remove_existing_path(destination);
    }

    let temp = temporary_skills_path(destination);
    remove_existing_path(&temp)?;
    let result = (|| -> Result<()> {
        crate::paths::create_private_dir_all(&temp)?;
        if let Some(source) = source {
            copy_dir_contents(source, &temp, rebase_external_symlinks, true)?;
        }
        if let Some(system) = system {
            copy_path(&system, &temp.join(".system"), &system, false)?;
        }
        install_staged_path(&temp, destination)?;
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

fn copy_dir_contents(
    source: &Path,
    destination: &Path,
    rebase_external_symlinks: bool,
    skip_codex_system: bool,
) -> Result<()> {
    let source_root = source
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", source.display()))?;
    copy_dir_contents_from(
        source,
        destination,
        &source_root,
        rebase_external_symlinks,
        skip_codex_system,
    )
}

fn copy_dir_contents_from(
    source: &Path,
    destination: &Path,
    source_root: &Path,
    rebase_external_symlinks: bool,
    skip_codex_system: bool,
) -> Result<()> {
    for entry in
        std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("reading {}", source.display()))?;
        if skip_codex_system
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(".system"))
        {
            continue;
        }
        copy_path(
            &entry.path(),
            &destination.join(entry.file_name()),
            source_root,
            rebase_external_symlinks,
        )?;
    }
    Ok(())
}

fn copy_path(
    source: &Path,
    destination: &Path,
    source_root: &Path,
    rebase_external_symlinks: bool,
) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(source).with_context(|| format!("stat {}", source.display()))?;
    if meta.file_type().is_symlink() {
        let target =
            std::fs::read_link(source).with_context(|| format!("readlink {}", source.display()))?;
        let target = copied_symlink_target(source, source_root, target, rebase_external_symlinks)?;
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
        return copy_dir_contents_from(
            source,
            destination,
            source_root,
            rebase_external_symlinks,
            false,
        );
    }
    if meta.is_file() {
        std::fs::copy(source, destination).with_context(|| {
            format!("copying {} to {}", source.display(), destination.display())
        })?;
        return Ok(());
    }
    bail!("unsupported skills entry type: {}", source.display());
}

fn copied_symlink_target(
    source: &Path,
    source_root: &Path,
    target: PathBuf,
    rebase_external_symlinks: bool,
) -> Result<PathBuf> {
    if target.is_absolute() || !rebase_external_symlinks {
        return Ok(target);
    }
    let target_path = source
        .parent()
        .with_context(|| format!("{} has no parent", source.display()))?
        .join(&target);
    let resolved = match target_path.canonicalize() {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(target),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("resolving symlink target for {}", source.display()));
        }
    };
    if resolved.starts_with(source_root) {
        Ok(target)
    } else {
        std::path::absolute(&target_path)
            .with_context(|| format!("absolutizing symlink target for {}", source.display()))
    }
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

    execute_prepared_subscription_run(paths, spec, &tool, prepared).await
}

/// Launch one exact profile in its isolated home without selection policy.
///
/// Conversation identity includes the profile that owns the native session.
/// Consequently resume/fork must bypass rotation and persisted `enabled` /
/// `bypass` policy: honoring either would launch the right ID in the wrong
/// home, or make archived sessions impossible to recover.
pub async fn run_isolated_profile_tool(
    paths: &Paths,
    tool_name: &str,
    profile_name: &str,
    runtime_args: &[String],
    working_directory: Option<&Path>,
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
    if !tool.profiles.contains_key(profile_name) {
        bail!("profile '{}/{}' does not exist", spec.name, profile_name);
    }

    let child_cwd = match working_directory {
        Some(path) if path.is_dir() => Some(path.to_path_buf()),
        Some(path) => {
            eprintln!(
                "rtr: recorded conversation directory is unavailable: {}; using the current directory",
                path.display()
            );
            None
        }
        None => None,
    };
    let prepared = PreparedSubscriptionRun {
        profile_name: profile_name.to_string(),
        child_args: runtime_args.to_vec(),
        child_env: prepare_native_profile_env(paths, spec, &tool, profile_name)?,
        child_env_remove: Vec::new(),
        child_cwd,
        bypass: false,
    };
    execute_prepared_subscription_run(paths, spec, &tool, prepared).await
}

async fn execute_prepared_subscription_run(
    paths: &Paths,
    spec: &tool_specs::ToolSpec,
    tool: &Tool,
    prepared: PreparedSubscriptionRun,
) -> Result<i32> {
    if prepared.bypass {
        eprintln!("{}", render_bypass_banner(spec, &prepared.profile_name));
    }
    let result = execute_tool(
        tool,
        prepared.child_args,
        prepared.child_env,
        prepared.child_env_remove,
        prepared.child_cwd,
    )
    .await;
    let child_exit_code = result.as_ref().ok().copied();
    if let Err(error) = usage::append_event(
        &paths.usage_file(),
        &usage::new_event(
            spec.name,
            &prepared.profile_name,
            child_exit_code,
            prepared.bypass,
        ),
    ) {
        eprintln!("rtr: could not record usage: {error:#}");
    }
    if let Some(exit_code) = child_exit_code {
        eprintln!(
            "{}",
            render_exit_summary(
                spec,
                &prepared.profile_name,
                exit_code,
                stderr_supports_color(),
            )
        );
    }
    result
}

fn render_bypass_banner(spec: &tool_specs::ToolSpec, profile_name: &str) -> String {
    let target = format!("{}/{}", spec.name, profile_name);
    format!(
        "rtr: bypass {target} — launching {} with its default home (no {}; undo: rtr unbypass {} --profile {})",
        spec.name,
        spec.native_home_env,
        spec.name,
        shell_quote(profile_name)
    )
}

/// Create a native-home profile and launch its tool for the initial sign-in.
pub async fn add_subscription_profile(
    paths: &Paths,
    tool_name: &str,
    profile_name: &str,
) -> Result<i32> {
    let spec = tool_specs::get(tool_name)?;
    let config_path = paths.config_file();
    if !config_path.exists() {
        bail!(
            "no config at {} — run `rtr init` first",
            config_path.display()
        );
    }
    persist_new_subscription_profile(&config_path, spec, profile_name)?;
    println!("Added profile: {}/{}", spec.name, profile_name);
    println!(
        "Native home: {}={}",
        spec.native_home_env,
        paths.profile_home_dir(spec.name, profile_name).display()
    );
    println!("Launching {} to sign in...", spec.name);

    let exit_code = run_subscription_tool(paths, spec.name, Some(profile_name), &[]).await?;
    println!();
    println!(
        "Run this profile again with: rtr {} --profile {}",
        spec.name,
        shell_quote(profile_name)
    );
    Ok(exit_code)
}

/// Clear a selected profile's stale credential lock and relaunch it in place.
pub async fn fix_subscription_profile(
    paths: &Paths,
    tool_name: &str,
    profile_name: &str,
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
    let profile = tool.profiles.get(profile_name).with_context(|| {
        format!(
            "profile {}/{} does not exist; create it with `rtr add {} --profile {}`",
            spec.name,
            profile_name,
            spec.name,
            shell_quote(profile_name)
        )
    })?;
    let bypass = profile.bypass;

    let profile_home = paths.ensure_profile_home_dir(spec.name, profile_name)?;
    for lock in remove_stale_credential_locks(spec, &profile_home)? {
        println!("Removed stale credential lock: {}", lock.display());
    }
    println!("Repairing profile: {}/{}", spec.name, profile_name);
    if bypass {
        println!("{}", render_fix_bypass_note(spec, profile_name));
    }
    println!(
        "Native home: {}={}",
        spec.native_home_env,
        profile_home.display()
    );
    println!("Launching {} to re-authenticate...", spec.name);

    let prepared = PreparedSubscriptionRun {
        profile_name: profile_name.to_string(),
        child_args: Vec::new(),
        child_env: prepare_native_profile_env(paths, spec, &tool, profile_name)?,
        child_env_remove: Vec::new(),
        child_cwd: None,
        bypass: false,
    };
    execute_prepared_subscription_run(paths, spec, &tool, prepared).await
}

fn render_fix_bypass_note(spec: &tool_specs::ToolSpec, profile_name: &str) -> String {
    format!(
        "note: {}/{} is bypassed; rtr fix still uses its isolated native home",
        spec.name, profile_name
    )
}

fn remove_stale_credential_locks(
    spec: &tool_specs::ToolSpec,
    profile_home: &Path,
) -> Result<Vec<PathBuf>> {
    let lock_names: &[&str] = match spec.name {
        "codex" => &["auth.json.lock"],
        _ => &[],
    };
    let mut removed = Vec::new();
    for name in lock_names {
        let path = profile_home.join(name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("stat {}", path.display()));
            }
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            bail!("credential lock {} must not be a directory", path.display());
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("removing stale credential lock {}", path.display()))?;
        removed.push(path);
    }
    Ok(removed)
}

fn persist_new_subscription_profile(
    config_path: &Path,
    spec: &tool_specs::ToolSpec,
    profile_name: &str,
) -> Result<()> {
    crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(config_path), || {
        let mut config = Config::load(config_path)?;
        let tool = config.tool(spec.name)?;
        if tool.command.is_empty() {
            bail!("tool '{}' has an empty command", spec.name);
        }
        if tool.profiles.contains_key(profile_name) {
            bail!(
                "profile {}/{} already exists; run `rtr {} --profile {}`",
                spec.name,
                profile_name,
                spec.name,
                shell_quote(profile_name)
            );
        }
        config::ensure_profile_entry_in_file(config_path, &mut config, spec.name, profile_name)?;
        Ok(())
    })
}

pub(crate) fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'@')
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Render the post-exit profile reminder and its profile-bound resume command.
fn render_exit_summary(
    spec: &tool_specs::ToolSpec,
    profile_name: &str,
    exit_code: i32,
    color: bool,
) -> String {
    const RESET: &str = "\x1b[0m";
    const DIM: &str = "\x1b[2m";
    const BOLD_CYAN: &str = "\x1b[1;36m";
    const RED: &str = "\x1b[31m";

    let label = if color {
        format!("{DIM}rtr:{RESET}")
    } else {
        "rtr:".to_string()
    };
    let profile = if color {
        format!("{BOLD_CYAN}{profile_name}{RESET}")
    } else {
        profile_name.to_string()
    };
    let mut summary = format!(
        "{label} {} ran in profile '{profile}' — resume: rtr {} -p {} {}",
        spec.name,
        spec.name,
        shell_quote(profile_name),
        spec.resume_args.join(" ")
    );
    if exit_code != 0 {
        if color {
            summary.push_str(&format!("{RED} (exit {exit_code}){RESET}"));
        } else {
            summary.push_str(&format!(" (exit {exit_code})"));
        }
    }
    summary
}

/// Decide whether the exit summary may contain ANSI color on stderr.
fn stderr_supports_color() -> bool {
    let color_enabled = std::env::var_os("NO_COLOR")
        .map(|value| value.is_empty())
        .unwrap_or(true);
    // SAFETY: `isatty` only inspects the process's stderr file descriptor.
    color_enabled && unsafe { libc::isatty(libc::STDERR_FILENO) } == 1
}

/// Run a configured tool with inherited stdio and wrapper-safe signal handling.
async fn execute_tool(
    tool: &Tool,
    child_args: Vec<String>,
    child_env: Vec<(String, std::ffi::OsString)>,
    child_env_remove: Vec<String>,
    child_cwd: Option<PathBuf>,
) -> Result<i32> {
    let mut signals = ChildSignals::new()?;
    let mut command = build_tool_command(tool, child_args, child_env, child_env_remove, child_cwd);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning '{}'", tool.command[0]))?;
    let child_pid = child.id().context("spawned child has no process id")? as i32;
    let mut foreground_terminal = ForegroundTerminal::handoff(child_pid)?;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.context("waiting for child")?,
            forwarded = signals.recv() => {
                if let Err(error) = forward_signal(child_pid, forwarded) {
                    eprintln!("rtr: could not forward signal {forwarded} to child: {error}");
                }
            }
        }
    };
    if let Some(terminal) = &mut foreground_terminal {
        if let Err(error) = terminal.restore() {
            eprintln!("rtr: could not restore foreground terminal: {error:#}");
        }
    }
    Ok(exit_code(status))
}

/// Build a child command with rtr-owned environment changes applied.
fn build_tool_command(
    tool: &Tool,
    child_args: Vec<String>,
    child_env: Vec<(String, std::ffi::OsString)>,
    child_env_remove: Vec<String>,
    child_cwd: Option<PathBuf>,
) -> Command {
    let mut command = Command::new(&tool.command[0]);
    command.args(&tool.command[1..]).args(child_args);
    for key in child_env_remove {
        command.env_remove(key);
    }
    for (key, value) in child_env {
        command.env(key, value);
    }
    if let Some(cwd) = child_cwd {
        command.current_dir(cwd);
    }
    command.process_group(0);
    command.kill_on_drop(true);
    command
}

/// Forward a signal received by rtr to the child's process group.
fn forward_signal(pid: i32, signal: i32) -> Result<()> {
    // SAFETY: `kill` reads only the integer pid and signal values supplied here.
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("forwarding signal")
}

fn set_foreground_process_group(fd: i32, process_group: i32) -> Result<()> {
    // SAFETY: the signal-set pointers are valid for each call, and `tcsetpgrp`
    // receives the controlling-terminal fd and an existing process-group id.
    unsafe {
        let mut blocked = std::mem::zeroed::<libc::sigset_t>();
        let mut previous = std::mem::zeroed::<libc::sigset_t>();
        if libc::sigemptyset(&mut blocked) != 0 || libc::sigaddset(&mut blocked, libc::SIGTTOU) != 0
        {
            return Err(std::io::Error::last_os_error()).context("blocking SIGTTOU");
        }
        let block_result = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
        if block_result != 0 {
            return Err(std::io::Error::from_raw_os_error(block_result))
                .context("blocking SIGTTOU");
        }
        let result = libc::tcsetpgrp(fd, process_group);
        let terminal_error = std::io::Error::last_os_error();
        let restore_result =
            libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if restore_result != 0 {
            return Err(std::io::Error::from_raw_os_error(restore_result))
                .context("restoring signal mask");
        }
        if result != 0 {
            return Err(terminal_error).context("setting foreground process group");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
    fn shell_quote_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("plain/path"), "plain/path");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("can't"), "'can'\\''t'");
        assert_eq!(shell_quote(""), "''");
    }

    fn test_paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
        }
    }

    #[test]
    fn bypassed_prepare_skips_profile_home_and_removes_codex_home() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let missing_skills = dir.path().join("must-not-be-read");
        let config = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n[tools.codex.profiles.personal]\nbypass=true\n",
            toml_path(&missing_skills)
        ))
        .unwrap();
        let tool = config.tool("codex").unwrap();

        let prepared = prepare_subscription_run(
            &paths,
            tool_specs::get("codex").unwrap(),
            tool,
            "personal",
            &["exec".to_string()],
        )
        .unwrap();

        assert!(prepared.bypass);
        assert!(prepared.child_env.is_empty());
        assert_eq!(prepared.child_env_remove, vec!["CODEX_HOME"]);
        assert_eq!(prepared.child_args, vec!["exec"]);
        assert!(!paths.profile_home_dir("codex", "personal").exists());
        assert!(!paths.state_dir.exists());
    }

    #[test]
    fn bypassed_claude_prepare_removes_both_native_home_variables() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let config = Config::parse(
            "[tools.claude]\ncommand=[\"claude\"]\n[tools.claude.profiles.work]\nbypass=true\n",
        )
        .unwrap();
        let prepared = prepare_subscription_run(
            &paths,
            tool_specs::get("claude").unwrap(),
            config.tool("claude").unwrap(),
            "work",
            &[],
        )
        .unwrap();

        assert_eq!(
            prepared.child_env_remove,
            vec!["CLAUDE_CONFIG_DIR", "CLAUDE_SECURESTORAGE_CONFIG_DIR"]
        );
        assert!(!paths.profile_home_dir("claude", "work").exists());
    }

    #[test]
    fn normal_prepare_still_creates_and_assigns_the_profile_home() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let skills = dir.path().join("skills");
        std::fs::create_dir(&skills).unwrap();
        let config = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n[tools.codex.profiles.personal]\n",
            toml_path(&skills)
        ))
        .unwrap();
        let prepared = prepare_subscription_run(
            &paths,
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            "personal",
            &[],
        )
        .unwrap();
        let profile_home = paths.profile_home_dir("codex", "personal");

        assert!(!prepared.bypass);
        assert!(prepared.child_env_remove.is_empty());
        assert_eq!(
            prepared.child_env,
            vec![(
                "CODEX_HOME".to_string(),
                profile_home.clone().into_os_string()
            )]
        );
        assert!(profile_home.is_dir());
    }

    #[test]
    fn disabled_bypassed_profile_is_rejected_without_home_creation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let config = Config::parse(
            "[tools.codex]\ncommand=[\"codex\"]\n[tools.codex.profiles.personal]\nenabled=false\nbypass=true\n",
        )
        .unwrap();

        let error = prepare_subscription_run(
            &paths,
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            "personal",
            &[],
        )
        .unwrap_err()
        .to_string();

        assert!(
            error.contains("profile 'codex/personal' is disabled"),
            "{error}"
        );
        assert!(!paths.state_dir.exists());
    }

    #[test]
    fn child_command_applies_native_home_removals() {
        let config = Config::parse("[tools.codex]\ncommand=[\"codex\"]\n").unwrap();
        let command = build_tool_command(
            config.tool("codex").unwrap(),
            Vec::new(),
            Vec::new(),
            vec!["CODEX_HOME".to_string()],
            None,
        );
        let removed = command
            .as_std()
            .get_envs()
            .any(|(key, value)| key == std::ffi::OsStr::new("CODEX_HOME") && value.is_none());
        assert!(removed);
    }

    #[test]
    fn bypass_banner_is_exact_and_shell_quotes_the_profile() {
        assert_eq!(
            render_bypass_banner(tool_specs::get("codex").unwrap(), "personal"),
            "rtr: bypass codex/personal — launching codex with its default home (no CODEX_HOME; undo: rtr unbypass codex --profile personal)"
        );
        assert!(
            render_bypass_banner(tool_specs::get("claude").unwrap(), "work team")
                .contains("undo: rtr unbypass claude --profile 'work team'")
        );
    }

    #[test]
    fn fix_note_explains_that_repair_uses_isolation() {
        assert_eq!(
            render_fix_bypass_note(tool_specs::get("codex").unwrap(), "personal"),
            "note: codex/personal is bypassed; rtr fix still uses its isolated native home"
        );
    }

    #[test]
    fn render_exit_summary_plain_codex_resume_has_no_ansi() {
        let summary = render_exit_summary(tool_specs::get("codex").unwrap(), "eng", 0, false);

        assert_eq!(
            summary,
            "rtr: codex ran in profile 'eng' — resume: rtr codex -p eng resume"
        );
        assert!(!summary.contains("\x1b"));
    }

    #[test]
    fn render_exit_summary_quotes_profile_and_uses_claude_resume_flag() {
        assert_eq!(
            render_exit_summary(tool_specs::get("claude").unwrap(), "work team", 0, false,),
            "rtr: claude ran in profile 'work team' — resume: rtr claude -p 'work team' --resume"
        );
    }

    #[test]
    fn render_exit_summary_colors_profile_and_resets() {
        assert_eq!(
            render_exit_summary(tool_specs::get("codex").unwrap(), "eng", 0, true),
            "\x1b[2mrtr:\x1b[0m codex ran in profile '\x1b[1;36meng\x1b[0m' — resume: rtr codex -p eng resume"
        );
    }

    #[test]
    fn render_exit_summary_marks_non_zero_exit() {
        assert_eq!(
            render_exit_summary(tool_specs::get("claude").unwrap(), "work", 7, false),
            "rtr: claude ran in profile 'work' — resume: rtr claude -p work --resume (exit 7)"
        );
        assert_eq!(
            render_exit_summary(tool_specs::get("claude").unwrap(), "work", 7, true),
            "\x1b[2mrtr:\x1b[0m claude ran in profile '\x1b[1;36mwork\x1b[0m' — resume: rtr claude -p work --resume\x1b[31m (exit 7)\x1b[0m"
        );
    }

    #[test]
    fn concurrent_profile_creators_persist_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[tools.codex]\ncommand = [\"codex\"]\n").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let config_path = config_path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    persist_new_subscription_profile(
                        &config_path,
                        tool_specs::get("codex").unwrap(),
                        "personal",
                    )
                    .map_err(|error| error.to_string())
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .filter(|error| error.contains("already exists"))
                .count(),
            1
        );
        let config = Config::load(&config_path).unwrap();
        assert_eq!(
            config
                .tool("codex")
                .unwrap()
                .profiles
                .keys()
                .collect::<Vec<_>>(),
            vec!["personal"]
        );
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
    fn configured_copy_refreshes_directories_and_files_from_documented_bases() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        let home = dir.path().join("home");
        let profile_home = dir.path().join("profile");
        let skills = config_dir.join("shared-skills");
        let instructions = home.join(".claude/CLAUDE.md");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::create_dir_all(instructions.parent().unwrap()).unwrap();
        std::fs::create_dir_all(profile_home.join("skills")).unwrap();
        std::fs::write(skills.join("new.md"), "one").unwrap();
        std::fs::write(&instructions, "first").unwrap();
        std::fs::write(profile_home.join("skills/stale.md"), "stale").unwrap();

        let config = Config::parse(
            r#"
[tools.claude]
command = ["claude"]
copy = [
  { source = "shared-skills", destination = "skills" },
  { source = "~/.claude/CLAUDE.md", destination = "~/CLAUDE.md" },
]
"#,
        )
        .unwrap();
        let tool = config.tool("claude").unwrap();

        sync_profile_startup(
            tool_specs::get("claude").unwrap(),
            tool,
            &profile_home,
            &config_dir,
            &home,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(profile_home.join("skills/new.md")).unwrap(),
            "one"
        );
        assert!(!profile_home.join("skills/stale.md").exists());
        assert_eq!(
            std::fs::read_to_string(profile_home.join("CLAUDE.md")).unwrap(),
            "first"
        );

        std::fs::write(skills.join("new.md"), "two").unwrap();
        std::fs::write(&instructions, "second").unwrap();
        sync_profile_startup(
            tool_specs::get("claude").unwrap(),
            tool,
            &profile_home,
            &config_dir,
            &home,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(profile_home.join("skills/new.md")).unwrap(),
            "two"
        );
        assert_eq!(
            std::fs::read_to_string(profile_home.join("CLAUDE.md")).unwrap(),
            "second"
        );
    }

    #[test]
    fn configured_empty_copy_list_disables_implicit_skills_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(profile_home.join("skills")).unwrap();
        std::fs::write(profile_home.join("skills/keep.md"), "keep").unwrap();
        let config = Config::parse("[tools.codex]\ncommand = [\"codex\"]\ncopy = []\n").unwrap();

        sync_profile_startup(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            &dir.path().join("home-without-default-skills"),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(profile_home.join("skills/keep.md")).unwrap(),
            "keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_copy_stages_every_mapping_before_replacing_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let profile_home = dir.path().join("profile");
        let valid_source = dir.path().join("settings.json");
        let invalid_source = dir.path().join("invalid-source");
        std::fs::create_dir_all(&invalid_source).unwrap();
        std::fs::create_dir_all(&profile_home).unwrap();
        std::fs::write(&valid_source, "new").unwrap();
        std::fs::write(profile_home.join("settings.json"), "old").unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(invalid_source.join("unsupported"))
            .status()
            .unwrap()
            .success());
        let config = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\ncopy=[{{source={},destination=\"settings.json\"}},{{source={},destination=\"skills\"}}]\n",
            toml_path(&valid_source),
            toml_path(&invalid_source)
        ))
        .unwrap();

        let error = sync_profile_startup(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            dir.path(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("copy[1]"), "{error}");
        assert_eq!(
            std::fs::read_to_string(profile_home.join("settings.json")).unwrap(),
            "old"
        );
        assert!(!profile_home.join("skills").exists());
    }

    #[test]
    fn configured_copy_rejects_unsafe_or_overlapping_destinations() {
        let profile_home = Path::new("/profiles/codex/work");
        for destination in [Path::new(".."), Path::new("/tmp/outside"), Path::new("~")] {
            assert!(resolve_copy_destination(destination, profile_home).is_err());
        }

        let dir = tempfile::tempdir().unwrap();
        let profile_home = dir.path().join("profile");
        let source = profile_home.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let mappings = [CopyMapping {
            source: source.clone(),
            destination: PathBuf::from("source/copied"),
        }];
        let error = resolve_copy_mappings(&mappings, &profile_home, dir.path(), dir.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("overlaps"), "{error}");

        #[cfg(unix)]
        {
            let symlink_source = profile_home.join("symlink-source");
            let symlink_target = profile_home.join("elsewhere");
            std::fs::create_dir_all(&symlink_source).unwrap();
            std::fs::create_dir_all(&symlink_target).unwrap();
            std::os::unix::fs::symlink(&symlink_target, symlink_source.join("out")).unwrap();
            let mappings = [CopyMapping {
                source: symlink_source,
                destination: PathBuf::from("symlink-source/out"),
            }];
            let error = resolve_copy_mappings(&mappings, &profile_home, dir.path(), dir.path())
                .unwrap_err()
                .to_string();
            assert!(error.contains("overlaps"), "{error}");
        }

        let external = dir.path().join("external");
        std::fs::create_dir_all(&external).unwrap();
        let mappings = [
            CopyMapping {
                source: external.clone(),
                destination: PathBuf::from("skills"),
            },
            CopyMapping {
                source: external,
                destination: PathBuf::from("skills/nested"),
            },
        ];
        let error = resolve_copy_mappings(&mappings, &profile_home, dir.path(), dir.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("destination") && error.contains("overlaps"),
            "{error}"
        );
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
        sync_profile_startup(
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
    fn claude_sync_keeps_external_relative_skill_links_usable() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("home/.claude/skills");
        let skill_library = dir.path().join("skill-library");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(skill_library.join("v1/review")).unwrap();
        std::fs::create_dir_all(skill_library.join("v2/review")).unwrap();
        std::fs::write(skill_library.join("v1/review/SKILL.md"), "version one").unwrap();
        std::fs::write(skill_library.join("v2/review/SKILL.md"), "version two").unwrap();
        std::os::unix::fs::symlink("v1", skill_library.join("current")).unwrap();
        std::os::unix::fs::symlink(
            "../../../skill-library/current/review",
            source.join("review"),
        )
        .unwrap();

        let config = Config::parse(&format!(
            "[tools.claude]\ncommand=[\"claude\"]\nskills_source={}\n",
            toml_path(&source)
        ))
        .unwrap();
        sync_profile_startup(
            tool_specs::get("claude").unwrap(),
            config.tool("claude").unwrap(),
            &profile_home,
            dir.path(),
            &dir.path().join("home"),
        )
        .unwrap();

        let copied_skill = profile_home.join("skills/review");
        assert_eq!(
            std::fs::read_to_string(copied_skill.join("SKILL.md")).unwrap(),
            "version one"
        );
        std::fs::remove_file(skill_library.join("current")).unwrap();
        std::os::unix::fs::symlink("v2", skill_library.join("current")).unwrap();
        assert_eq!(
            std::fs::read_to_string(copied_skill.join("SKILL.md")).unwrap(),
            "version two"
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_sync_rebases_external_relative_skill_links() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("home/.codex/skills");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(dir.path().join("skill-library/review")).unwrap();
        let target = "../../../skill-library/review";
        std::os::unix::fs::symlink(target, source.join("review")).unwrap();

        let config = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n",
            toml_path(&source)
        ))
        .unwrap();
        sync_profile_startup(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            &dir.path().join("home"),
        )
        .unwrap();

        let copied = profile_home.join("skills/review");
        assert!(std::fs::read_link(&copied).unwrap().is_absolute());
        assert!(copied.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn sync_profile_skills_preserves_dangling_relative_links() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(&source).unwrap();
        std::os::unix::fs::symlink("missing-skill", source.join("dangling")).unwrap();

        let config = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n",
            toml_path(&source)
        ))
        .unwrap();
        sync_profile_startup(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            dir.path(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_link(profile_home.join("skills/dangling")).unwrap(),
            PathBuf::from("missing-skill")
        );
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
        assert!(sync_profile_startup(
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

        sync_profile_startup(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            &dir.path().join("home"),
        )
        .unwrap();
        assert!(!profile_home.join("skills").exists());
    }

    #[cfg(unix)]
    #[test]
    fn codex_sync_skips_inherited_skills_and_preserves_system_cache() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let inherited = home.join(".agents/skills");
        let legacy_parent = home.join(".codex");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(inherited.join("shared")).unwrap();
        std::fs::create_dir_all(&legacy_parent).unwrap();
        std::os::unix::fs::symlink(&inherited, legacy_parent.join("skills")).unwrap();
        std::fs::create_dir_all(profile_home.join("skills/shared")).unwrap();
        std::fs::create_dir_all(profile_home.join("skills/.system")).unwrap();
        std::fs::write(profile_home.join("skills/shared/SKILL.md"), "duplicate").unwrap();
        std::fs::write(profile_home.join("skills/.system/marker"), "current").unwrap();

        let config = Config::parse("[tools.codex]\ncommand=[\"codex\"]\n").unwrap();
        sync_profile_startup(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            &home,
        )
        .unwrap();

        assert!(!profile_home.join("skills/shared").exists());
        assert_eq!(
            std::fs::read_to_string(profile_home.join("skills/.system/marker")).unwrap(),
            "current"
        );
    }

    #[test]
    fn codex_sync_copies_legacy_skills_without_identity_or_system_state() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let legacy = home.join(".codex/skills");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(legacy.join("legacy")).unwrap();
        std::fs::create_dir_all(legacy.join(".system")).unwrap();
        std::fs::create_dir_all(profile_home.join("skills/.system")).unwrap();
        std::fs::write(legacy.join("legacy/SKILL.md"), "legacy").unwrap();
        std::fs::write(legacy.join(".system/stale"), "stale").unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"private\"").unwrap();
        std::fs::write(home.join(".codex/auth.json"), "secret").unwrap();
        std::fs::write(profile_home.join("skills/.system/current"), "current").unwrap();

        let config = Config::parse("[tools.codex]\ncommand=[\"codex\"]\n").unwrap();
        sync_profile_startup(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            &home,
        )
        .unwrap();

        assert!(profile_home.join("skills/legacy/SKILL.md").is_file());
        assert!(profile_home.join("skills/.system/current").is_file());
        assert!(!profile_home.join("skills/.system/stale").exists());
        assert!(!profile_home.join("config.toml").exists());
        assert!(!profile_home.join("auth.json").exists());
    }

    #[test]
    fn codex_sync_skips_explicit_source_inside_inherited_skills() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let source = home.join(".agents/skills/shared");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(profile_home.join("skills/stale")).unwrap();
        std::fs::write(source.join("SKILL.md"), "shared").unwrap();

        let config = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n",
            toml_path(&source)
        ))
        .unwrap();
        sync_profile_startup(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            &home,
        )
        .unwrap();
        assert!(!profile_home.join("skills").exists());
    }

    #[cfg(unix)]
    #[test]
    fn codex_sync_skips_symlink_source_inside_inherited_skills() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let inherited = home.join(".agents/skills");
        let target = dir.path().join("external/shared");
        let source = inherited.join("shared");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(&inherited).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(profile_home.join("skills/stale")).unwrap();
        std::fs::write(target.join("SKILL.md"), "shared").unwrap();
        std::os::unix::fs::symlink(&target, &source).unwrap();

        let config = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n",
            toml_path(&source)
        ))
        .unwrap();
        sync_profile_startup(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            &home,
        )
        .unwrap();
        assert!(!profile_home.join("skills").exists());
    }

    #[test]
    fn codex_sync_excludes_case_variant_system_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(source.join(".SYSTEM")).unwrap();
        std::fs::create_dir_all(profile_home.join("skills/.system")).unwrap();
        std::fs::write(source.join(".SYSTEM/injected"), "source").unwrap();
        std::fs::write(profile_home.join("skills/.system/current"), "current").unwrap();

        let config = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n",
            toml_path(&source)
        ))
        .unwrap();
        sync_profile_startup(
            tool_specs::get("codex").unwrap(),
            config.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            Path::new("/home/me"),
        )
        .unwrap();
        assert!(profile_home.join("skills/.system/current").is_file());
        assert!(!profile_home.join("skills/.SYSTEM/injected").exists());
    }

    #[test]
    fn staged_copy_install_restores_destination_when_install_fails() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("skills");
        let missing_staged = dir.path().join("missing-staged");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("usable"), "previous").unwrap();

        let error = install_staged_path(&missing_staged, &destination)
            .unwrap_err()
            .to_string();
        assert!(error.contains("installing staged copy"), "{error}");
        assert_eq!(
            std::fs::read_to_string(destination.join("usable")).unwrap(),
            "previous"
        );
    }

    #[test]
    fn staged_copy_group_rolls_back_earlier_installs_when_a_later_install_fails() {
        let dir = tempfile::tempdir().unwrap();
        let first_destination = dir.path().join("first");
        let second_destination = dir.path().join("second");
        let first_staged = dir.path().join("first-staged");
        let missing_second_staged = dir.path().join("missing-second-staged");
        std::fs::write(&first_destination, "first-old").unwrap();
        std::fs::write(&second_destination, "second-old").unwrap();
        std::fs::write(&first_staged, "first-new").unwrap();

        let error = install_staged_paths(&[
            (first_staged, first_destination.clone()),
            (missing_second_staged, second_destination.clone()),
        ])
        .unwrap_err()
        .to_string();

        assert!(error.contains("installing staged copy"), "{error}");
        assert_eq!(
            std::fs::read_to_string(first_destination).unwrap(),
            "first-old"
        );
        assert_eq!(
            std::fs::read_to_string(second_destination).unwrap(),
            "second-old"
        );
    }
}
