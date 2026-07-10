//! Script-friendly config path display and editor launching.

use std::ffi::OsString;
use std::process::ExitStatus;

use anyhow::{bail, Context, Result};

use crate::paths::Paths;

pub fn render_config_path(paths: &Paths) -> String {
    format!("{}\n", paths.config_file().display())
}

pub fn edit_config(paths: &Paths) -> Result<i32> {
    let path = paths.config_file();
    if !path.exists() {
        bail!("no config at {} — run `rtr init` first", path.display());
    }
    let editor = select_editor(std::env::var_os("VISUAL"), std::env::var_os("EDITOR"))?;
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("launching editor '{}'", editor.to_string_lossy()))?;
    Ok(exit_code(status))
}

fn select_editor(visual: Option<OsString>, editor: Option<OsString>) -> Result<OsString> {
    visual
        .filter(|value| !value.is_empty())
        .or_else(|| editor.filter(|value| !value.is_empty()))
        .context("neither $VISUAL nor $EDITOR is set; set one to use `rtr config edit`")
}

fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::paths::Paths;

    fn test_paths() -> Paths {
        Paths {
            config_dir: PathBuf::from("/tmp/rtr-config"),
            state_dir: PathBuf::from("/tmp/rtr-state"),
        }
    }

    #[test]
    fn rendered_config_path_is_script_friendly() {
        assert_eq!(
            render_config_path(&test_paths()),
            "/tmp/rtr-config/config.toml\n"
        );
    }

    #[test]
    fn editor_selection_prefers_visual_and_ignores_empty_values() {
        assert_eq!(
            select_editor(
                Some(OsString::from("visual")),
                Some(OsString::from("editor"))
            )
            .unwrap(),
            OsString::from("visual")
        );
        assert_eq!(
            select_editor(Some(OsString::new()), Some(OsString::from("editor"))).unwrap(),
            OsString::from("editor")
        );
        let error = select_editor(None, Some(OsString::new()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("VISUAL"), "{error}");
        assert!(error.contains("EDITOR"), "{error}");
    }
}
