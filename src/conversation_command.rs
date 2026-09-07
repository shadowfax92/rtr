//! CLI adapter for the cross-profile conversation catalog.
//!
//! Discovery, identity, inspection, and native launch semantics stay in
//! `conversations`; this module owns terminal presentation and the fzf protocol.

use std::io::{self, Write};
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

    let Some((conversation, mode)) = pick(&catalog, args.query.as_deref(), false)? else {
        return Ok(0);
    };
    conversations::open(paths, &conversation, mode, &[]).await
}

pub async fn run_open(
    paths: &Paths,
    args: ConversationOpenArgs,
    default_mode: OpenMode,
    to_profile: Option<&str>,
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
            pick(&catalog, Some(selector), to_profile.is_some())?
        }
    } else {
        pick(&catalog, None, to_profile.is_some())?
    };
    let Some((conversation, mode)) = selected else {
        return Ok(0);
    };
    if mode == OpenMode::Fork {
        conversations::fork(paths, &conversation, to_profile, &args.args).await
    } else {
        conversations::open(paths, &conversation, mode, &args.args).await
    }
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
    fork_only: bool,
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
            // fzf cannot match columns removed by --with-nth. The complete
            // transcript therefore remains after the display metadata in the
            // transformed row, while no-hscroll keeps that search-only tail
            // from displacing the stable session summary on screen.
            "--with-nth=2..",
            "--accept-nth=1",
            "--no-multi",
            "--no-hscroll",
            "--scheme=history",
            "--layout=reverse",
            "--height=90%",
            "--min-height=18",
            "--border=rounded",
            "--border-label= rtr conversations ",
            "--highlight-line",
            "--info=inline-right",
            "--cycle",
            "--preview-label= latest transcript messages ",
            "--preview-window=right,55%,border-left,wrap,<50(down,50%,border-top,wrap)",
            "--bind=alt-p:toggle-preview,ctrl-u:preview-half-page-up,ctrl-d:preview-half-page-down",
            "--prompt=conversations> ",
            "--preview",
            &preview,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    // A destination override is a fork request. Do not let the picker silently
    // discard that account choice by interpreting Ctrl-R as an in-place resume.
    command.args(if fork_only {
        ["--expect=ctrl-f", "--header=Enter: fork to selected profile  Ctrl-U/D: scroll preview  Alt-P: toggle preview"]
    } else {
        ["--expect=ctrl-r,ctrl-f", "--header=Enter: fork (next profile)  Ctrl-R: resume  Ctrl-F: fork  Ctrl-U/D: scroll preview  Alt-P: toggle preview"]
    });
    if let Some(initial_query) = initial_query {
        command.arg("--query").arg(initial_query);
    }

    let mut child = command
        .spawn()
        .with_context(|| "launching fzf (install fzf or set RTR_FZF to its executable path)")?;
    // Index one transcript at a time into fzf's pipe instead of materializing a
    // second copy of the complete cross-profile corpus in RTR. Closing stdin is
    // the handoff that tells fzf the asynchronously consumed catalog is final.
    let write_result = {
        let mut stdin = child.stdin.take().context("opening fzf stdin")?;
        let result = write_picker_records(&mut stdin, &catalog.conversations);
        drop(stdin);
        result
    };
    let index_diagnostics = match write_result {
        Ok(diagnostics) => diagnostics,
        // fzf closes its input as soon as the user cancels. Its exit status
        // below owns whether that early close was cancellation or a failure.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Vec::new(),
        Err(error) => {
            // A pipe failure cannot produce a trustworthy selection. Reap the
            // child here so the failed picker never survives its RTR parent.
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("writing the conversation catalog to fzf");
        }
    };
    let output = child.wait_with_output().context("waiting for fzf")?;
    if matches!(output.status.code(), Some(1 | 130)) {
        return Ok(None);
    }
    if !output.status.success() {
        bail!("fzf exited with {}", output.status);
    }
    if !index_diagnostics.is_empty() {
        eprintln!(
            "rtr: full-text search skipped {} unreadable transcript(s)",
            index_diagnostics.len()
        );
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
        "ctrl-r" if fork_only => bail!("cannot resume when --to-profile requests a fork"),
        "ctrl-r" => OpenMode::Resume,
        "ctrl-f" => OpenMode::Fork,
        "" => OpenMode::Fork,
        key => bail!("fzf returned unsupported key '{key}'"),
    };
    Ok(Some((conversation, mode)))
}

fn write_picker_records<W: Write>(
    writer: &mut W,
    conversations: &[Conversation],
) -> io::Result<Vec<String>> {
    let mut diagnostics = Vec::new();
    for conversation in conversations {
        let search_text = match conversations::searchable_transcript_text(conversation) {
            Ok(search_text) => search_text,
            Err(error) => {
                diagnostics.push(format!(
                    "{}: {error:#}",
                    conversation.transcript_path.display()
                ));
                String::new()
            }
        };
        write_picker_record(writer, conversation, &search_text)?;
    }
    Ok(diagnostics)
}

fn write_picker_record<W: Write>(
    writer: &mut W,
    conversation: &Conversation,
    search_text: &str,
) -> io::Result<()> {
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
    writer.write_all(fields.join("\t").as_bytes())?;
    writer.write_all(b"\t")?;
    // searchable_transcript_text guarantees a single control-free field. Write
    // it directly so RTR does not allocate another copy of a long conversation.
    debug_assert!(!search_text.chars().any(char::is_control));
    writer.write_all(search_text.as_bytes())?;
    writer.write_all(b"\n")
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
            if character.is_control() {
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
        let mut record = Vec::new();
        write_picker_record(
            &mut record,
            &conversation("release\nwork"),
            "complete dialogue needle",
        )
        .unwrap();
        let record = String::from_utf8(record).unwrap();
        let fields = record
            .trim_end_matches('\n')
            .split('\t')
            .collect::<Vec<_>>();

        assert_eq!(fields.len(), 10);
        assert!(fields[0].starts_with("v1:codex:"));
        assert_eq!(fields[4], "release work");
        assert_eq!(fields[5], "first prompt");
        assert_eq!(fields[6], "/work project");
        assert_eq!(fields[9], "complete dialogue needle");
    }

    #[test]
    fn picker_records_append_complete_transcript_dialogue() {
        let temp = tempfile::tempdir().unwrap();
        let transcript = temp.path().join("rollout-search.jsonl");
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "answer found only in the full transcript"
                        }]
                    }
                })
            ),
        )
        .unwrap();
        let mut conversation = conversation("release work");
        conversation.transcript_path = transcript;
        let mut record = Vec::new();

        let diagnostics = write_picker_records(&mut record, &[conversation]).unwrap();
        let record = String::from_utf8(record).unwrap();
        let fields = record
            .trim_end_matches('\n')
            .split('\t')
            .collect::<Vec<_>>();

        assert!(diagnostics.is_empty());
        assert_eq!(fields.len(), 10);
        assert_eq!(fields[9], "answer found only in the full transcript");
    }
}
