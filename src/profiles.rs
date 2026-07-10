//! Profile inspection commands for the native launcher.

use std::io::{BufRead, Write};
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

pub fn run_show_profile(paths: &Paths, target: &str) -> Result<()> {
    let (tool_name, profile_name) = target
        .split_once('/')
        .with_context(|| format!("profile target '{target}' must look like <tool>/<profile>"))?;
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
    writeln!(output, "Profile home to delete: {}", profile_home.display())?;
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

    fn test_paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
        }
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
