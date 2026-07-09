//! Profile inspection commands for the native launcher.

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
}
