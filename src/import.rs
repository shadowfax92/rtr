use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::capture::CaptureRecord;
use crate::config::{self, Config, Profile, Tool};
use crate::paths::Paths;
use crate::tool_specs::{self, ToolSpec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthBundle {
    pub rewrites: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
    pub hosts: BTreeSet<String>,
    pub discarded_legacy_rewrites: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    Prompt,
    Force,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportSaveOutcome {
    pub saved: bool,
    pub overwritten: bool,
}

/// Extract the tool-specific auth bundle from a capture JSONL file.
pub fn extract_auth_bundle(spec: &ToolSpec, path: &Path) -> Result<AuthBundle> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading capture {}", path.display()))?;
    extract_auth_bundle_from_jsonl(spec, &text)
}

fn extract_auth_bundle_from_jsonl(spec: &ToolSpec, text: &str) -> Result<AuthBundle> {
    let mut records = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: CaptureRecord = serde_json::from_str(line)
            .with_context(|| format!("parsing capture line {}", idx + 1))?;
        records.push(rec);
    }
    extract_auth_bundle_from_records(spec, &records)
}

fn extract_auth_bundle_from_records(
    spec: &ToolSpec,
    records: &[CaptureRecord],
) -> Result<AuthBundle> {
    let mut values: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let mut metadata: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let mut hosts = BTreeSet::new();

    for record in records {
        if !tool_specs::matches_capture_host(spec, &record.host) {
            continue;
        }
        hosts.insert(record.host.clone());
        for (name, value) in &record.headers {
            if let Some(canonical) = matching_header(spec.required_headers, name) {
                values.entry(canonical).or_default().insert(value.clone());
            }
            if let Some(canonical) = matching_header(spec.metadata_headers, name) {
                metadata.entry(canonical).or_default().insert(value.clone());
            }
        }
    }

    let mut candidate_rewrites = BTreeMap::new();
    let mut ambiguous_legacy_rewrites = false;
    for required in spec.required_headers {
        let Some(found) = values.get(required) else {
            continue;
        };
        if found.len() > 1 {
            ambiguous_legacy_rewrites = true;
            continue;
        }
        candidate_rewrites.insert(
            (*required).to_string(),
            found.iter().next().unwrap().clone(),
        );
    }
    let incomplete_legacy_rewrites =
        !candidate_rewrites.is_empty() && candidate_rewrites.len() < spec.required_headers.len();
    let discarded_legacy_rewrites = ambiguous_legacy_rewrites || incomplete_legacy_rewrites;
    let rewrites = if discarded_legacy_rewrites {
        BTreeMap::new()
    } else {
        candidate_rewrites
    };

    let metadata = metadata
        .into_iter()
        .filter_map(|(name, values)| {
            let value = if values.len() == 1 {
                values.into_iter().next().unwrap()
            } else {
                format!("<{} distinct values>", values.len())
            };
            Some((name.to_string(), value))
        })
        .collect();

    Ok(AuthBundle {
        rewrites,
        metadata,
        hosts,
        discarded_legacy_rewrites,
    })
}

fn matching_header(headers: &'static [&'static str], name: &str) -> Option<&'static str> {
    headers
        .iter()
        .copied()
        .find(|expected| expected.eq_ignore_ascii_case(name))
}

fn bundle_value(value: &str, show_secrets: bool) -> String {
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

pub fn render_auth_bundle(spec: &ToolSpec, bundle: &AuthBundle, show_secrets: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "Detected {} auth bundle:", display_tool(spec.name));
    if bundle.rewrites.is_empty() {
        if bundle.discarded_legacy_rewrites {
            let _ = writeln!(
                out,
                "  Legacy rewrites: (incomplete or ambiguous bundle captured; not saved)"
            );
        } else {
            let _ = writeln!(out, "  Legacy rewrites: (none captured)");
        }
    } else {
        for name in spec.required_headers {
            if let Some(value) = bundle.rewrites.get(*name) {
                let _ = writeln!(out, "  {name}: {}", bundle_value(value, show_secrets));
            }
        }
    }
    if !bundle.hosts.is_empty() {
        let hosts: Vec<&str> = bundle.hosts.iter().map(String::as_str).collect();
        let _ = writeln!(out, "  Hosts: {}", hosts.join(", "));
    }
    for (name, value) in &bundle.metadata {
        let _ = writeln!(
            out,
            "  {name}: {} (metadata only)",
            bundle_value(value, show_secrets)
        );
    }
    out
}

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
            let _ = writeln!(out, "    {name}: {}", bundle_value(value, show_secrets));
        }
    }
    if !profile.remove.is_empty() {
        let _ = writeln!(out, "  remove: {}", profile.remove.join(", "));
    }
    if !profile.metadata.is_empty() {
        let _ = writeln!(out, "  metadata:");
        for (name, value) in &profile.metadata {
            let _ = writeln!(out, "    {name}: {}", bundle_value(value, show_secrets));
        }
    }
    out
}

pub fn render_profile_list(cfg: &Config) -> Result<String> {
    use std::fmt::Write as _;

    let mut out = String::new();
    for spec in tool_specs::all() {
        let _ = writeln!(out, "{}", spec.name);
        match cfg.tools.get(spec.name) {
            Some(tool) => {
                let profiles: Vec<&String> = tool.profiles.keys().collect();
                if profiles.is_empty() {
                    let _ = writeln!(out, "  profiles: (none)");
                } else {
                    let _ = writeln!(out, "  profiles:");
                    for name in profiles {
                        let profile = &tool.profiles[name];
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
                if tool.presets.is_empty() {
                    let _ = writeln!(out, "  presets: (none)");
                } else {
                    let _ = writeln!(out, "  presets:");
                    for name in tool.presets.keys() {
                        let suffix = if tool.default_preset.as_deref() == Some(name.as_str()) {
                            " [default]"
                        } else {
                            ""
                        };
                        let _ = writeln!(out, "    {name}{suffix}");
                    }
                }
            }
            None => {
                let _ = writeln!(out, "  profiles: (not configured)");
                let _ = writeln!(out, "  presets: (not configured)");
            }
        }
    }
    Ok(out)
}

/// Save an extracted legacy auth bundle as a profile, applying the overwrite policy.
pub fn save_imported_profile<F>(
    cfg: &mut Config,
    spec: &ToolSpec,
    profile_name: &str,
    bundle: &AuthBundle,
    policy: ConflictPolicy,
    confirm: F,
) -> Result<ImportSaveOutcome>
where
    F: FnOnce(&str, &str) -> Result<bool>,
{
    if bundle.hosts.is_empty() {
        bail!(
            "capture has no {} traffic; refusing to save profile '{}'",
            spec.name,
            profile_name
        );
    }

    let tool = cfg
        .tools
        .entry(spec.name.to_string())
        .or_insert_with(|| Tool {
            command: vec![spec.name.to_string()],
            hosts: tool_specs::runtime_hosts(spec),
            active: None,
            selection: Some("round-robin".to_string()),
            default_preset: None,
            presets: BTreeMap::new(),
            profiles: BTreeMap::new(),
        });

    let exists = tool.profiles.contains_key(profile_name);
    if exists {
        match policy {
            ConflictPolicy::Force => {}
            ConflictPolicy::Reject => bail!(
                "profile {}/{} already exists (use --force to overwrite)",
                spec.name,
                profile_name
            ),
            ConflictPolicy::Prompt => {
                if !confirm(spec.name, profile_name)? {
                    return Ok(ImportSaveOutcome {
                        saved: false,
                        overwritten: false,
                    });
                }
            }
        }
    }

    tool.profiles.insert(
        profile_name.to_string(),
        Profile {
            set: bundle.rewrites.clone(),
            remove: Vec::new(),
            enabled: true,
            metadata: bundle.metadata.clone(),
        },
    );
    Ok(ImportSaveOutcome {
        saved: true,
        overwritten: exists,
    })
}

pub fn parse_yes(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn prompt_overwrite(tool: &str, profile: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "profile {tool}/{profile} already exists; refusing to prompt on non-interactive stdin (use --force to overwrite or --no-overwrite to reject)"
        );
    }
    print!("Profile {tool}/{profile} already exists. Overwrite? [y/N] ");
    std::io::stdout().flush().context("flushing prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading overwrite confirmation")?;
    Ok(parse_yes(&answer))
}

pub fn run_import_profile(
    paths: &Paths,
    tool_name: &str,
    profile_name: &str,
    capture_path: &Path,
    policy: ConflictPolicy,
    show_secrets: bool,
) -> Result<()> {
    let spec = tool_specs::get(tool_name)?;
    let cfg_path = paths.config_file();
    if !cfg_path.exists() {
        bail!("no config at {} — run `rtr init` first", cfg_path.display());
    }

    println!(
        "Legacy import: first-class rtr {} uses {} native homes; captured headers are not required for normal onboarding.",
        spec.name, spec.native_home_env
    );
    let bundle = extract_auth_bundle(spec, capture_path)?;
    print!("{}", render_auth_bundle(spec, &bundle, show_secrets));

    let mut cfg = Config::load(&cfg_path)?;
    let outcome = save_imported_profile(
        &mut cfg,
        spec,
        profile_name,
        &bundle,
        policy,
        prompt_overwrite,
    )?;
    if !outcome.saved {
        println!("Skipped profile: {}/{}", spec.name, profile_name);
        return Ok(());
    }

    let text = cfg.to_toml()?;
    config::write_secret_file(&cfg_path, &text)?;
    if outcome.overwritten {
        println!("Updated profile: {}/{}", spec.name, profile_name);
    } else {
        println!("Saved profile: {}/{}", spec.name, profile_name);
    }
    Ok(())
}

pub fn run_list_profiles(paths: &Paths) -> Result<()> {
    let cfg = Config::load(&paths.config_file())?;
    print!("{}", render_profile_list(&cfg)?);
    Ok(())
}

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

fn display_tool(name: &str) -> &str {
    match name {
        "claude" => "Claude",
        "codex" => "Codex",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn rec(host: &str, headers: &[(&str, &str)]) -> CaptureRecord {
        CaptureRecord {
            ts: "2026-07-01T12:00:00Z".to_string(),
            method: "GET".to_string(),
            url: format!("https://{host}/x"),
            host: host.to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    fn jsonl(records: &[CaptureRecord]) -> String {
        records
            .iter()
            .map(|rec| serde_json::to_string(rec).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn claude_extracts_authorization_and_metadata_only() {
        let spec = tool_specs::get("claude").unwrap();
        let bundle = extract_auth_bundle_from_records(
            spec,
            &[
                rec(
                    "api.anthropic.com",
                    &[
                        ("authorization", "Bearer claude-token"),
                        ("x-organization-uuid", "org-1"),
                    ],
                ),
                rec(
                    "mcp-proxy.anthropic.com",
                    &[("Authorization", "Bearer claude-token")],
                ),
            ],
        )
        .unwrap();
        assert_eq!(bundle.rewrites.len(), 1);
        assert_eq!(
            bundle.rewrites.get("Authorization").map(String::as_str),
            Some("Bearer claude-token")
        );
        assert_eq!(
            bundle
                .metadata
                .get("x-organization-uuid")
                .map(String::as_str),
            Some("org-1")
        );
    }

    #[test]
    fn codex_extracts_required_headers_and_ignores_telemetry_and_cookie() {
        let spec = tool_specs::get("codex").unwrap();
        let bundle = extract_auth_bundle_from_records(
            spec,
            &[
                rec(
                    "chatgpt.com",
                    &[
                        ("authorization", "Bearer codex-token"),
                        ("chatgpt-account-id", "acct-1"),
                        ("cookie", "session=do-not-store"),
                    ],
                ),
                rec("ab.chatgpt.com", &[("statsig-api-key", "telemetry")]),
            ],
        )
        .unwrap();
        assert_eq!(
            bundle.rewrites.get("Authorization").map(String::as_str),
            Some("Bearer codex-token")
        );
        assert_eq!(
            bundle
                .rewrites
                .get("chatgpt-account-id")
                .map(String::as_str),
            Some("acct-1")
        );
        assert!(!bundle.rewrites.contains_key("cookie"));
        assert_eq!(
            bundle.hosts.iter().cloned().collect::<Vec<_>>(),
            vec!["chatgpt.com"]
        );
    }

    #[test]
    fn import_allows_missing_rewrites_and_errors_on_conflicts() {
        let codex = tool_specs::get("codex").unwrap();
        let missing = extract_auth_bundle_from_records(
            codex,
            &[rec(
                "chatgpt.com",
                &[("authorization", "Bearer codex-token")],
            )],
        )
        .unwrap();
        assert!(missing.rewrites.is_empty());
        assert!(missing.discarded_legacy_rewrites);
        assert!(!missing.rewrites.contains_key("chatgpt-account-id"));

        let empty =
            extract_auth_bundle_from_records(codex, &[rec("chatgpt.com", &[("accept", "*/*")])])
                .unwrap();
        assert!(empty.rewrites.is_empty());
        assert!(!empty.discarded_legacy_rewrites);

        let conflicting = extract_auth_bundle_from_records(
            codex,
            &[
                rec(
                    "chatgpt.com",
                    &[
                        ("authorization", "Bearer a"),
                        ("chatgpt-account-id", "acct-1"),
                    ],
                ),
                rec(
                    "chatgpt.com",
                    &[
                        ("authorization", "Bearer b"),
                        ("chatgpt-account-id", "acct-1"),
                    ],
                ),
            ],
        )
        .unwrap();
        assert!(conflicting.rewrites.is_empty());
        assert!(conflicting.discarded_legacy_rewrites);
    }

    #[test]
    fn extraction_reads_jsonl_capture() {
        let spec = tool_specs::get("claude").unwrap();
        let capture = jsonl(&[rec("api.anthropic.com", &[("authorization", "Bearer t")])]);
        let bundle = extract_auth_bundle_from_jsonl(spec, &capture).unwrap();
        assert_eq!(
            bundle.rewrites.get("Authorization").map(String::as_str),
            Some("Bearer t")
        );
    }

    #[test]
    fn rendering_redacts_by_default_and_reveals_on_request() {
        let spec = tool_specs::get("codex").unwrap();
        let bundle = AuthBundle {
            rewrites: [
                ("Authorization".to_string(), "Bearer raw-token".to_string()),
                ("chatgpt-account-id".to_string(), "acct-raw".to_string()),
            ]
            .into_iter()
            .collect(),
            metadata: BTreeMap::new(),
            hosts: ["chatgpt.com".to_string()].into_iter().collect(),
            discarded_legacy_rewrites: false,
        };
        let hidden = render_auth_bundle(spec, &bundle, false);
        assert!(!hidden.contains("raw-token"), "{hidden}");
        assert!(!hidden.contains("acct-raw"), "{hidden}");
        assert!(hidden.contains("<redacted>"), "{hidden}");

        let shown = render_auth_bundle(spec, &bundle, true);
        assert!(shown.contains("raw-token"), "{shown}");
        assert!(shown.contains("acct-raw"), "{shown}");
    }

    #[test]
    fn save_imported_profile_inserts_and_forces_overwrite() {
        let spec = tool_specs::get("codex").unwrap();
        let mut cfg = Config::parse("[tools.codex]\ncommand=[\"codex\"]\n").unwrap();
        let bundle = AuthBundle {
            rewrites: [("Authorization".to_string(), "Bearer new".to_string())]
                .into_iter()
                .collect(),
            metadata: BTreeMap::new(),
            hosts: ["chatgpt.com".to_string()].into_iter().collect(),
            discarded_legacy_rewrites: false,
        };
        let first = save_imported_profile(
            &mut cfg,
            spec,
            "personal",
            &bundle,
            ConflictPolicy::Reject,
            |_, _| Ok(false),
        )
        .unwrap();
        assert!(first.saved);
        assert!(!first.overwritten);

        let err = save_imported_profile(
            &mut cfg,
            spec,
            "personal",
            &bundle,
            ConflictPolicy::Reject,
            |_, _| Ok(false),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already exists"), "got: {err}");

        let forced = save_imported_profile(
            &mut cfg,
            spec,
            "personal",
            &bundle,
            ConflictPolicy::Force,
            |_, _| Ok(false),
        )
        .unwrap();
        assert!(forced.saved);
        assert!(forced.overwritten);
    }

    #[test]
    fn prompt_policy_accepts_yes_and_leaves_config_unchanged_on_no() {
        let spec = tool_specs::get("claude").unwrap();
        let mut cfg = Config::parse(
            r#"
[tools.claude]
command = ["claude"]

[tools.claude.profiles.work]
set = { Authorization = "Bearer old" }
"#,
        )
        .unwrap();
        let before = cfg.to_toml().unwrap();
        let bundle = AuthBundle {
            rewrites: [("Authorization".to_string(), "Bearer new".to_string())]
                .into_iter()
                .collect(),
            metadata: BTreeMap::new(),
            hosts: ["api.anthropic.com".to_string()].into_iter().collect(),
            discarded_legacy_rewrites: false,
        };
        let skipped = save_imported_profile(
            &mut cfg,
            spec,
            "work",
            &bundle,
            ConflictPolicy::Prompt,
            |_, _| Ok(false),
        )
        .unwrap();
        assert!(!skipped.saved);
        assert_eq!(cfg.to_toml().unwrap(), before);

        let saved = save_imported_profile(
            &mut cfg,
            spec,
            "work",
            &bundle,
            ConflictPolicy::Prompt,
            |_, _| Ok(true),
        )
        .unwrap();
        assert!(saved.saved);
        assert_eq!(
            cfg.tool("claude")
                .unwrap()
                .profiles
                .get("work")
                .unwrap()
                .set
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer new")
        );
        assert!(parse_yes("yes"));
        assert!(parse_yes("Y\n"));
        assert!(!parse_yes("no"));
    }

    #[test]
    fn non_interactive_prompt_rejects_without_reading() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let err = prompt_overwrite("codex", "personal")
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-interactive"), "got: {err}");
        assert!(err.contains("--force"), "got: {err}");
    }

    #[test]
    fn save_import_rejects_capture_without_matching_hosts() {
        let spec = tool_specs::get("codex").unwrap();
        let mut cfg = Config::parse("[tools.codex]\ncommand=[\"codex\"]\n").unwrap();
        let bundle = AuthBundle {
            rewrites: BTreeMap::new(),
            metadata: BTreeMap::new(),
            hosts: BTreeSet::new(),
            discarded_legacy_rewrites: false,
        };
        let err = save_imported_profile(
            &mut cfg,
            spec,
            "personal",
            &bundle,
            ConflictPolicy::Reject,
            |_, _| Ok(false),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no codex traffic"), "got: {err}");
    }

    #[test]
    fn imported_config_write_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config::write_secret_file(&path, crate::config::STARTER_CONFIG).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

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
    fn profile_list_hides_preset_args() {
        let cfg = Config::parse(
            r#"
[tools.codex]
command = ["codex"]
default_preset = "xhigh"

[tools.codex.presets.xhigh]
args = ["--api-key", "preset-secret"]
"#,
        )
        .unwrap();
        let rendered = render_profile_list(&cfg).unwrap();
        assert!(rendered.contains("xhigh [default]"), "{rendered}");
        assert!(!rendered.contains("--api-key"), "{rendered}");
        assert!(!rendered.contains("preset-secret"), "{rendered}");
    }
}
