# Architecture

## Module Map

| Module | Responsibility |
|---|---|
| `cli` | First-class launch, conversation, profile maintenance, and config command parsing |
| `config` | Strict TOML schema plus lossless, atomic profile table edits |
| `config_command` | Script-friendly config path output and editor launching |
| `conversations` | Cross-profile native catalog, human-dialogue indexing, bounded inspection, and resume/fork translation |
| `conversation_command` | Human/JSON rendering and direct-open versus picker dispatch |
| `picker` | Terminal input/layout, background dialogue search, preview excerpts, and launch descriptions |
| `sessions` | Backwards-compatible five-row `rtr here` view over `conversations` |
| `tool_specs` | Native-home variables and skills relocation policy per tool |
| `selection` | Enabled-profile validation and round-robin choice |
| `state` | Locked, atomic round-robin cursor persistence |
| `paths` | Config/state resolution, private directories, safe profile paths |
| `runner` | Profile creation/repair, native-home preparation, skills refresh, direct child execution |
| `profiles` | Profile list/show/status plus confirmed, exact-home removal |
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

Conversation opens take a separate policy-free path:

```text
CLI / Herdr
 ├─ conversations::query
 │   ├─ Claude top-level project JSONL + native title records
 │   └─ Codex rollout metadata + history/name indexes
 ├─ picker::run (only when interactive selection is needed)
 │   ├─ background catalog + complete human-dialogue index
 │   ├─ background fuzzy matching + bounded previews
 │   └─ terminal events + responsive rows and preview tabs
 └─ conversations::open
     ├─ native resume/fork argument translation
     └─ runner::run_isolated_profile_tool
```

The catalog identity is `(tool, profile, native session ID)`. A direct native
name can resolve that identity, but mutable display titles never replace it.
Exact resume/fork intentionally bypasses selection, rotation, and ordinary
`enabled` / `bypass` policy because the recorded profile home is part of native
session identity. It still uses the shared startup sync, child lifecycle, and
usage-event machinery.

Codex discovery reads its small indexes, the bounded rollout metadata prefix,
and a bounded timestamp tail rather than scanning message bodies. Claude's much
smaller top-level transcript corpus is scanned for cwd and native title records;
nested subagent logs are excluded. The interactive picker paints its frame before
catalog discovery, then makes metadata selectable while a loader indexes the
complete human dialogue. A separate worker ranks queries and returns bounded
preview excerpts; neither disk parsing nor fuzzy matching runs on the terminal
event loop. Recent-message previews are bounded tail reads and are cached.

Refresh generations reject stale loader results; query revisions reject stale
search snapshots and prevent Enter from launching an old result while a query
is pending. Selection uses the encoded identity throughout. Cancellation is
checked between files and transcript records and does not wait for indexing.
Terminal restoration precedes the existing native launcher.

The index stores one dialogue string plus message ranges; Unicode character
offsets translate matcher highlights back to message excerpts. Tool payloads
and internal instructions never enter that string. Script output and exact
opens retain the existing discovery path and do not index full dialogue.

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
├── state.toml
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
- Malformed historical usage lines are reported and skipped during stats.
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
