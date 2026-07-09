use anyhow::{Context, Result};

use crate::config::{Config, Profile};
use crate::paths::Paths;
use crate::tool_specs;

fn display_value(value: &str, show_secrets: bool) -> String {
    if show_secrets {
        value.to_string()
    } else if let Some((scheme, _)) = value.split_once(' ') {
        if scheme.eq_ignore_ascii_case("bearer") {
            return format!("{scheme} <redacted>, len {}", value.len());
        }
        format!("<redacted>, len {}", value.len())
    } else {
        format!("<redacted>, len {}", value.len())
    }
}

/// Render one profile while redacting stored secrets by default.
pub fn render_profile(
    tool: &str,
    profile_name: &str,
    profile: &Profile,
    show_secrets: bool,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "{tool}/{profile_name}");
    let _ = writeln!(out, "  enabled: {}", profile.enabled);
    if profile.set.is_empty() {
        let _ = writeln!(out, "  rewrites: (none)");
    } else {
        let _ = writeln!(out, "  rewrites:");
        for (name, value) in &profile.set {
            let _ = writeln!(out, "    {name}: {}", display_value(value, show_secrets));
        }
    }
    if !profile.remove.is_empty() {
        let _ = writeln!(out, "  remove: {}", profile.remove.join(", "));
    }
    if !profile.metadata.is_empty() {
        let _ = writeln!(out, "  metadata:");
        for (name, value) in &profile.metadata {
            let _ = writeln!(out, "    {name}: {}", display_value(value, show_secrets));
        }
    }
    out
}

/// Render the configured Claude and Codex profile inventory.
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
                    let rewrites: Vec<&str> = profile.set.keys().map(String::as_str).collect();
                    let rewrites = if rewrites.is_empty() {
                        "(none)".to_string()
                    } else {
                        rewrites.join(", ")
                    };
                    let _ = writeln!(out, "    {name} ({status}; rewrites: {rewrites})");
                }
            }
            None => {
                let _ = writeln!(out, "  profiles: (not configured)");
            }
        }
    }
    out
}

/// Print all configured Claude and Codex profiles.
pub fn run_list_profiles(paths: &Paths) -> Result<()> {
    let cfg = Config::load(&paths.config_file())?;
    print!("{}", render_profile_list(&cfg));
    Ok(())
}

/// Print one profile target in `<tool>/<profile>` form.
pub fn run_show_profile(paths: &Paths, target: &str, show_secrets: bool) -> Result<()> {
    let (tool_name, profile_name) = target
        .split_once('/')
        .with_context(|| format!("profile target '{target}' must look like <tool>/<profile>"))?;
    tool_specs::get(tool_name)?;
    let cfg = Config::load(&paths.config_file())?;
    let tool = cfg.tool(tool_name)?;
    let profile = tool
        .profiles
        .get(profile_name)
        .with_context(|| format!("tool '{tool_name}' has no profile '{profile_name}'"))?;
    print!(
        "{}",
        render_profile(tool_name, profile_name, profile, show_secrets)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_rendering_redacts_rewrites() {
        let profile = Profile {
            set: [("Authorization".to_string(), "Bearer secret".to_string())]
                .into_iter()
                .collect(),
            metadata: [("x-organization-uuid".to_string(), "org-secret".to_string())]
                .into_iter()
                .collect(),
            ..Profile::default()
        };
        let hidden = render_profile("claude", "work", &profile, false);
        assert!(!hidden.contains("secret"), "{hidden}");
        let shown = render_profile("claude", "work", &profile, true);
        assert!(shown.contains("Bearer secret"), "{shown}");
        assert!(shown.contains("org-secret"), "{shown}");
    }

    #[test]
    fn profile_list_shows_profiles_only() {
        let cfg = Config::parse(
            r#"
[tools.codex]
command = ["codex"]

[tools.codex.profiles.personal]
set = {}
"#,
        )
        .unwrap();
        let rendered = render_profile_list(&cfg);
        assert!(rendered.contains("personal"), "{rendered}");
        assert!(!rendered.contains("presets:"), "{rendered}");
    }
}
