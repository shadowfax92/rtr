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
        let mut model = None;
        let mut config_model = None;
        let mut effort = None;
        let mut index = 0;
        while index < args.len() {
            let argument = &args[index];
            if argument == "--" {
                break;
            }
            let (key, value, consumed) = if let Some((key, value)) = argument.split_once('=') {
                (key, Some(value), 1)
            } else if matches!(
                argument.as_str(),
                "-m" | "--model" | "--effort" | "-c" | "--config"
            ) {
                (
                    argument.as_str(),
                    args.get(index + 1).map(String::as_str),
                    2,
                )
            } else {
                (argument.as_str(), None, 1)
            };
            if let Some(value) = value {
                match key {
                    "-m" | "--model" => model = Some(clean(value)),
                    "--effort" => effort = Some(clean(value)),
                    "-c" | "--config" if conversation.tool == "codex" => {
                        if let Some((name, value)) = value.split_once('=') {
                            let decoded = format!("value = {}", value.trim())
                                .parse::<toml::Table>()
                                .ok()
                                .and_then(|v| {
                                    v.get("value").and_then(|v| v.as_str()).map(str::to_string)
                                })
                                .unwrap_or_else(|| value.trim().to_string());
                            match name.trim() {
                                "model" => config_model = Some(clean(&decoded)),
                                "model_reasoning_effort" => effort = Some(clean(&decoded)),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            index += consumed;
        }
        Self {
            model: model.or(config_model),
            effort,
            arguments: clean(
                &args
                    .iter()
                    .map(|arg| runner::shell_quote(arg))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        }
    }

    pub fn line(&self, conversation: &Conversation, mode: OpenMode) -> String {
        format!(
            "{} with {}/{} · {} · {}",
            if mode == OpenMode::Fork {
                "Fork"
            } else {
                "Resume"
            },
            clean(&conversation.tool),
            clean(&conversation.profile),
            self.model.as_deref().unwrap_or("native model"),
            self.effort.as_deref().unwrap_or("native effort")
        )
    }
}

pub(super) fn copy_command(
    conversation: &Conversation,
    mode: OpenMode,
    extra: &[String],
) -> String {
    let mut command = format!(
        "rtr {} {} --tool {} --profile {}",
        mode.label(),
        runner::shell_quote(&conversation.id),
        runner::shell_quote(&conversation.tool),
        runner::shell_quote(&conversation.profile)
    );
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
