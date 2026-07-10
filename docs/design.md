# Design

## Goal

rtr launches Claude Code and Codex with complete, isolated subscription state
while keeping the underlying CLIs in charge of authentication, settings,
sessions, terminal behavior, and runtime arguments.

## Native Homes Are the Identity Boundary

One subscription profile owns one native tool home:

- Codex: `CODEX_HOME=<state>/homes/codex/<profile>`
- Claude: `CLAUDE_CONFIG_DIR=<state>/homes/claude/<profile>`
- Claude secure storage: `CLAUDE_SECURESTORAGE_CONFIG_DIR=<same profile home>`

This boundary includes more than an access token. It keeps refresh state,
account metadata, tool settings, sessions, and tool-owned files together. rtr
does not interpret or copy those files.

Claude's config directory owns user settings, app state, session history,
plugins, and side-by-side account context. Claude Code 2.1.205 also lets the
secure-storage path qualify the macOS Keychain service, so rtr pins both Claude
variables to the selected native boundary without accessing credentials.

## Launch Flow

1. Load strict tool and profile configuration.
2. Resolve an explicit profile or choose the next enabled profile.
3. Create and validate its owner-only native home.
4. Refresh the tool's skills into that home under a lock.
5. Persist the automatic cursor only after preparation succeeds.
6. Spawn the configured command with the native-home variable and runtime args.
7. Map the child status to a shell exit code and append a usage event.

`rtr add` first serializes a duplicate check and profile-table update under the
config lock, then enters the same launch flow with the new profile forced for
the sign-in run. Duplicate adds do not touch the existing home or skills.

`rtr fix` validates an existing profile, removes only its recognized stale
credential lock, and enters the same preparation and child-execution flow
without profile selection. This lets even a disabled profile re-authenticate in
place while leaving its settings, sessions, and rotation cursor unchanged.

`rtr rm` validates and confirms the exact encoded native-home path before
mutation. It removes only that profile's TOML table with a lossless editor under
the config lock, then recursively deletes that one real directory. Config-first
ordering favors recoverable orphaned files over credential loss if deletion
fails.

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

Claude supports symlinked skill directories. External relative links are
rebased to remain valid from the copied profile tree without resolving later
alias changes. Internal and dangling relative links stay verbatim. Codex links
use the same relocation policy.

Codex keeps `HOME` and the working directory, so native personal, repository,
and admin skill roots remain discoverable. rtr copies only a distinct legacy or
configured root into `$CODEX_HOME/skills`, excludes source `.system`, preserves
Codex's generated `.system` cache, and rolls back if staged installation fails.

## Verified Claude Code Contract

- [Environment variables](https://code.claude.com/docs/en/env-vars) documents
  `CLAUDE_CONFIG_DIR` as the override for user settings, session history,
  plugins, and non-macOS credential files.
- [The `.claude` directory](https://code.claude.com/docs/en/claude-directory)
  separates user state from project `.claude/*` state.
- [Skills](https://code.claude.com/docs/en/slash-commands) documents personal
  skills and symlinked skill-directory support.
- [Authentication](https://code.claude.com/docs/en/team) documents macOS
  Keychain storage and config-directory credential files on Linux and Windows.

## State and Concurrency

Config remains hand-editable and is never rewritten during launch. Mutable
round-robin cursors live in `state.toml`; usage events live in `usage.jsonl`.
Both use advisory locks, and state replacement is atomic.

Profile-table removal preserves unrelated comments, formatting, and quoted
keys. A cursor larger than the remaining enabled profile count is normalized by
the selector's modulo operation, so profile deletion needs no state rewrite.

Native-home path segments are encoded before joining. Directory creation
rejects symlink components and tightens permissions to `0700`.

## Configuration Philosophy

The schema contains only behavior the launcher uses: commands, skills sources,
profiles, and profile enablement. Unknown fields fail deserialization so stale
or misspelled settings cannot look active while doing nothing.
