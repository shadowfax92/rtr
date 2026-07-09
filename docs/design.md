# Design

## Goal

rtr launches Claude Code and Codex with complete, isolated subscription state
while keeping the underlying CLIs in charge of authentication, settings,
sessions, terminal behavior, and runtime arguments.

## Native Homes Are the Identity Boundary

One subscription profile owns one native tool home:

- Codex: `CODEX_HOME=<state>/homes/codex/<profile>`
- Claude: `CLAUDE_CONFIG_DIR=<state>/homes/claude/<profile>`

This boundary includes more than an access token. It keeps refresh state,
account metadata, tool settings, sessions, and tool-owned files together. rtr
does not interpret or copy those files.

## Launch Flow

1. Load strict tool and profile configuration.
2. Resolve an explicit profile or choose the next enabled profile.
3. Create and validate its owner-only native home.
4. Refresh the tool's skills into that home under a lock.
5. Persist the automatic cursor only after preparation succeeds.
6. Spawn the configured command with the native-home variable and runtime args.
7. Map the child status to a shell exit code and append a usage event.

Forced selection does not update automatic rotation. A spawn error still emits
a usage event with no exit code, making failed launch attempts visible without
inventing a child result.

## Terminal Ownership

The child inherits stdin, stdout, and stderr. This preserves full-screen TUI
behavior, prompts, and output formatting. While waiting, rtr catches common
Unix terminal signals and forwards them to the direct child so it can finish
and produce a real status before usage is recorded. rtr also sets
`kill_on_drop` for cancellation paths.

## Skills Refresh

The selected home receives a fresh `skills` tree on every launch. rtr copies to
a temporary sibling, replaces the destination only after a complete copy, and
serializes concurrent refreshes with a profile-local lock.

An explicit missing source is an error and leaves the previous tree intact. A
missing default source removes a stale destination because the user's global
tool home is the source of truth for default skills.

## State and Concurrency

Config remains hand-editable and is never rewritten during launch. Mutable
round-robin cursors live in `state.toml`; usage events live in `usage.jsonl`.
Both use advisory locks, and state replacement is atomic.

Native-home path segments are encoded before joining. Directory creation
rejects symlink components and tightens permissions to `0700`.

## Configuration Philosophy

The schema contains only behavior the launcher uses: commands, skills sources,
profiles, and profile enablement. Unknown fields fail deserialization so stale
or misspelled settings cannot look active while doing nothing.
