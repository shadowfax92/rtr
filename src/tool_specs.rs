use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub native_home_env: &'static str,
    pub default_skills_source: &'static [&'static str],
}

pub const CLAUDE: ToolSpec = ToolSpec {
    name: "claude",
    native_home_env: "CLAUDE_CONFIG_DIR",
    default_skills_source: &[".claude", "skills"],
};

pub const CODEX: ToolSpec = ToolSpec {
    name: "codex",
    native_home_env: "CODEX_HOME",
    default_skills_source: &[".codex", "skills"],
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
    fn specs_define_only_native_home_and_skills_ownership() {
        assert_eq!(get("claude").unwrap().native_home_env, "CLAUDE_CONFIG_DIR");
        assert_eq!(
            get("claude").unwrap().default_skills_source,
            &[".claude", "skills"]
        );
        assert_eq!(get("codex").unwrap().native_home_env, "CODEX_HOME");
        assert_eq!(
            get("codex").unwrap().default_skills_source,
            &[".codex", "skills"]
        );
        assert!(get("curl").is_err());
    }
}
