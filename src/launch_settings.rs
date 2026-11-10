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
