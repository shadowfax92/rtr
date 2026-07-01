use anyhow::{bail, Result};

use crate::config::Profile;
use crate::rewrite::host_matches;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub capture_hosts: &'static [&'static str],
    pub runtime_hosts: &'static [&'static str],
    pub required_headers: &'static [&'static str],
    pub metadata_headers: &'static [&'static str],
}

pub const CLAUDE: ToolSpec = ToolSpec {
    name: "claude",
    capture_hosts: &["api.anthropic.com", "mcp-proxy.anthropic.com"],
    runtime_hosts: &[".anthropic.com"],
    required_headers: &["Authorization"],
    metadata_headers: &["x-organization-uuid"],
};

pub const CODEX: ToolSpec = ToolSpec {
    name: "codex",
    capture_hosts: &["chatgpt.com"],
    runtime_hosts: &["chatgpt.com"],
    required_headers: &["Authorization", "chatgpt-account-id"],
    metadata_headers: &[],
};

pub const SPECS: &[ToolSpec] = &[CLAUDE, CODEX];

/// Resolve the first-class subscription tool definition used by capture/import/runtime commands.
pub fn get(name: &str) -> Result<&'static ToolSpec> {
    SPECS.iter().find(|spec| spec.name == name).ok_or_else(|| {
        anyhow::anyhow!("unsupported subscription tool '{name}' (supported: claude, codex)")
    })
}

pub fn all() -> &'static [ToolSpec] {
    SPECS
}

pub fn runtime_hosts(spec: &ToolSpec) -> Vec<String> {
    spec.runtime_hosts
        .iter()
        .map(|host| (*host).to_string())
        .collect()
}

pub fn capture_hosts(spec: &ToolSpec) -> Vec<String> {
    spec.capture_hosts
        .iter()
        .map(|host| (*host).to_string())
        .collect()
}

pub fn matches_capture_host(spec: &ToolSpec, host: &str) -> bool {
    host_matches(host, &capture_hosts(spec))
}

pub fn validate_supported(name: &str) -> Result<()> {
    match get(name) {
        Ok(_) => Ok(()),
        Err(e) => bail!("{e}"),
    }
}

pub fn missing_required_rewrites(spec: &ToolSpec, profile: &Profile) -> Vec<&'static str> {
    spec.required_headers
        .iter()
        .copied()
        .filter(|name| {
            !profile
                .set
                .keys()
                .any(|existing| existing.eq_ignore_ascii_case(name))
        })
        .collect()
}

/// Ensure a runtime profile contains the auth rewrites required by its first-class tool spec.
pub fn validate_runtime_profile(
    spec: &ToolSpec,
    profile_name: &str,
    profile: &Profile,
) -> Result<()> {
    let missing = missing_required_rewrites(spec, profile);
    if missing.is_empty() {
        return Ok(());
    }
    bail!(
        "profile {}/{} is missing required rewrites: {}",
        spec.name,
        profile_name,
        missing.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_spec_matches_expected_hosts_and_headers() {
        let spec = get("claude").unwrap();
        assert_eq!(
            spec.capture_hosts,
            &["api.anthropic.com", "mcp-proxy.anthropic.com"]
        );
        assert_eq!(spec.runtime_hosts, &[".anthropic.com"]);
        assert_eq!(spec.required_headers, &["Authorization"]);
        assert!(matches_capture_host(spec, "api.anthropic.com"));
        assert!(matches_capture_host(spec, "mcp-proxy.anthropic.com"));
        assert!(!matches_capture_host(spec, "example.com"));
    }

    #[test]
    fn codex_spec_uses_exact_chatgpt_host() {
        let spec = get("codex").unwrap();
        assert_eq!(spec.capture_hosts, &["chatgpt.com"]);
        assert_eq!(spec.runtime_hosts, &["chatgpt.com"]);
        assert_eq!(
            spec.required_headers,
            &["Authorization", "chatgpt-account-id"]
        );
        assert!(matches_capture_host(spec, "chatgpt.com"));
        assert!(!matches_capture_host(spec, "ab.chatgpt.com"));
    }

    #[test]
    fn unknown_tool_is_rejected() {
        let err = get("curl").unwrap_err().to_string();
        assert!(err.contains("unsupported subscription tool"), "got: {err}");
    }

    #[test]
    fn runtime_profile_requires_spec_rewrites() {
        let codex = get("codex").unwrap();
        let profile = Profile {
            set: [("Authorization".to_string(), "Bearer token".to_string())]
                .into_iter()
                .collect(),
            ..Profile::default()
        };
        let err = validate_runtime_profile(codex, "personal", &profile)
            .unwrap_err()
            .to_string();
        assert!(err.contains("codex/personal"), "got: {err}");
        assert!(err.contains("chatgpt-account-id"), "got: {err}");

        let profile = Profile {
            set: [
                ("authorization".to_string(), "Bearer token".to_string()),
                ("CHATGPT-ACCOUNT-ID".to_string(), "acct".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Profile::default()
        };
        validate_runtime_profile(codex, "personal", &profile).unwrap();
    }
}
