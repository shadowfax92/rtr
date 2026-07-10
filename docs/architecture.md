# Architecture

## Module Map

| Module | Responsibility |
|---|---|
| `cli` | First-class launch, profile maintenance, and config command parsing |
| `config` | Strict TOML schema plus lossless, atomic profile table edits |
| `config_command` | Script-friendly config path output and editor launching |
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

`fix` skips selection and prepares an explicitly validated existing profile,
so it shares the same environment, skills refresh, child execution, and usage
recording without reading or writing the round-robin cursor. `rm` validates and
confirms first, removes the selected TOML table under the config lock, then
deletes only the safe path returned for that profile.

## Process Contract

The configured command owns the executable and immutable leading arguments.
Runtime arguments are appended exactly once. The runner adds the tool-specific
identity variables:

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

## Test Boundaries

Unit tests cover strict schemas, path encoding, locks, selection, skills copy,
Claude/Codex symlink policies, profile rendering, and statistics.
`tests/run_smoke.rs` launches real shell
children to verify environment, argument order, skills refresh, cursor
behavior, exact-home removal, config editor status, repair isolation, exit
mapping, error recording, and absence of extra run artifacts.
