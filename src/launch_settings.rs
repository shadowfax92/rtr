//! Resolves native arguments before routing and finalizes settings for the
//! selected home. Wrapper commands and arguments after a native `--` are opaque.

use std::{collections::HashSet, path::Path};

use crate::session_status::SessionStatus;

/// Model and effort reported for one native invocation. The resolver retains
/// raw values; the picker's display-only constructor escapes terminal controls.
#[derive(Debug, Default)]
pub(crate) struct LaunchSettings {
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl LaunchSettings {
    pub fn from_args(tool: &str, args: &[String]) -> Self {
        let values = resolve(tool, &[], None, args).resolved.values;
        Self {
            model: values.model.as_deref().map(single_line),
            effort: values.effort.as_deref().map(single_line),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelOrigin {
    Configured,
    Conversation,
    Explicit,
    NativeDefault,
}

/// Destination-independent settings and the inputs needed if a Codex cache
/// rejects the inherited model. Routing can read the winning raw value here.
pub(crate) struct RequestedSettings<'a> {
    tool: &'a str,
    defaults: &'a [String],
    session_facts: Option<&'a SessionStatus>,
    runtime: &'a [String],
    pub resolved: ResolvedSettings,
}

/// The same arguments supply the report and the child. A conversation command
/// inserts its native identity at the original defaults/runtime boundary.
#[derive(Debug)]
pub(crate) struct ResolvedSettings {
    pub values: LaunchSettings,
    pub model_origin: ModelOrigin,
    args: Vec<String>,
    identity_at: usize,
    model_at: Option<usize>,
    unavailable_model: Option<String>,
}

impl RequestedSettings<'_> {
    pub fn needs_destination_cache(&self) -> bool {
        self.tool == "codex" && self.resolved.model_origin == ModelOrigin::Conversation
    }

    pub fn finalize(self, models_cache: &Path) -> ResolvedSettings {
        if self.needs_destination_cache()
            && self
                .resolved
                .values
                .model
                .as_deref()
                .is_some_and(|model| cache_rejects(models_cache, model))
        {
            let mut resolved = resolve_layers(
                self.tool,
                self.defaults,
                self.session_facts,
                self.runtime,
                false,
            );
            resolved.unavailable_model = self.resolved.values.model;
            return resolved;
        }
        self.resolved
    }
}

impl ResolvedSettings {
    /// Apply a Claude fallback at the winning option's original position.
    /// The resolver records that position before discarding duplicate flags,
    /// so wrapper arguments and literal model strings stay untouched.
    /// Apply the fallback before inserting the native fork or resume identity,
    /// which can shift the model option's position.
    pub fn replace_claude_model(&mut self, fallback: &str) {
        let index = self
            .model_at
            .expect("Fable route has a winning model option");
        if self.args[index] == "--model" {
            self.args[index + 1] = fallback.to_owned();
        } else {
            self.args[index] = format!("--model={fallback}");
        }
        self.values.model = Some(fallback.to_owned());
    }

    pub fn announce_conversation_model(&self) {
        if let Some(model) = &self.unavailable_model {
            eprintln!(
                "rtr: conversation model {} is absent from the destination model cache; using the configured default",
                single_line(model)
            );
        } else if self.model_origin == ModelOrigin::Conversation {
            eprintln!("rtr: requested model from conversation; override with -- --model MODEL");
        }
    }

    pub fn insert_identity(&mut self, args: Vec<String>) {
        self.args.splice(self.identity_at..self.identity_at, args);
    }

    pub fn into_args(self) -> Vec<String> {
        self.args
    }
}

pub(crate) fn resolve<'a>(
    tool: &'a str,
    defaults: &'a [String],
    session_facts: Option<&'a SessionStatus>,
    runtime: &'a [String],
) -> RequestedSettings<'a> {
    RequestedSettings {
        tool,
        defaults,
        session_facts,
        runtime,
        resolved: resolve_layers(tool, defaults, session_facts, runtime, true),
    }
}

pub(crate) fn merge_tool_args(tool: &str, defaults: &[String], runtime: &[String]) -> Vec<String> {
    resolve(tool, defaults, None, runtime).resolved.into_args()
}

fn cache_rejects(path: &Path, model: &str) -> bool {
    use std::os::unix::fs::OpenOptionsExt;

    // Forks finalize inside the rotation transaction. A special file in an
    // optional native cache path must not block launches for the entire tool.
    let Some(file) = std::fs::File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()
        .filter(|file| file.metadata().is_ok_and(|metadata| metadata.is_file()))
    else {
        return false;
    };
    let Ok(cache) = serde_json::from_reader::<_, serde_json::Value>(std::io::BufReader::new(file))
    else {
        return false;
    };
    cache["models"].as_array().is_some_and(|models| {
        let mut slugs = models
            .iter()
            .filter_map(|entry| entry["slug"].as_str())
            .filter(|slug| !slug.trim().is_empty());
        slugs
            .next()
            .is_some_and(|first| first != model && !slugs.any(|slug| slug == model))
    })
}

fn resolve_layers(
    tool: &str,
    defaults: &[String],
    session_facts: Option<&SessionStatus>,
    runtime: &[String],
    inherit_model: bool,
) -> ResolvedSettings {
    let mut inherited = Vec::new();
    if let Some(facts) = session_facts {
        if let Some(model) = facts.model.as_ref().filter(|_| inherit_model) {
            inherited.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &facts.effort {
            if tool == "claude" {
                inherited.extend(["--effort".into(), effort.clone()]);
            } else {
                let quoted = serde_json::Value::String(effort.clone());
                inherited.extend(["-c".into(), format!("model_reasoning_effort={quoted}")]);
            }
        }
    }

    // Synthesized settings go before a configured delimiter. Tokens supplied
    // after it, including runtime args, keep their literal meaning and order.
    let (defaults, default_literals) = defaults.split_at(
        defaults
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(defaults.len()),
    );
    let mut recognized = Vec::new();
    let mut offset = 0;
    let mut literal = false;
    for (source, origin) in [
        (defaults, ModelOrigin::Configured),
        (inherited.as_slice(), ModelOrigin::Conversation),
        (default_literals, ModelOrigin::Configured),
        (runtime, ModelOrigin::Explicit),
    ] {
        // Scan each layer independently so a dangling default cannot consume
        // a later layer's first token. A native delimiter ends every layer.
        let mut index = 0;
        while !literal && index < source.len() {
            if source[index] == "--" {
                literal = true;
            } else if let Some(arg) = native_arg(tool, source, index) {
                let len = arg.len;
                recognized.push((offset + index, origin, arg));
                index += len;
            } else {
                index += 1;
            }
        }
        offset += source.len();
    }

    let mut settings = LaunchSettings::default();
    let mut model_origin = ModelOrigin::NativeDefault;
    let mut model_at = None;
    let mut retained = vec![true; offset];
    let mut seen = HashSet::new();
    for (index, origin, arg) in recognized.into_iter().rev() {
        if seen.contains(&arg.key) {
            retained[index..index + arg.len].fill(false);
            continue;
        }
        match &arg.key {
            NativeArgKey::Model => {
                settings.model = arg.value;
                if settings.model.is_some() {
                    model_origin = origin;
                    model_at = Some(index);
                }
            }
            NativeArgKey::Effort => settings.effort = arg.value,
            _ => {}
        }
        seen.insert(arg.key);
    }

    ResolvedSettings {
        values: settings,
        model_origin,
        args: defaults
            .iter()
            .chain(&inherited)
            .chain(default_literals)
            .chain(runtime)
            .zip(&retained)
            .filter(|(_, keep)| **keep)
            .map(|(arg, _)| arg.clone())
            .collect(),
        identity_at: retained[..defaults.len()]
            .iter()
            .filter(|keep| **keep)
            .count(),
        model_at: model_at.map(|index| retained[..index].iter().filter(|keep| **keep).count()),
        unavailable_model: None,
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
enum NativeArgKey {
    Flag,
    Model,
    Effort,
    CodexConfig(String),
}

struct NativeArg {
    key: NativeArgKey,
    len: usize,
    value: Option<String>,
}

fn native_arg(tool: &str, args: &[String], index: usize) -> Option<NativeArg> {
    let argument = &args[index];
    let (key, attached) = argument
        .split_once('=')
        .map_or((argument.as_str(), None), |(key, value)| (key, Some(value)));
    let (key, attached) = match (tool, key) {
        ("codex", key) if key.starts_with("-m") && key != "-m" => {
            ("-m", argument.strip_prefix("-m"))
        }
        ("codex", key) if key.starts_with("-c") && key != "-c" => {
            ("-c", argument.strip_prefix("-c"))
        }
        _ => (key, attached),
    };
    let (value, len) = if let Some(value) = attached {
        (Some(value), 1)
    } else if let Some(value) = args.get(index + 1).filter(|arg| arg.as_str() != "--") {
        (Some(value.as_str()), 2)
    } else {
        (None, 1)
    };

    let (key, len, value) = match (tool, key) {
        ("claude", "--dangerously-skip-permissions")
        | ("codex", "--dangerously-bypass-approvals-and-sandbox")
            if attached.is_none() =>
        {
            (NativeArgKey::Flag, 1, None)
        }
        ("claude", "--model") | ("codex", "-m" | "--model") => {
            (NativeArgKey::Model, len, value.map(str::to_owned))
        }
        ("claude", "--effort") => (NativeArgKey::Effort, len, value.map(str::to_owned)),
        ("codex", "-c" | "--config") => {
            let (name, value) = value?.split_once('=')?;
            match name.trim() {
                "" => return None,
                "model" => (NativeArgKey::Model, len, Some(decode_config_string(value))),
                "model_reasoning_effort" => {
                    (NativeArgKey::Effort, len, Some(decode_config_string(value)))
                }
                name => (NativeArgKey::CodexConfig(name.to_owned()), len, None),
            }
        }
        _ => return None,
    };
    Some(NativeArg { key, len, value })
}

fn decode_config_string(value: &str) -> String {
    let value = value.trim();
    format!("value = {value}")
        .parse::<toml::Table>()
        .ok()
        .and_then(|table| table.get("value")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| value.to_owned())
}

/// Keep untrusted argument values on one line without terminal control bytes.
pub(crate) fn single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}
