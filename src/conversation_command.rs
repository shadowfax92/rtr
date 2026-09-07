//! CLI adapter for the native catalog and interactive session picker.
//! Noninteractive output stays independent of terminal rendering and indexing.

use anyhow::{Context, Result};

use crate::cli::{ConversationOpenArgs, SessionsArgs};
use crate::conversations::{
    self, Catalog, Conversation, ConversationKey, ConversationQuery, OpenMode,
};
use crate::paths::Paths;
use crate::picker::{self, Options};

pub async fn run_sessions(paths: &Paths, args: SessionsArgs) -> Result<i32> {
    if args.json || args.list {
        let catalog = conversations::query(paths, &query_for(&args)?)?;
        if args.json {
            println!("{}", render_json(&catalog)?);
        } else {
            print!("{}", render_list(&catalog.conversations));
            report_diagnostics(&catalog);
        }
        return Ok(0);
    }
    let options = Options {
        query: args.query,
        tool: args.tool,
        profile: args.profile,
        here: args.here,
        mode: OpenMode::Fork,
        to_profile: None,
        extra_args: Vec::new(),
    };
    let Some((conversation, mode)) = picker::run(paths, options)? else {
        return Ok(0);
    };
    conversations::open(paths, &conversation, mode, &[]).await
}

pub async fn run_open(
    paths: &Paths,
    args: ConversationOpenArgs,
    mode: OpenMode,
    to_profile: Option<&str>,
) -> Result<i32> {
    // Exact selectors retain their fast, noninteractive launch path. Only an
    // omitted or ambiguous selector enters the terminal picker.
    if let Some(selector) = args.selector.as_deref() {
        let catalog = conversations::query(
            paths,
            &query_for(&SessionsArgs {
                tool: args.tool.clone(),
                profile: args.profile.clone(),
                here: args.here,
                query: None,
                list: false,
                json: false,
            })?,
        )?;
        let matches = conversations::matches_selector(&catalog, selector)?;
        if matches.len() == 1 {
            return if mode == OpenMode::Fork {
                conversations::fork(paths, matches[0], to_profile, &args.args).await
            } else {
                conversations::open(paths, matches[0], mode, &args.args).await
            };
        }
    }
    let options = Options {
        query: args.selector,
        tool: args.tool,
        profile: args.profile,
        here: args.here,
        mode,
        to_profile: to_profile.map(str::to_string),
        extra_args: args.args.clone(),
    };
    let Some((conversation, mode)) = picker::run(paths, options)? else {
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
        query = query.with_cwd(std::env::current_dir()?);
    }
    Ok(query)
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
