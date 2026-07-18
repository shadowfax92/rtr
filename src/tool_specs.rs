use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub resume_args: &'static [&'static str],
    pub native_home_env: &'static str,
    pub native_secure_storage_env: Option<&'static str>,
    pub default_skills_source: &'static [&'static str],
    pub rebase_external_skill_symlinks: bool,
}

pub const CLAUDE: ToolSpec = ToolSpec {
    name: "claude",
    resume_args: &["--resume"],
    native_home_env: "CLAUDE_CONFIG_DIR",
    native_secure_storage_env: Some("CLAUDE_SECURESTORAGE_CONFIG_DIR"),
    default_skills_source: &[".claude", "skills"],
    rebase_external_skill_symlinks: true,
};

pub const CODEX: ToolSpec = ToolSpec {
    name: "codex",
    resume_args: &["resume"],
    native_home_env: "CODEX_HOME",
    native_secure_storage_env: None,
    default_skills_source: &[".codex", "skills"],
    rebase_external_skill_symlinks: true,
};

pub const SPECS: &[ToolSpec] = &[CLAUDE, CODEX];

/// Resolve one of rtr's first-class native-profile tools.
pub fn get(name: &str) -> Result<&'static ToolSpec> {
    SPECS.iter().find(|spec| spec.name == name).ok_or_else(|| {
        anyhow::anyhow!("unsupported subscription tool '{name}' (supported: claude, codex)")
    })
}

pub fn all() -> &'static [ToolSpec] {
    SPECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_define_tool_runtime_contracts() {
        assert_eq!(get("claude").unwrap().resume_args, &["--resume"]);
        assert_eq!(get("claude").unwrap().native_home_env, "CLAUDE_CONFIG_DIR");
        assert_eq!(
            get("claude").unwrap().native_secure_storage_env,
            Some("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        );
        assert_eq!(
            get("claude").unwrap().default_skills_source,
            &[".claude", "skills"]
        );
        assert!(get("claude").unwrap().rebase_external_skill_symlinks);
        assert_eq!(get("codex").unwrap().resume_args, &["resume"]);
        assert_eq!(get("codex").unwrap().native_home_env, "CODEX_HOME");
        assert_eq!(get("codex").unwrap().native_secure_storage_env, None);
        assert_eq!(
            get("codex").unwrap().default_skills_source,
            &[".codex", "skills"]
        );
        assert!(get("codex").unwrap().rebase_external_skill_symlinks);
        assert!(get("curl").is_err());
    }
}
