//! Profile inspection commands for the native launcher.

use std::io::{BufRead, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::{Config, Profile};
use crate::paths::Paths;
use crate::tool_specs;

pub fn render_profile(
    tool: &str,
    profile_name: &str,
    profile: &Profile,
    native_home_env: &str,
    native_home: &Path,
) -> String {
    format!(
        "{tool}/{profile_name}\n  enabled: {}\n  native home: {native_home_env}={}\n",
        profile.enabled,
        native_home.display()
    )
}

pub fn render_profile_list(cfg: &Config) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for spec in tool_specs::all() {
        let _ = writeln!(out, "{}", spec.name);
        match cfg.tools.get(spec.name) {
            Some(tool) if tool.profiles.is_empty() => {
                let _ = writeln!(out, "  profiles: (none)");
            }
            Some(tool) => {
                let _ = writeln!(out, "  profiles:");
                for (name, profile) in &tool.profiles {
                    let status = if profile.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    };
                    let _ = writeln!(out, "    {name} ({status})");
                }
            }
            None => {
                let _ = writeln!(out, "  profiles: (not configured)");
            }
        }
    }
    out
}

pub fn run_list_profiles(paths: &Paths) -> Result<()> {
    let cfg = Config::load(&paths.config_file())?;
    print!("{}", render_profile_list(&cfg));
    Ok(())
}

/// Split a `<tool>/<profile>` command target into its two parts.
fn parse_target(target: &str) -> Result<(&str, &str)> {
    target
        .split_once('/')
        .with_context(|| format!("profile target '{target}' must look like <tool>/<profile>"))
}

pub fn run_show_profile(paths: &Paths, target: &str) -> Result<()> {
    let (tool_name, profile_name) = parse_target(target)?;
    let spec = tool_specs::get(tool_name)?;
    let cfg = Config::load(&paths.config_file())?;
    let profile = cfg
        .tool(tool_name)?
        .profiles
        .get(profile_name)
        .with_context(|| format!("tool '{tool_name}' has no profile '{profile_name}'"))?;
    print!(
        "{}",
        render_profile(
            tool_name,
            profile_name,
            profile,
            spec.native_home_env,
            &paths.profile_home_dir(tool_name, profile_name),
        )
    );
    Ok(())
}

#[derive(Debug)]
pub struct ToggleReport {
    pub tool: String,
    pub profile: String,
    pub enabled: bool,
    pub changed: bool,
    pub tool_enabled_remaining: usize,
}

/// Flip one profile's enabled flag under the config lock; idempotent by design.
pub fn set_profile_enabled(paths: &Paths, target: &str, enabled: bool) -> Result<ToggleReport> {
    let (tool_name, profile_name) = parse_target(target)?;
    let spec = tool_specs::get(tool_name)?;
    let config_path = paths.config_file();
    if !config_path.exists() {
        bail!(
            "no config at {} — run `rtr init` first",
            config_path.display()
        );
    }
    crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(&config_path), || {
        let mut config = Config::load(&config_path)?;
        let profile = config
            .tool(spec.name)?
            .profiles
            .get(profile_name)
            .with_context(|| format!("tool '{}' has no profile '{profile_name}'", spec.name))?;
        let changed = profile.enabled != enabled;
        if changed {
            crate::config::set_profile_enabled_in_file(
                &config_path,
                &mut config,
                spec.name,
                profile_name,
                enabled,
            )?;
        }
        Ok(ToggleReport {
            tool: spec.name.to_string(),
            profile: profile_name.to_string(),
            enabled,
            changed,
            tool_enabled_remaining: crate::selection::enabled_profiles(config.tool(spec.name)?)
                .len(),
        })
    })
}

pub fn render_toggle_report(report: &ToggleReport) -> String {
    let target = format!("{}/{}", report.tool, report.profile);
    let state = if report.enabled {
        "enabled"
    } else {
        "disabled"
    };
    let mut out = match (report.changed, report.enabled) {
        (false, _) => format!("{target} is already {state}\n"),
        (true, true) => format!("Enabled {target}\n"),
        (true, false) => format!(
            "Disabled {target} (re-enable with: rtr enable {})\n",
            crate::runner::shell_quote(&target)
        ),
    };
    if !report.enabled && report.tool_enabled_remaining == 0 {
        out.push_str(&format!(
            "note: tool '{}' has no enabled profiles\n",
            report.tool
        ));
    }
    out
}

pub fn run_set_profile_enabled(paths: &Paths, target: &str, enabled: bool) -> Result<()> {
    let report = set_profile_enabled(paths, target, enabled)?;
    print!("{}", render_toggle_report(&report));
    Ok(())
}

/// Confirm and permanently remove one configured profile and its native home.
pub fn run_remove_profile(
    paths: &Paths,
    tool_name: &str,
    profile_name: &str,
    assume_yes: bool,
) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    remove_profile_with_io(
        paths,
        tool_name,
        profile_name,
        assume_yes,
        &mut stdin.lock(),
        &mut stdout.lock(),
    )?;
    Ok(())
}

fn remove_profile_with_io<R: BufRead, W: Write>(
    paths: &Paths,
    tool_name: &str,
    profile_name: &str,
    assume_yes: bool,
    input: &mut R,
    output: &mut W,
) -> Result<bool> {
    let spec = tool_specs::get(tool_name)?;
    let config_path = paths.config_file();
    if !config_path.exists() {
        bail!(
            "no config at {} — run `rtr init` first",
            config_path.display()
        );
    }
    let config = Config::load(&config_path)?;
    let tool = config.tool(spec.name)?;
    if !tool.profiles.contains_key(profile_name) {
        bail!("tool '{}' has no profile '{profile_name}'", spec.name);
    }

    let profile_home = paths.profile_home_dir(spec.name, profile_name);
    output.write_all(b"Profile home to delete: ")?;
    output.write_all(profile_home.as_os_str().as_bytes())?;
    output.write_all(b"\n")?;
    if !assume_yes {
        write!(
            output,
            "Permanently delete {}/{} and its auth and sessions? [y/N] ",
            spec.name, profile_name
        )?;
        output.flush()?;
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            writeln!(output, "Cancelled.")?;
            return Ok(false);
        }
    }

    crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(&config_path), || {
        let mut config = Config::load(&config_path)?;
        let tool = config.tool(spec.name)?;
        if !tool.profiles.contains_key(profile_name) {
            bail!("tool '{}' has no profile '{profile_name}'", spec.name);
        }
        if !crate::config::remove_profile_entry_in_file(
            &config_path,
            &mut config,
            spec.name,
            profile_name,
        )? {
            bail!("tool '{}' has no profile '{profile_name}'", spec.name);
        }
        Ok(())
    })?;

    paths.remove_profile_home_dir(spec.name, profile_name)?;
    writeln!(output, "Removed profile: {}/{}", spec.name, profile_name)?;
    Ok(true)
}

/// Render tool configuration and selected profiles without loading child state.
pub fn render_status(cfg: &Config, tool_filter: Option<&str>) -> Result<String> {
    use std::fmt::Write as _;

    if let Some(name) = tool_filter {
        if !cfg.tools.contains_key(name) {
            bail!("no tool named '{name}' in config.toml");
        }
    }

    let mut out = String::from("rtr status\n\ntools:\n");
    for (name, tool) in &cfg.tools {
        if tool_filter.is_some_and(|filter| filter != name) {
            continue;
        }
        let profiles = tool
            .profiles
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "  {name}");
        let _ = writeln!(out, "    command:  {}", tool.command.join(" "));
        let _ = writeln!(
            out,
            "    profiles: {}",
            if profiles.is_empty() {
                "(none)"
            } else {
                &profiles
            }
        );
    }
    Ok(out)
}

pub fn print_status(paths: &Paths, tool_filter: Option<&str>) -> Result<()> {
    let cfg = Config::load(&paths.config_file())?;
    print!("{}", render_status(&cfg, tool_filter)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    fn test_paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
        }
    }

    fn write_config(paths: &Paths, text: &str) -> PathBuf {
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let path = paths.config_file();
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn target_parsing_requires_tool_slash_profile() {
        assert_eq!(
            parse_target("codex/work team").unwrap(),
            ("codex", "work team")
        );
        let err = parse_target("codex").unwrap_err().to_string();
        assert!(
            err.contains("profile target 'codex' must look like <tool>/<profile>"),
            "{err}"
        );
    }

    #[test]
    fn toggle_changes_state_once_and_is_idempotent_without_rewriting() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let path = write_config(
            &paths,
            "# mine\n[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.a]\n[tools.codex.profiles.b]\n",
        );

        let disabled = set_profile_enabled(&paths, "codex/a", false).unwrap();
        assert!(disabled.changed);
        assert_eq!(disabled.tool_enabled_remaining, 1);
        let after_change = std::fs::read_to_string(&path).unwrap();
        assert!(after_change.contains("# mine"), "{after_change}");

        let changed_meta = std::fs::metadata(&path).unwrap();
        let repeat = set_profile_enabled(&paths, "codex/a", false).unwrap();
        assert!(!repeat.changed);
        assert_eq!(repeat.tool_enabled_remaining, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_change);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            changed_meta.modified().unwrap()
        );

        let enabled = set_profile_enabled(&paths, "codex/a", true).unwrap();
        assert!(enabled.changed);
        assert_eq!(enabled.tool_enabled_remaining, 2);
    }

    #[test]
    fn toggle_rejects_unknown_targets_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let original = "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.a]\n";
        let path = write_config(&paths, original);

        for (target, expected) in [
            ("codex", "must look like <tool>/<profile>"),
            ("curl/a", "unsupported subscription tool 'curl'"),
            ("codex/ghost", "tool 'codex' has no profile 'ghost'"),
        ] {
            let err = set_profile_enabled(&paths, target, false)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "target {target}: {err}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn toggle_without_config_points_to_init_and_creates_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());

        let err = set_profile_enabled(&paths, "codex/a", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("rtr init"), "{err}");
        assert!(!paths.config_dir.exists());
    }

    #[test]
    fn concurrent_toggles_on_different_profiles_both_persist() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        write_config(
            &paths,
            "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.a]\n[tools.codex.profiles.b]\n",
        );

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = ["codex/a", "codex/b"]
            .into_iter()
            .map(|target| {
                let barrier = std::sync::Arc::clone(&barrier);
                let paths = paths.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    set_profile_enabled(&paths, target, false)
                })
            })
            .collect();
        for handle in handles {
            assert!(handle.join().unwrap().unwrap().changed);
        }

        let config = Config::load(&paths.config_file()).unwrap();
        assert!(!config.tool("codex").unwrap().profiles["a"].enabled);
        assert!(!config.tool("codex").unwrap().profiles["b"].enabled);
    }

    #[test]
    fn toggle_messages_tell_one_consistent_story() {
        let report = |enabled, changed, remaining| ToggleReport {
            tool: "codex".to_string(),
            profile: "personal".to_string(),
            enabled,
            changed,
            tool_enabled_remaining: remaining,
        };
        assert_eq!(
            render_toggle_report(&report(false, true, 1)),
            "Disabled codex/personal (re-enable with: rtr enable codex/personal)\n"
        );
        assert_eq!(
            render_toggle_report(&report(false, true, 0)),
            "Disabled codex/personal (re-enable with: rtr enable codex/personal)\nnote: tool 'codex' has no enabled profiles\n"
        );
        assert_eq!(
            render_toggle_report(&report(true, true, 2)),
            "Enabled codex/personal\n"
        );
        assert_eq!(
            render_toggle_report(&report(false, false, 0)),
            "codex/personal is already disabled\nnote: tool 'codex' has no enabled profiles\n"
        );
        assert_eq!(
            render_toggle_report(&report(true, false, 1)),
            "codex/personal is already enabled\n"
        );

        let quoted = render_toggle_report(&ToggleReport {
            tool: "codex".to_string(),
            profile: "work team".to_string(),
            enabled: false,
            changed: true,
            tool_enabled_remaining: 1,
        });
        assert_eq!(
            quoted,
            "Disabled codex/work team (re-enable with: rtr enable 'codex/work team')\n"
        );
    }

    fn write_removal_fixture(paths: &Paths) {
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.config_file(),
            "# keep\n[tools.codex]\ncommand = [\"codex\"]\n\n[tools.codex.profiles.personal]\n\n[tools.codex.profiles.work]\n",
        )
        .unwrap();
        for profile in ["personal", "work"] {
            let home = paths.ensure_profile_home_dir("codex", profile).unwrap();
            std::fs::write(home.join("auth.json"), profile).unwrap();
        }
    }

    #[test]
    fn profile_views_contain_only_native_launcher_state() {
        let cfg =
            Config::parse("[tools.codex]\ncommand=[\"codex\"]\n[tools.codex.profiles.personal]\n")
                .unwrap();
        let list = render_profile_list(&cfg);
        assert!(list.contains("personal (enabled)"), "{list}");

        let profile = cfg.tool("codex").unwrap().profiles.get("personal").unwrap();
        let shown = render_profile(
            "codex",
            "personal",
            profile,
            "CODEX_HOME",
            Path::new("/state/homes/codex/personal"),
        );
        assert!(shown.contains("CODEX_HOME=/state/homes/codex/personal"));

        let status = render_status(&cfg, None).unwrap();
        assert!(status.contains("  codex\n"), "{status}");
        for removed in ["proxy", "host", "CA", "trust", "rewrite", "capture"] {
            assert!(!list.contains(removed), "{list}");
            assert!(!shown.contains(removed), "{shown}");
            assert!(!status.contains(removed), "{status}");
        }
    }

    #[test]
    fn profile_removal_can_be_cancelled_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        write_removal_fixture(&paths);
        let before = std::fs::read_to_string(paths.config_file()).unwrap();
        let mut input = std::io::Cursor::new(b"n\n");
        let mut output = Vec::new();

        assert!(!remove_profile_with_io(
            &paths,
            "codex",
            "personal",
            false,
            &mut input,
            &mut output,
        )
        .unwrap());

        assert_eq!(
            std::fs::read_to_string(paths.config_file()).unwrap(),
            before
        );
        assert!(paths
            .profile_home_dir("codex", "personal")
            .join("auth.json")
            .is_file());
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Profile home to delete:"), "{output}");
        assert!(output.contains("Cancelled"), "{output}");
    }

    #[test]
    fn profile_removal_prints_exact_non_utf8_home_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = test_paths(dir.path());
        paths.state_dir = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"state-\xff".to_vec()));
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.config_file(),
            "[tools.codex]\ncommand = [\"codex\"]\n[tools.codex.profiles.personal]\n",
        )
        .unwrap();
        let mut input = std::io::Cursor::new(b"n\n");
        let mut output = Vec::new();

        remove_profile_with_io(&paths, "codex", "personal", false, &mut input, &mut output)
            .unwrap();

        let expected_path = paths.profile_home_dir("codex", "personal");
        let expected = expected_path.as_os_str().as_bytes();
        assert!(
            output
                .windows(expected.len())
                .any(|window| window == expected),
            "output did not contain exact path bytes: {output:?}"
        );
    }

    #[test]
    fn confirmed_profile_removal_deletes_only_the_selected_profile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        write_removal_fixture(&paths);
        let mut input = std::io::empty();
        let mut output = Vec::new();

        assert!(
            remove_profile_with_io(&paths, "codex", "personal", true, &mut input, &mut output,)
                .unwrap()
        );

        let config = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(config.contains("# keep"), "{config}");
        assert!(!config.contains("profiles.personal"), "{config}");
        assert!(config.contains("profiles.work"), "{config}");
        assert!(!paths.profile_home_dir("codex", "personal").exists());
        assert!(paths
            .profile_home_dir("codex", "work")
            .join("auth.json")
            .is_file());
    }

    #[test]
    fn unknown_profile_is_rejected_before_home_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        write_removal_fixture(&paths);
        let mut input = std::io::empty();
        let mut output = Vec::new();

        let error =
            remove_profile_with_io(&paths, "codex", "missing", true, &mut input, &mut output)
                .unwrap_err()
                .to_string();

        assert!(error.contains("no profile 'missing'"), "{error}");
        assert!(paths
            .profile_home_dir("codex", "personal")
            .join("auth.json")
            .is_file());
    }
}
