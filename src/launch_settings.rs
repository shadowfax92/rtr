//! Display-only settings shared by the startup summary and conversation picker.
//! Read the runner's merged arguments, not layered native configuration: the
//! child can still select its own default or restore settings from a session.

/// Requested model and effort after argument merging, sanitized for display.
pub(crate) struct LaunchSettings {
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl LaunchSettings {
    pub fn from_args(tool: &str, args: &[String]) -> Self {
        let mut model = None;
        let mut config_model = None;
        let mut effort = None;
        let mut index = 0;
        while index < args.len() {
            let argument = &args[index];
            if argument == "--" {
                break;
            }
            // Option arity belongs to the native tool: Claude's -c means
            // continue and takes no value, while Codex's -c consumes a setting.
            let (key, value, consumed) = if let Some((key, value)) = argument.split_once('=') {
                (key, Some(value), 1)
            } else if matches!(
                (tool, argument.as_str()),
                ("claude", "--model" | "--effort")
                    | ("codex", "-m" | "--model" | "-c" | "--config")
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
                match (tool, key) {
                    ("claude", "--model") | ("codex", "-m" | "--model") => {
                        model = Some(single_line(value));
                    }
                    ("claude", "--effort") => effort = Some(single_line(value)),
                    ("codex", "-c" | "--config") => {
                        if let Some((name, value)) = value.split_once('=') {
                            let decoded = format!("value = {}", value.trim())
                                .parse::<toml::Table>()
                                .ok()
                                .and_then(|v| {
                                    v.get("value").and_then(|v| v.as_str()).map(str::to_string)
                                })
                                .unwrap_or_else(|| value.trim().to_string());
                            match name.trim() {
                                "model" => config_model = Some(single_line(&decoded)),
                                "model_reasoning_effort" => effort = Some(single_line(&decoded)),
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
        }
    }
}

/// Keep untrusted argument values on one line without terminal control bytes.
pub(crate) fn single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// Restore conversation settings between configured defaults and explicit argv.
/// Only a readable native model cache can reject an inherited Codex model; user
/// overrides are always passed through, even if the cache cannot serve them.
pub(crate) fn inherit(
    paths: &crate::paths::Paths,
    conversation: &crate::conversations::Conversation,
    target: &str,
    runtime: &[String],
    announce: bool,
) -> anyhow::Result<(Vec<String>, LaunchSettings)> {
    let facts = crate::session_status::for_conversation(paths, conversation)?;
    let config = crate::config::Config::load(&paths.config_file())?;
    let defaults = &config.tool(&conversation.tool)?.args;
    let explicit = LaunchSettings::from_args(&conversation.tool, runtime);
    let mut inherited = Vec::new();
    let mut model = facts.model;
    if conversation.tool == "codex" && explicit.model.is_none() {
        let cache = paths
            .profile_home_dir("codex", target)
            .join("models_cache.json");
        if let Some(models) = std::fs::read(cache)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["models"].as_array().cloned())
        {
            if !models.is_empty()
                && model.as_ref().is_some_and(|m| {
                    !models
                        .iter()
                        .any(|v| v["slug"].as_str() == Some(m.as_str()))
                })
            {
                if announce {
                    eprintln!("rtr: conversation model {} is absent from the destination model cache; using the configured default", single_line(model.as_deref().unwrap()));
                }
                model = None;
            }
        }
    }
    if explicit.model.is_none() {
        if let Some(model) = model {
            inherited.extend(["--model".into(), model]);
            if announce {
                eprintln!("rtr: model from conversation; override with -- --model MODEL");
            }
        }
    }
    if explicit.effort.is_none() {
        if let Some(effort) = facts.effort {
            if conversation.tool == "claude" {
                inherited.extend(["--effort".into(), effort]);
            } else {
                inherited.extend([
                    "-c".into(),
                    format!("model_reasoning_effort={}", serde_json::to_string(&effort)?),
                ]);
            }
        }
    }
    let args = crate::runner::merge_tool_args(&conversation.tool, &inherited, runtime);
    let merged = crate::runner::merge_tool_args(&conversation.tool, defaults, &args);
    Ok((args, LaunchSettings::from_args(&conversation.tool, &merged)))
}
