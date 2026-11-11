//! CLI adapter for the native catalog and interactive session picker.
//! Lists share the persistent index while remaining independent of rendering.

use anyhow::{Context, Result};
use std::io::IsTerminal;

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
        include_non_conversations: args.include_non_conversations,
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
    run_open_with(paths, args, mode, to_profile, None).await
}

pub async fn run_fork(paths: &Paths, options: crate::cli::ConversationForkArgs) -> Result<i32> {
    // A watcher reusing its report path must never mistake an earlier move for
    // this attempt. A new result is published atomically only after preparation.
    if let Some(path) = &options.report {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("removing the previous fork report"),
        }
    }
    let result = run_open_with(
        paths,
        options.source.clone(),
        OpenMode::Fork,
        options.to_profile.as_deref(),
        Some(&options),
    )
    .await;
    if let Err(error) = &result {
        if let Some(unavailable) = error.downcast_ref::<crate::quota::NoUsableProfiles>() {
            let json = serde_json::to_string(unavailable)?;
            if options.dry_run {
                println!("{json}");
            }
            if let Some(path) = &options.report {
                crate::file_lock::write_private_atomic(path, &json)?;
            }
        }
    }
    result
}

async fn run_open_with(
    paths: &Paths,
    args: ConversationOpenArgs,
    mode: OpenMode,
    to_profile: Option<&str>,
    fork_options: Option<&crate::cli::ConversationForkArgs>,
) -> Result<i32> {
    let query = query_for(&SessionsArgs {
        tool: args.tool.clone(),
        profile: args.profile.clone(),
        here: args.here,
        query: None,
        list: false,
        json: false,
        include_non_conversations: args.include_non_conversations,
    })?;
    let mut picker_query = args.selector.clone();
    let mut here = args.here;
    let mut include_non_conversations = args.include_non_conversations;
    if let Some(selector) = args.selector.as_deref() {
        let lookup = conversations::lookup_id(paths, selector, &query)?;
        let is_id = conversations::looks_like_id(selector);
        if !lookup.catalog.conversations.is_empty() {
            include_non_conversations = true;
        }
        let catalog = if !lookup.catalog.conversations.is_empty() || is_id {
            &lookup.catalog
        } else {
            // Native names remain exact catalog selectors. A filename hit never
            // enters this expensive path, even for historical opaque IDs.
            &conversations::query(paths, &query)?
        };
        let matches = conversations::matches_selector(catalog, selector)?;
        if matches.len() == 1 {
            return if mode == OpenMode::Fork {
                conversations::fork_with_options(
                    paths,
                    matches[0],
                    to_profile,
                    &args.args,
                    fork_options,
                )
                .await
            } else {
                conversations::open(paths, matches[0], mode, &args.args).await
            };
        }
        if matches.is_empty()
            && !is_id
            && lookup.catalog.conversations.is_empty()
            && !lookup.catalog.diagnostics.is_empty()
        {
            anyhow::bail!(
                "could not open conversation '{selector}': {}",
                lookup.catalog.diagnostics.join("; ")
            );
        }
        if matches.is_empty() && is_id {
            let message =
                conversations::missing_id_message(selector, &lookup, args.tool.as_deref())?;
            if fork_options.is_some_and(|o| o.no_picker || o.dry_run)
                || !std::io::stdin().is_terminal()
                || !std::io::stdout().is_terminal()
            {
                anyhow::bail!("{message}");
            }
            eprintln!("rtr: {message}. Opening the picker in the current directory.");
            picker_query = None;
            here = true;
        }
    }
    if fork_options.is_some_and(|o| o.no_picker || o.dry_run) {
        anyhow::bail!("session selector is missing, unknown, or ambiguous; picker disabled");
    }
    let options = Options {
        query: picker_query,
        tool: args.tool,
        profile: args.profile,
        here,
        include_non_conversations,
        mode,
        to_profile: to_profile.map(str::to_string),
        extra_args: args.args.clone(),
    };
    let Some((conversation, mode)) = picker::run(paths, options)? else {
        return Ok(0);
    };
    if mode == OpenMode::Fork {
        conversations::fork_with_options(paths, &conversation, to_profile, &args.args, fork_options)
            .await
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
    let mut query = ConversationQuery::all().with_non_conversations(args.include_non_conversations);
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
            conversation
                .updated_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M"),
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
