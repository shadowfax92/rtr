# Architecture

## Module Map

| Module | Responsibility |
|---|---|
| `cli` | First-class launch, conversation, profile maintenance, and config command parsing |
| `config` | Strict TOML schema plus lossless, atomic profile table edits |
| `config_command` | Script-friendly config path output and editor launching |
| `conversations` | Cross-profile native catalog, human-dialogue indexing, bounded inspection, and resume/fork translation |
| `session_status` | Bounded newest-turn usage and model/effort inspection with inherited-history attribution |
| `quota` | Locked quota observations, success/clear watermarks, transfer origins, and preflight availability |
| `codex_history` | Physical rollout identity, active segment selection, and bounded inherited lineage |
| `conversation_transfer` | Independent native history copies, format adapters, asset relocation, and exclusive publication |
| `conversation_command` | Human/JSON rendering and direct-open versus picker dispatch |
| `picker` | Terminal input/layout, parallel term matching, preview excerpts, and launch descriptions |
| `session_index` | Private persistent metadata/dialogue projections, append checkpoints, and concurrent cache publication |
| `tool_specs` | Native-home variables and skills relocation policy per tool |
| `selection` | Exact percentage allocation, enabled-profile validation, and equal/weighted rotation |
| `state` | Locked, atomic rotation cursor and weighted score persistence |
| `weights` | Percentage CLI transactions and shared allocation presentation |
| `paths` | Config/state resolution, private directories, safe profile paths |
| `runner` | Profile creation/repair, native-home preparation, skills refresh, direct child execution |
| `profiles` | Profile show/status, policy changes, and confirmed exact-home removal |
| `profile_overview` | Current profile state joined with recorded usage for `ls` |
| `output` | Shared inspection color policy, semantic styles, and visible-width padding |
| `usage` | Locked JSONL events and aggregate statistics |
| `file_lock` | Shared advisory locking and atomic private-file writes |

## Runtime Sequence

```text
CLI
 └─ runner::run_subscription_tool
     ├─ Config::load
     ├─ tool_specs::get
     ├─ selection::select_profile
     ├─ Paths::ensure_profile_home_dir
     ├─ sync_profile_skills
     ├─ tokio::process::Command::spawn + signal-aware wait
     └─ usage::append_event
```

Automatic selection and profile preparation run inside the state lock. The
closure returns the prepared immutable arguments and environment; state is
saved only when that closure succeeds. Child execution happens after releasing
the state lock so a long-running CLI does not block another profile launch.

Profiles without `share_percent` divide the remainder after enabled fixed
percentages are allocated. The resolver uses integer ratios, so rounding in
displayed percentages never biases selection. Tools without enabled overrides
retain the legacy round-robin path and cursor. With overrides, smooth weighted
round-robin adds each profile's weight to its score, chooses the highest score
(name order breaks ties), and subtracts total weight from the winner.

The per-tool weighted state stores both allocation and scores. A changed
allocation starts fresh credit on the next selection; removing all enabled
overrides starts a fresh equal cycle. Config commands hold only the config lock
and never rewrite scheduler state. Normal launches and forks share progress;
explicit selections and resumes never advance it. Inspection uses the same
resolver without touching state. Disabled profiles retain settings but have no
allocation, and zero-share enabled profiles can still be selected explicitly.

Conversation opens resolve the source before applying operation-specific policy:

```text
CLI / Herdr
 ├─ conversations::lookup_id (filename lookup for exact IDs)
 ├─ conversations::query (lists, native names, and picker)
 │   ├─ Claude top-level project JSONL + native title records
 │   └─ Codex rollout metadata + history/name indexes
 ├─ picker::run (only when interactive selection is needed)
 │   ├─ cached catalog + background incremental human-dialogue index
 │   ├─ parallel literal field/dialogue matching + previews
 │   └─ terminal events + responsive rows and preview tabs
 └─ conversations::open
     ├─ resume → runner::run_isolated_profile_tool (exact source)
     └─ fork → runner::prepare_fork_run (shared rotation or --to-profile)
         ├─ same profile → native fork
         └─ other profile → conversation_transfer::copy → native resume
```

The catalog identity is `(tool, profile, native session ID)`. A direct native
name can resolve that identity, but mutable display titles never replace it.
Resume bypasses selection, rotation, and ordinary `enabled` / `bypass` policy
because the recorded home is part of native identity. Forks select enabled
destinations other than their source, sharing normal scheduling state unless
`--to-profile` is supplied. Only a sole usable source stays in place; when all
alternatives have zero share, forks rotate equally among those alternatives. Their
prepared launches always use isolated homes, ignoring ordinary bypass policy.
Both use shared startup sync, argument defaults, child lifecycle, and usage events.

The transfer module streams bounded snapshots into private staging under the
destination home. Codex adapters follow physical rollout IDs and parent byte/ordinal limits,
including same-thread segments, and flatten live history into one local-time
rollout. Claude adapters drop source account ownership, retain message links and
relocate session companions. Known native asset references are rewritten to fresh
destination-owned paths. No native database or account configuration is copied.
Assets publish first into exclusively reserved roots; a no-overwrite hard link
publishes the transcript last. Failure cleans only unpublished owned roots.
Published copies survive child failures, and their recovery commands identify the
destination profile explicitly. The rotation lock never spans transfer or child
execution; failure after destination preparation consumes the reserved slot.

`session_index` is shared by catalog lists and the picker. Warm picker opens map
`picker-v5.bin` and decode only its small embedded catalog. Dialogue stays in the
mapping, with original and lowercase text plus fixed-width message offsets. No
transcript discovery, shard decoding, preview I/O, or reconciliation runs before
the terminal acknowledges painting cached rows. Search uses the mapped lowercase
bytes; only selected preview messages become strings. Project aliases are cached
and refreshed once per distinct directory rather than resolved on each query.

After the first frame, reconciliation discovers/stats native transcripts and
refreshes Codex's external name/prompt indexes. Only changed physical checkpoints
load; unchanged composed dialogue blocks are reused. Dependency stamps include
Codex ancestors, so changing a parent invalidates its children's projections.
A missing mapped snapshot takes the cold path, with bounded metadata projections
followed by full dialogue indexing. Exact-ID opens keep their direct lookup.

`$RTR_STATE_DIR/session-index/catalog.json` stores versioned per-path summaries;
`shards/` holds each transcript's dialogue, byte endpoints, native metadata fold,
and last complete offset. Keys include path, size, nanosecond mtime, device, and
inode. Growth resumes from the saved offset after checking prefix/tail
fingerprints; replacement, truncation, same-size modification, or invalid cache
state rebuilds. Fingerprints contain hashes, never raw tool/request bytes. A
complete JSON record without a trailing newline is readable, but subsequent
growth rebuilds it; incomplete JSON tails are retried. Large files checkpoint
every 32 MiB. Tool payloads and internal instructions are discarded.

Shards publish before catalog summaries using owner-only atomic writes and
per-path advisory locks. Catalog updates merge under a separate short lock,
preserving another process's progress and rejecting stale source identities.
Parsing never holds those locks. Deleted sources are pruned, including orphan shards recovered by embedded path
when the catalog is lost; unreadable, corrupt,
or incompatible caches are misses. Cache failures preserve in-memory results.
Directories are `0700`, files `0600`; native homes remain read-only.

The mapped snapshot commits catalog and dialogue together by syncing a private
staging file and atomically renaming it. Published inodes are never truncated or
modified, so open mappings survive other processes' publications and pruning.
A separate publication lock rejects older refreshes after a newer one commits.
Sources can keep changing while a snapshot is built: it is a cached view of the
observed generation, checked again on reconciliation. Invalid headers, offsets,
versions, or unreadable snapshots are misses. Version 5 rebuilds physical
checkpoints once and removes the old JSON shards during migration. Lists still
wait for fresh metadata without opening mapped dialogue.

The first physical Codex header owns identity, cwd, visibility, start time, and
ancestry. Replayed ancestor `session_meta` records can appear later in a fork;
they contribute timestamps but never replace that ownership. Cache version 3
invalidates the earlier fold so unchanged files and append checkpoints rebuild.
Physical Codex projections compose in live-lineage order, honoring inherited
byte limits; abandoned suffixes never enter search or preview. Exec and subagent
threads remain hidden by default. A query worker runs terms across cores,
cancels obsolete revisions, and returns ranked rows and selected previews.
All terms use case-insensitive literal substrings in title, project path, ID,
first prompt, or dialogue, with AND membership across fields/messages. Anchors
respect individual field/message boundaries, including in metadata excerpts.
Matching excerpts map folded Unicode back to original scalars.

The terminal formats only the visible row window and redraws on input, results,
or resize. Selection and preview revisions are separate from search revisions,
so arrow keys move locally without searching. The previous preview remains until
a matching replacement arrives. Conversation starts at the bottom and requests
older indexed messages while scrolling. Refresh generations and search revisions
reject stale results; Enter cannot accept an obsolete query. Cancellation checks
between files and records avoid delaying native handoff. Terminal restoration
precedes native launch.

`fix` skips selection and prepares an explicitly validated existing profile,
so it shares the same environment, skills refresh, child execution, and usage
recording without reading or writing the round-robin cursor. `rm` validates and
confirms first, removes the selected TOML table under the config lock, then
deletes only the safe path returned for that profile.

## Process Contract

The configured command owns the executable and immutable leading arguments.
Tool-level `args` are native defaults: runtime model/effort/config options
replace the corresponding defaults, while unrelated values remain. Keeping
this mergeable layer separate prevents RTR from interpreting wrapper arguments
inside `command`. The runner also adds the tool-specific identity variables:

| Tool | Variable |
|---|---|
| Claude | `CLAUDE_CONFIG_DIR`, `CLAUDE_SECURESTORAGE_CONFIG_DIR` |
| Codex | `CODEX_HOME` |

The child inherits stdio and its numeric exit status. rtr forwards SIGINT,
SIGTERM, SIGHUP, and SIGQUIT received while waiting. On Unix, signal exits use
the shell convention `128 + signal`.

The shared prepared-run executor prints the startup summary on stderr before
spawning the child, so its output precedes the native terminal handoff. The
`launch_settings` module reads merged arguments for both this summary and the
conversation picker; neither guesses defaults from the child's configuration.

Claude receives `CLAUDE_CONFIG_DIR` and
`CLAUDE_SECURESTORAGE_CONFIG_DIR` set to the same home. Only `skills/` is seeded;
settings, commands, agents, plugins, auth state, and sessions remain owned by
that profile, while project `.claude/*` discovery remains rooted in the working
tree.

Codex keeps `HOME` and the working directory. Its canonical
`$HOME/.agents/skills`, repository, and admin roots remain native; rtr bridges a
distinct legacy or configured root into the selected home while excluding
source `.system` and preserving Codex's generated `.system` cache.

## Filesystem Contract

```text
$RTR_CONFIG_DIR/
└── config.toml

$RTR_STATE_DIR/
├── homes/
│   ├── claude/<profile>/      # config and secure-storage namespace
│   └── codex/<profile>/
├── session-index/          # disposable private catalog + dialogue shards
├── state.toml             # legacy-compatible scheduler only
├── quota.json             # limits and recovery watermarks
├── fork-origins.json      # copied-history ownership boundaries
└── usage.jsonl
```

Directories containing profile state are real directories with `0700`
permissions. Config, state, locks, and usage files use owner-only permissions.
Unsafe profile-name bytes are percent-encoded into deterministic path segments.
Recursive removal rejects symlinked path components instead of following them.

## Failure Boundaries

- Config and profile validation happen before filesystem or process changes.
- Profile removal updates config before deleting the home, so a deletion error
  can leave only recoverable orphaned state, never a configured profile whose
  credentials were already destroyed.
- Repair removes only the selected Codex home's `auth.json.lock`; it does not
  delete `auth.json`, sessions, general runtime locks, or sibling profile data.
- Skills refresh errors preserve the previous destination.
- Automatic cursor updates are not saved after preflight errors.
- Spawn errors are returned with executable context and recorded without an
  exit code.
- Malformed historical usage lines are reported and skipped during usage aggregation.
- An unreadable usage log leaves configured profiles visible in `ls` with unknown
  counts, rather than presenting zeros or hiding profile policy.
- Malformed conversation records become catalog diagnostics and do not prevent
  healthy profiles from being searched.
- A conversation transcript is revalidated before launch; an exact open never
  falls back to a same-named session in another profile.

## Test Boundaries

Unit tests cover strict schemas, path encoding, locks, selection, skills copy,
Claude/Codex symlink policies, profile rendering, and statistics.
`tests/run_smoke.rs` launches real shell
children to verify environment, argument order, skills refresh, cursor
behavior, exact-home removal, config editor status, repair isolation, exit
mapping, error recording, real PTY picker key semantics and terminal restoration,
exact archived-profile opens, and
absence of extra run artifacts.


## Quota failover

`session_status` inspects a bounded head plus reverse tail blocks without the
catalog/index. Claude's structured usage marker and Codex's terminal error
identify exhaustion; only same-turn saturated Codex windows can supersede a
parsed local-time reset. Synthetic messages never become model defaults.
The transfer publisher records the copied byte boundary in `fork-origins.json`
before linking its transcript into the destination. New profile-owned appends
are thus distinguishable even when inherited timestamps lie in the future.
Legacy copies use creation time as a conservative ownership heuristic.

`quota` reads recent native tails without holding locks, then merges per-profile
observations under the quota lock. Success and manual-clear watermarks prevent
older concurrent observations from resurrecting exhaustion. Limits expire at
their known reset or after a 15-minute unknown-reset hold. Quota state has its
own file because older releases reject new fields in `state.toml`.

Automatic selection filters resolved weights instead of recomputing configured
percentages. The runner reserves a usable destination under the existing
rotation lock; no usable destination returns typed exit 75 without a cursor
commit. Explicit destinations warn and retain their existing selection policy.
Dry runs apply observations and selection only in memory. Fork reports publish
atomically after history preparation and before process handoff; their IDs
identify prepared copies, while native forks have not created an ID yet.
