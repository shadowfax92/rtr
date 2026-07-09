use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub runtime_hosts: &'static [&'static str],
    pub native_home_env: &'static str,
    pub native_secure_storage_env: Option<&'static str>,
    pub default_skills_source: &'static [&'static str],
    pub rebase_external_skill_symlinks: bool,
}

pub const CLAUDE: ToolSpec = ToolSpec {
    name: "claude",
    runtime_hosts: &[".anthropic.com"],
    native_home_env: "CLAUDE_CONFIG_DIR",
    native_secure_storage_env: Some("CLAUDE_SECURESTORAGE_CONFIG_DIR"),
    default_skills_source: &[".claude", "skills"],
    rebase_external_skill_symlinks: true,
};

pub const CODEX: ToolSpec = ToolSpec {
    name: "codex",
    runtime_hosts: &["chatgpt.com"],
    native_home_env: "CODEX_HOME",
    native_secure_storage_env: None,
    default_skills_source: &[".codex", "skills"],
    rebase_external_skill_symlinks: false,
};

pub const SPECS: &[ToolSpec] = &[CLAUDE, CODEX];

/// Resolve the first-class subscription tool definition used by add and runtime commands.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_spec_defines_runtime_home_and_skills() {
        let spec = get("claude").unwrap();
        assert_eq!(spec.runtime_hosts, &[".anthropic.com"]);
        assert_eq!(spec.native_home_env, "CLAUDE_CONFIG_DIR");
        assert_eq!(
            spec.native_secure_storage_env,
            Some("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        );
        assert_eq!(spec.default_skills_source, &[".claude", "skills"]);
        assert!(spec.rebase_external_skill_symlinks);
    }

    #[test]
    fn codex_spec_defines_runtime_home_and_skills() {
        let spec = get("codex").unwrap();
        assert_eq!(spec.runtime_hosts, &["chatgpt.com"]);
        assert_eq!(spec.native_home_env, "CODEX_HOME");
        assert_eq!(spec.native_secure_storage_env, None);
        assert_eq!(spec.default_skills_source, &[".codex", "skills"]);
        assert!(!spec.rebase_external_skill_symlinks);
    }

    #[test]
    fn unknown_tool_is_rejected() {
        let err = get("curl").unwrap_err().to_string();
        assert!(err.contains("unsupported subscription tool"), "got: {err}");
    }
}
