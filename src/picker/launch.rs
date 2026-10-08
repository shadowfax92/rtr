//! Read-only launch descriptions use the runner's argument merge policy.
//! The picker reports explicit requested settings. If no model/effort is passed,
//! it says "native default" rather than guessing from layered native settings.
use super::search::clean;
use crate::{
    config::Config,
    conversations::{Conversation, OpenMode},
    runner,
};

#[derive(Clone, Default)]
pub(super) struct Launch {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub arguments: String,
}

impl Launch {
    pub fn resolve(config: &Config, conversation: &Conversation, extra: &[String]) -> Self {
        let Some(tool) = config.tools.get(&conversation.tool) else {
            return Self::default();
        };
        let args = runner::merge_tool_args(&conversation.tool, &tool.args, extra);
        let settings = crate::launch_settings::LaunchSettings::from_args(&conversation.tool, &args);
        Self {
            model: settings.model,
            effort: settings.effort,
            arguments: clean(
                &args
                    .iter()
                    .map(|arg| runner::shell_quote(arg))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        }
    }

    pub fn line(
        &self,
        conversation: &Conversation,
        mode: OpenMode,
        to_profile: Option<&str>,
    ) -> String {
        let action = if mode == OpenMode::Fork {
            format!(
                "Fork {}/{} → {}",
                clean(&conversation.tool),
                clean(&conversation.profile),
                to_profile
                    .map(clean)
                    .unwrap_or_else(|| "next profile".into())
            )
        } else {
            format!(
                "Resume with {}/{}",
                clean(&conversation.tool),
                clean(&conversation.profile)
            )
        };
        format!(
            "{action} · {} · {}",
            self.model.as_deref().unwrap_or("native model"),
            self.effort.as_deref().unwrap_or("native effort")
        )
    }
}

pub(super) fn copy_command(
    conversation: &Conversation,
    mode: OpenMode,
    extra: &[String],
    to_profile: Option<&str>,
) -> String {
    let mut command = format!(
        "rtr {} {} --tool {} --profile {}",
        mode.label(),
        runner::shell_quote(&conversation.id),
        runner::shell_quote(&conversation.tool),
        runner::shell_quote(&conversation.profile)
    );
    if mode == OpenMode::Fork {
        if let Some(profile) = to_profile {
            command.push_str(" --to-profile ");
            command.push_str(&runner::shell_quote(profile));
        }
    }
    if !extra.is_empty() {
        command.push_str(" -- ");
        command.push_str(
            &extra
                .iter()
                .map(|arg| runner::shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    command
}
