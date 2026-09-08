//! Joins current profile policy with recorded launch outcomes for `rtr ls`.
//! Configured profiles remain visible with zero activity. Usage from removed
//! profiles appears separately, without inventing current enable/bypass state.
use std::fmt::Write as _;

use anyhow::Result;
use chrono::Local;

use crate::{
    config::{Config, Profile},
    output::{display_width, Style, Tone},
    paths::Paths,
    usage::{self, Stats},
};

type Row = [(String, Tone); 6];
const HEADERS: [&str; 6] = ["AGENT", "PROFILE", "STATE", "HOME", "RUNS", "FAILED"];

pub fn run(paths: &Paths, all: bool, style: Style) -> Result<()> {
    let config = match Config::load(&paths.config_file()) {
        Ok(config) => config,
        // Historical usage can outlive the entire config, not only a profile.
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Config::default()
        }
        Err(error) => return Err(error),
    };
    let day = (!all).then(|| Local::now().date_naive());
    let stats = match usage::read_events(&paths.usage_file()) {
        Ok(events) => Some(usage::aggregate(&events, day)),
        Err(error) => {
            // An unavailable history must not hide configured profiles or turn
            // unreadable counts into misleading zeros.
            eprintln!("rtr: usage unavailable: {error:#}");
            None
        }
    };
    print!(
        "{}",
        render(
            &config,
            stats.as_ref(),
            if all { "all time" } else { "today" },
            style
        )
    );
    if !all {
        println!(
            "\n{}",
            style.paint("Tip: rtr ls --all shows all-time usage.", Tone::Muted)
        );
    }
    Ok(())
}

pub fn render(config: &Config, stats: Option<&Stats>, label: &str, style: Style) -> String {
    let mut configured = Vec::new();
    for (tool, settings) in &config.tools {
        for (name, profile) in &settings.profiles {
            configured.push(row(tool, name, Some(profile), stats));
        }
    }
    let mut historical = Vec::new();
    if let Some(stats) = stats {
        for (tool, profiles) in stats {
            for name in profiles.keys() {
                if !config
                    .tools
                    .get(tool)
                    .is_some_and(|settings| settings.profiles.contains_key(name))
                {
                    historical.push(row(tool, name, None, Some(stats)));
                }
            }
        }
    }
    let mut widths = HEADERS.map(str::len);
    for row in configured.iter().chain(&historical) {
        for (index, (text, _)) in row.iter().enumerate() {
            widths[index] = widths[index].max(display_width(text));
        }
    }
    let mut out = format!(
        "{} {}\n\n",
        style.paint("rtr profiles", Tone::Strong),
        style.paint(&format!("· {label}"), Tone::Muted)
    );
    if configured.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            style.paint("No configured profiles.", Tone::Muted)
        );
    } else {
        table(&mut out, &configured, widths, style);
    }
    if !historical.is_empty() {
        let _ = writeln!(
            out,
            "\n{}",
            style.paint("Removed profiles · recorded usage", Tone::Muted)
        );
        table(&mut out, &historical, widths, style);
    }
    if stats.is_none() {
        let _ = writeln!(
            out,
            "\n{}",
            style.paint("Usage unavailable; counts shown as -.", Tone::Warning)
        );
    }
    out
}

fn row(tool: &str, name: &str, profile: Option<&Profile>, stats: Option<&Stats>) -> Row {
    let (state, state_tone, home, home_tone) = match profile {
        Some(profile) => (
            if profile.enabled {
                "enabled"
            } else {
                "disabled"
            },
            if profile.enabled {
                Tone::Good
            } else {
                Tone::Muted
            },
            if profile.bypass {
                "bypassed"
            } else {
                "isolated"
            },
            if profile.bypass {
                Tone::Warning
            } else {
                Tone::Normal
            },
        ),
        None => ("removed", Tone::Muted, "-", Tone::Muted),
    };
    let counts = stats.and_then(|stats| stats.get(tool)?.get(name));
    let runs = counts.map_or(0, |counts| counts.runs);
    let failures = counts.map_or(0, |counts| counts.failures);
    [
        (tool.into(), Tone::Accent),
        (name.into(), Tone::Strong),
        (state.into(), state_tone),
        (home.into(), home_tone),
        (
            if stats.is_some() {
                runs.to_string()
            } else {
                "-".into()
            },
            if runs == 0 { Tone::Muted } else { Tone::Normal },
        ),
        (
            if stats.is_some() {
                failures.to_string()
            } else {
                "-".into()
            },
            if failures == 0 {
                Tone::Muted
            } else {
                Tone::Bad
            },
        ),
    ]
}

fn table(out: &mut String, rows: &[Row], widths: [usize; 6], style: Style) {
    let header = HEADERS
        .iter()
        .enumerate()
        .map(|(i, text)| style.padded(text, Tone::Muted, widths[i], i >= 4))
        .collect::<Vec<_>>()
        .join("  ");
    let _ = writeln!(out, "{header}");
    let mut previous_tool = "";
    for row in rows {
        let line = row
            .iter()
            .enumerate()
            .map(|(i, (text, tone))| {
                let text = if i == 0 && text == previous_tool {
                    ""
                } else {
                    text
                };
                style.padded(text, *tone, widths[i], i >= 4)
            })
            .collect::<Vec<_>>()
            .join("  ");
        let _ = writeln!(out, "{line}");
        previous_tool = &row[0].0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageEvent;
    use chrono::NaiveDate;

    fn config() -> Config {
        Config::parse("[tools.codex]\ncommand=[\"codex\"]\n[tools.codex.profiles.busy]\nbypass=true\n[tools.codex.profiles.idle]\nenabled=false\n").unwrap()
    }

    #[test]
    fn joins_unused_profiles_with_usage_and_keeps_removed_profiles_separate() {
        let events = [("busy", Some(0)), ("busy", None), ("removed", Some(1))].map(
            |(profile, exit_code)| UsageEvent {
                ts: "2026-09-08T12:00:00Z".into(),
                tool: "codex".into(),
                profile: profile.into(),
                exit_code,
                bypass: false,
            },
        );
        let stats = usage::aggregate(&events, None);
        let rendered = render(&config(), Some(&stats), "all time", Style::default());
        let flat = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("busy enabled bypassed 2 1"), "{rendered}");
        assert!(flat.contains("idle disabled isolated 0 0"), "{rendered}");
        let history = flat
            .split_once("Removed profiles · recorded usage")
            .unwrap()
            .1;
        assert!(history.contains("removed removed - 1 1"), "{rendered}");
        assert!(!history.contains("busy"), "{rendered}");

        let empty_day =
            usage::aggregate(&events, Some(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()));
        let today = render(&config(), Some(&empty_day), "today", Style::default());
        let flat = today.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("busy enabled bypassed 0 0"), "{today}");
        assert!(!today.contains("Removed profiles"), "{today}");
    }

    #[test]
    fn unavailable_usage_does_not_claim_zero_runs() {
        let rendered = render(&config(), None, "today", Style::default());
        let flat = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("busy enabled bypassed - -"), "{rendered}");
        assert!(rendered.contains("Usage unavailable"));
    }
}
