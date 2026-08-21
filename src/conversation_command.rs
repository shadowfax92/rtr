//! CLI adapter for the cross-profile conversation catalog.
//!
//! Discovery, identity, inspection, and native launch semantics stay in
//! `conversations`; this module owns terminal presentation and the fzf protocol.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
#[cfg(test)]
use chrono::{DateTime, Utc};
#[cfg(test)]
use std::path::PathBuf;

use crate::cli::{ConversationOpenArgs, SessionsArgs};
use crate::conversations::{
    self, Catalog, Conversation, ConversationKey, ConversationQuery, OpenMode,
};
use crate::paths::Paths;

pub async fn run_sessions(paths: &Paths, args: SessionsArgs) -> Result<i32> {
    let catalog = conversations::query(paths, &query_for(&args)?)?;
    if args.json {
        println!("{}", render_json(&catalog)?);
        return Ok(0);
    }
    if args.list {
        print!("{}", render_list(&catalog.conversations));
        report_diagnostics(&catalog);
        return Ok(0);
    }

    let Some((conversation, mode)) = pick(&catalog, args.query.as_deref())? else {
        return Ok(0);
    };
    conversations::open(paths, &conversation, mode, &[]).await
}

pub async fn run_open(
    paths: &Paths,
    args: ConversationOpenArgs,
    default_mode: OpenMode,
) -> Result<i32> {
    let sessions_args = SessionsArgs {
        tool: args.tool,
        profile: args.profile,
        here: args.here,
        query: args.selector.clone(),
        list: false,
        json: false,
    };
    let catalog = conversations::query(paths, &query_for(&sessions_args)?)?;
    let selected = if let Some(selector) = args.selector.as_deref() {
        let matches = conversations::matches_selector(&catalog, selector)?;
        if matches.len() == 1 {
            Some((matches[0].clone(), default_mode))
        } else {
            pick(&catalog, Some(selector))?
        }
    } else {
        pick(&catalog, None)?
    };
    let Some((conversation, mode)) = selected else {
        return Ok(0);
    };
    conversations::open(paths, &conversation, mode, &args.args).await
}

pub fn print_preview(paths: &Paths, encoded_key: &str) -> Result<()> {
    let key = ConversationKey::decode(encoded_key)?;
    print!("{}", conversations::inspect(paths, &key)?);
    Ok(())
}

fn query_for(args: &SessionsArgs) -> Result<ConversationQuery> {
    let mut query = ConversationQuery::all();
    if let Some(tool) = &args.tool {
        query = query.with_tool(tool);
    }
    if let Some(profile) = &args.profile {
        query = query.with_profile(profile);
    }
    if args.here {
        query = query.with_cwd(
            std::env::current_dir().context("resolving the current directory for --here")?,
        );
    }
    Ok(query)
}

fn pick(
    catalog: &Catalog,
    initial_query: Option<&str>,
) -> Result<Option<(Conversation, OpenMode)>> {
    if catalog.conversations.is_empty() {
        bail!("no matching Claude or Codex conversations found");
    }
    let fzf = std::env::var_os("RTR_FZF").unwrap_or_else(|| "fzf".into());
    let executable = std::env::current_exe().context("resolving the rtr executable")?;
    let preview = format!(
        "{} conversation-preview {{1}}",
        crate::runner::shell_quote(&executable.display().to_string())
    );
    let mut command = Command::new(fzf);
    command
        .args([
            "--delimiter=\t",
            "--with-nth=2..",
            "--accept-nth=1",
            "--expect=ctrl-r,ctrl-f",
            "--no-multi",
            "--scheme=history",
            "--layout=reverse",
            "--preview-window=right,60%,wrap",
            "--header=Enter: fork  Ctrl-R: resume  Ctrl-F: fork",
            "--prompt=conversations> ",
            "--preview",
            &preview,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(initial_query) = initial_query {
        command.arg("--query").arg(initial_query);
    }

    let mut child = command
        .spawn()
        .with_context(|| "launching fzf (install fzf or set RTR_FZF to its executable path)")?;
    let records = picker_records(&catalog.conversations);
    let write_result = child
        .stdin
        .take()
        .context("opening fzf stdin")?
        .write_all(records.as_bytes());
    if let Err(error) = write_result {
        // fzf closes its input as soon as the user cancels. A large catalog can
        // still be in flight at that moment; its exit status below owns whether
        // this was a normal cancellation or a real failure.
        if error.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(error).context("writing the conversation catalog to fzf");
        }
    }
    let output = child.wait_with_output().context("waiting for fzf")?;
    if matches!(output.status.code(), Some(1 | 130)) {
        return Ok(None);
    }
    if !output.status.success() {
        bail!("fzf exited with {}", output.status);
    }
    let output = String::from_utf8(output.stdout).context("fzf returned non-UTF-8 output")?;
    let (pressed, encoded_key) = output
        .split_once('\n')
        .context("fzf returned no selected conversation")?;
    let encoded_key = encoded_key.trim_end_matches(['\r', '\n']);
    if encoded_key.is_empty() {
        return Ok(None);
    }
    let key = ConversationKey::decode(encoded_key)?;
    let conversation = catalog
        .conversations
        .iter()
        .find(|conversation| ConversationKey::from(*conversation) == key)
        .cloned()
        .context("the selected conversation disappeared from the catalog")?;
    let mode = match pressed.trim_end_matches('\r') {
        "ctrl-r" => OpenMode::Resume,
        "ctrl-f" => OpenMode::Fork,
        "" => OpenMode::Fork,
        key => bail!("fzf returned unsupported key '{key}'"),
    };
    Ok(Some((conversation, mode)))
}

fn picker_records(conversations: &[Conversation]) -> String {
    let mut output = String::new();
    for conversation in conversations {
        let key = ConversationKey::from(conversation).encode();
        let status = match (conversation.enabled, conversation.bypass) {
            (false, _) => "disabled",
            (_, true) => "bypassed",
            _ => "ready",
        };
        let fields = [
            key,
            display_field(&conversation.tool),
            display_field(&conversation.profile),
            conversation.updated_at.format("%Y-%m-%d %H:%M").to_string(),
            display_field(&conversation.title),
            display_field(conversation.first_prompt.as_deref().unwrap_or("")),
            display_field(&conversation.cwd.display().to_string()),
            display_field(&conversation.id),
            status.to_string(),
        ];
        output.push_str(&fields.join("\t"));
        output.push('\n');
    }
    output
}

fn render_list(conversations: &[Conversation]) -> String {
    if conversations.is_empty() {
        return "No matching Claude or Codex conversations found.\n".to_string();
    }
    let mut output = String::new();
    for conversation in conversations {
        output.push_str(&format!(
            "{}  {}/{}  {}  {}\n    {}\n",
            conversation.updated_at.format("%Y-%m-%d %H:%M"),
            conversation.tool,
            conversation.profile,
            conversation.title,
            conversation.id,
            conversation.cwd.display()
        ));
    }
    output
}

fn render_json(catalog: &Catalog) -> Result<String> {
    let conversations = catalog
        .conversations
        .iter()
        .map(|conversation| {
            serde_json::json!({
                "key": ConversationKey::from(conversation).encode(),
                "tool": conversation.tool,
                "profile": conversation.profile,
                "id": conversation.id,
                "native_name": conversation.native_name,
                "title": conversation.title,
                "first_prompt": conversation.first_prompt,
                "cwd": conversation.cwd,
                "started_at": conversation.started_at.map(|value| value.to_rfc3339()),
                "updated_at": conversation.updated_at.to_rfc3339(),
                "enabled": conversation.enabled,
                "bypass": conversation.bypass,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&serde_json::json!({
        "schema_version": 1,
        "conversations": conversations,
        "diagnostics": catalog.diagnostics,
    }))
    .context("serializing the conversation catalog")
}

fn report_diagnostics(catalog: &Catalog) {
    if !catalog.diagnostics.is_empty() {
        eprintln!(
            "rtr: skipped {} malformed or unreadable conversation record(s)",
            catalog.diagnostics.len()
        );
    }
}

fn display_field(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character == '\t' || character == '\r' || character == '\n' {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(title: &str) -> Conversation {
        Conversation {
            tool: "codex".into(),
            profile: "eng".into(),
            id: "thread-id".into(),
            native_name: Some(title.into()),
            title: title.into(),
            first_prompt: Some("first\tprompt".into()),
            cwd: PathBuf::from("/work\tproject"),
            started_at: None,
            updated_at: DateTime::parse_from_rfc3339("2026-08-20T18:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            enabled: true,
            bypass: false,
            transcript_path: PathBuf::from("/transcript"),
        }
    }

    #[test]
    fn picker_record_hides_identity_and_sanitizes_searchable_fields() {
        let record = picker_records(&[conversation("release\nwork")]);
        let fields = record.trim_end().split('\t').collect::<Vec<_>>();

        assert_eq!(fields.len(), 9);
        assert!(fields[0].starts_with("v1:codex:"));
        assert_eq!(fields[4], "release work");
        assert_eq!(fields[5], "first prompt");
        assert_eq!(fields[6], "/work project");
    }
}
