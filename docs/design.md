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
does not modify or copy those files; conversation discovery reads their native
metadata and leaves ownership with the tool.

Claude's config directory owns user settings, app state, session history,
plugins, and side-by-side account context. Claude Code 2.1.205 also lets the
secure-storage path qualify the macOS Keychain service, so rtr pins both Claude
variables to the selected native boundary without accessing credentials.

## Launch Flow

1. Load strict tool and profile configuration.
2. Resolve the requested native settings and determine the candidate profiles.
3. Refresh stale or missing usage for automatic candidates through their own native logins.
4. Evaluate model eligibility and reserve the selected profile.
5. Prepare its owner-only native home and refresh skills under a lock.
6. Save automatic rotation only after preparation succeeds.
7. Spawn the configured command with the native-home variables and resolved arguments.
8. Record the child status. Learn attributable usage-limit holds at exit for
   isolated `rtr claude` and `rtr codex` launches.

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

## Native Conversation Operations

RTR treats a conversation as `(tool, profile, native session ID)`. Names and
generated titles are native mutable presentation metadata: Codex owns names in
`session_index.jsonl`, while Claude uses `agent-name`, `custom-title`, then `ai-title` in display-priority order.
Users rename the active conversation with each CLI's `/rename`; RTR does not
create another name store.

Exact ID opens search filenames in the allowed homes and read only matching
histories (plus Codex's small native name index). Native-name selectors still
need the catalog. Missing UUIDs report scope and default-home matches, then
fall back to an empty current-directory picker or a nonzero nonterminal exit.

Discovery translates both native stores into one catalog while preserving the
owning profile. Codex exec and subagent histories are hidden unless explicitly
included, but exact ID lookup includes them. Catalogs reuse persistent summaries
and give new files bounded metadata projections before background indexing. The
first physical Codex header owns identity and visibility; later replayed ancestor
headers never replace it. Claude top-level transcripts retain their native cwd,
prompt, and title fold; nested subagent transcripts are excluded.

Resume uses the native command in the exact original isolated home. Disabled and
normally bypassed source profiles remain recoverable without changing rotation.
When an eligible alternative exists, an automatic fork excludes the source and
applies the eligible alternatives' relative shares. When the source is the only
eligible profile for the model, the fork stays in that home, even if other
profiles are enabled. If only zero-share alternatives are eligible, forks
rotate equally among them. `--to-profile` forces an enabled destination
without moving the cursor.
`--profile` continues to select the source. The recorded cwd is restored when it
still exists, and forks always use isolated homes despite ordinary bypass policy.

Forks copy native history with a fresh ID, then resume that ID in the destination.
An in-place fork publishes the copy alongside the source session in the same home.
Codex paginated parent prefixes become one independent rollout;
Claude retains message UUID links and copies session companions, but drops
`session-owner` records rather than attributing the destination session to the
source account. It does not invent account identifiers. Native CLIs own
replay and index rebuilding. Credentials and settings stay in their owning homes;
queued work, account bridge ownership, and Claude file-rewind state are excluded.

Automatic selection and destination preparation hold the normal state lock, then
commit the cursor before bulk copying. Later failure consumes the reserved slot;
rolling it back could undo a concurrent launch. Private staging and exclusive
publication prevent overwriting existing sessions. Assets publish before the
transcript, and only unpublished operation-owned files are removed on failure.
After publication the new session is retained even if its native launch fails.

The built-in terminal picker separates rendered rows, searchable text, and the
encoded identity returned on selection. A persistent RTR-owned catalog and per-file dialogue shards
reuse unchanged transcripts and checkpoint append offsets. Parallel workers
project changed files and search literal terms across human dialogue and
metadata. The terminal formats visible rows only and paints when state changes;
selection and preview requests never change the search revision. Refresh generations and query
revisions prevent stale background results from changing what Enter opens.

Codex thread IDs and rollout IDs are distinct after a revert. The newest segment
supersedes the original, with active storage preferred to archived duplicates;
segment filename time orders successive reverts. Preferring a segment also
repairs historical RTR UTC filenames that sort after newer native local-time
segments. `history_base.thread_id` identifies one immutable rollout, not the
current rollout of that thread; its byte and ordinal limits apply to that file.
An absent base stops ancestry. Catalog time, preview, search and copy share this
lineage rule. SQLite is used only as an external verification oracle, never at
runtime. New copies use Codex's local-time directories and filenames.

Conversation, Matches, and Details previews keep recent context, matching old
passages, and launch settings separate. Conversation content is bottom anchored;
its scroll state measures distance from the bottom, pages into older indexed
messages, and resets with selection. Explicit model and effort descriptions
reuse the runner's argument merging; unknown native defaults remain unknown.
Enter follows the invoking command, and Ctrl-F / Ctrl-R select an explicit mode.
A terminal guard restores normal input and screen state before native launch.

## Terminal Ownership

Interactive children inherit stdin, stdout, and stderr. This preserves full-screen
terminal behavior, prompts, and output formatting. While waiting, rtr catches common
Unix terminal signals and forwards them to the direct child so it can finish
and produce a real status before usage is recorded. rtr also sets
`kill_on_drop` for cancellation paths.

rtr relays the Codex exec startup stream to read the native session ID. A new
isolated Claude print launch gets an rtr-generated `--session-id` unless its
arguments already select a session. At the end of an isolated `rtr claude` or
`rtr codex` launch, rtr uses owned transcript evidence to learn a hold.
Interactive Codex can learn account-level holds from recently written rollouts
in the selected home. A fork suggestion also requires a unique session match
and an eligible destination in another home.

rtr can replay a new noninteractive command once if the complete transcript
shows that its sole turn hit a usage limit before model output or tool activity.
The replay selects another eligible isolated profile and keeps the effective
model, user arguments, and working directory. Supplied Claude session IDs stay
bound to their first home. Interactive launches, continuations, and commands
that could have consumed piped or file input require a user-initiated continuation.

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
round-robin cursors and weighted scores live in `state.toml`; usage events live in `usage.jsonl`.
Both use advisory locks, and state replacement is atomic.

Percentage overrides are fixed shares of automatic selections; unassigned
enabled profiles split the remainder equally. The scheduler uses exact integer
ratios and smooth weighted rotation to avoid random streaks. Its persisted
allocation detects policy changes and discards stale scheduling credit under
the state lock. All-default tools retain the existing equal-rotation cursor.
Explicit launches, resumes, and inspection do not change automatic progress.

The weight command serializes lossless percentage edits under the config lock,
validates the resulting allocation before writing, and prints that transaction's
distribution. Disabled profiles retain settings but receive no automatic share;
zero-share enabled profiles remain available explicitly. Incomplete or excessive
fixed totals are reported instead of silently inflating a low-quota profile's
share. Reset removes all overrides for a tool, including disabled profiles.

Profile-table removal preserves unrelated comments, formatting, and quoted
keys. Equal rotation normalizes stale cursors by modulo; weighted rotation
detects the changed allocation. Profile deletion needs no state rewrite, but
removing the last profile without an override can leave fixed totals incomplete.

Native-home path segments are encoded before joining. Directory creation
rejects symlink components and tightens permissions to `0700`.

## Configuration Philosophy

The schema covers native commands, startup copies, profiles, and percentage
shares. `[routing]` controls native usage refresh. `[routing.claude]` controls
Fable fallback with `auto_drop` and `fallback_model`. Config loading rejects
unknown fields.


## Model-aware routing

Resolve the requested native arguments before selecting a destination. Build a
per-command inventory of native meters and owned transcript observations. For
automatic routing, refresh stale or missing candidate readings before
evaluating them. Apply holds and full windows to the requested model at a
common decision time. Admit unknown headroom alongside observed headroom.

Usage refresh runs outside the rotation lock in the candidates' native homes.
Claude writes its own usage cache. Codex App Server returns readings to rtr,
which stores timestamped observations in `usage-refresh.json` alongside the
latest refresh status for each tool and profile. Compare the latest saved App
Server reply with the newest usable rollout snapshot per limit ID. When both
contain a Codex meter, the newer reading wins. A failed refresh leaves the
previous evidence available. See
[usage refresh](usage.md#refresh-subscription-usage) for the command and limits.

Automatic launch and fork share one rotation per tool. Store the full configured
allocation and mask exhausted or excluded homes for each decision. Accrue credit
only for eligible positive-share profiles. Automatic forks can rotate uniformly
among usable zero-share alternatives. If every routable home blocks Fable,
try the configured Opus fallback when enabled.

Prepare the chosen home before saving the trial rotation. Release the lock before
copying history or launching the child. Preview, report, and handoff share the
prepared settings. A copy failure after reservation keeps the consumed slot.
Exact resume keeps its home and native ID, and reads quota evidence for a warning.

The v1 watchdog uses `session --json`, `fork --no-picker --dry-run --json`,
and `fork --no-picker --report PATH`. Exit 75 carries a per-profile usable-time
estimate and a pool-wide earliest reset. Routing decisions use `routing.jsonl`.
Child outcomes use `usage.jsonl`.
