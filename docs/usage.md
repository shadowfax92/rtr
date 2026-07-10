# Usage

## Initialize

```bash
rtr init
```

This creates `~/.config/rtr/config.toml` with Claude and Codex tool entries.
`--force` replaces an existing file.

Create and sign in to native profiles:

```bash
rtr add claude --profile work
rtr add codex --profile personal
```

`rtr add` atomically adds the corresponding empty table, prepares the native
home and skills, and launches the tool. Adding an existing profile fails before
changing its config, home, skills, or launching a child.

The resulting config entries look like:

```toml
[tools.claude.profiles.work]

[tools.codex.profiles.personal]
```

Profiles are enabled by default. Use `rtr disable` / `rtr enable` (below) to
exclude one from selection and bring it back.

## Configuration

Default path: `~/.config/rtr/config.toml`

```toml
[tools.claude]
command = ["claude"]
skills_source = "~/.claude/skills"

[tools.claude.profiles.work]

[tools.claude.profiles.personal]
enabled = false

[tools.codex]
command = ["codex"]
skills_source = "shared/codex-skills"

[tools.codex.profiles.work]

[tools.codex.profiles.personal]
```

| Field | Meaning |
|---|---|
| `command` | Executable and immutable leading arguments |
| `skills_source` | Optional source copied into every selected native home |
| `profiles.<name>.enabled` | Whether selection may use the profile; managed by `rtr enable` / `rtr disable` |

Relative `skills_source` paths resolve from the rtr config directory. `~` and
`~/...` resolve from the user's home. Configuration is strict: unsupported
fields are rejected instead of ignored.

## Launch a Profile Again

```bash
rtr claude --profile work
rtr codex --profile personal
```

All tool-owned state remains isolated under rtr's state directory.

For Claude, rtr sets `CLAUDE_CONFIG_DIR` and
`CLAUDE_SECURESTORAGE_CONFIG_DIR` to the same profile home. Claude-owned user
settings, app state, sessions, plugins, and account context therefore remain
profile-specific. On macOS, Claude keeps credential secrets in Keychain and
uses the config path to qualify that service; rtr never accesses the secret.
Project `.claude/*` discovery still comes from the working tree.

Pass tool arguments after the rtr arguments:

```bash
rtr claude -p work --model claude-opus-4-6
rtr codex -p personal -m gpt-5.5 -c model_reasoning_effort=xhigh
```

Use `--` to force the rest of the command line to the child:

```bash
rtr codex -p personal -- --profile child-profile
```

## Automatic Rotation

Without `--profile`, rtr chooses enabled profiles in lexicographic order and
advances a per-tool cursor:

```bash
rtr codex
rtr codex
rtr codex
```

A forced profile does not move that cursor. Profile preparation completes
before a cursor update is saved, so an invalid skills source does not consume a
turn.

## Disable and Re-enable a Profile

```bash
rtr disable codex/personal
rtr enable codex/personal
```

Disabling flips only `enabled = false` in config.toml, preserving hand-written
comments. The profile's native home, sign-in, skills, usage history, and the
rotation cursor stay untouched, so re-enabling restores it exactly as it was.
Disabled profiles are skipped by rotation and rejected by `--profile`.

Both commands are idempotent: repeating one reports the current state and
succeeds. Disabling the last enabled profile is allowed — launches fail with
"no enabled profiles" until one is re-enabled. Toggles serialize under the
same config lock as `rtr add`, so concurrent updates are never lost.

## Skills

Each launch refreshes `<native-home>/skills` from the tool's source.

Defaults:

| Tool | Source | Destination |
|---|---|---|
| Claude | `~/.claude/skills` | `$CLAUDE_CONFIG_DIR/skills` |
| Codex | `~/.codex/skills` | `$CODEX_HOME/skills` |

Override a source per tool:

```toml
[tools.codex]
command = ["codex"]
skills_source = "~/shared/codex-skills"
```

Relative paths resolve from `RTR_CONFIG_DIR`. The refresh is locked and uses a
temporary sibling directory, so concurrent launches cannot expose a partial
skills tree.

Codex keeps the real `HOME`, so `$HOME/.agents/skills`, repository
`.agents/skills`, and admin roots remain natively discoverable. rtr skips a
configured source already inside the canonical user root. Otherwise it bridges
the configured source or a distinct legacy `~/.codex/skills`, excluding source
`.system` and preserving the profile's Codex-owned `.system` cache.

External relative skill symlinks are rebased so they stay usable after copying
into a profile home. Internal and dangling relative links stay verbatim.

## Inspect Profiles and Usage

```bash
rtr ls
rtr show codex/personal
rtr status
rtr status codex
rtr stats
rtr stats --today
```

`show` includes the profile's native-home environment variable and resolved
path. `stats` groups launch counts and non-zero or unavailable child exits by
tool and profile.

## Environment Overrides

```bash
RTR_CONFIG_DIR=/tmp/rtr-config \
RTR_STATE_DIR=/tmp/rtr-state \
rtr codex --profile personal
```

The defaults are:

- Config: `~/.config/rtr`
- State, native homes, and usage: `~/.local/state/rtr`

## Files

| Path | Purpose |
|---|---|
| `~/.config/rtr/config.toml` | Tool and profile configuration |
| `~/.local/state/rtr/homes/<tool>/<profile>/` | Isolated native tool home |
| `~/.local/state/rtr/state.toml` | Round-robin cursors |
| `~/.local/state/rtr/usage.jsonl` | Per-launch tool, profile, timestamp, and exit code |

## Errors

- Missing config points to `rtr init`.
- Unknown, missing, or disabled profiles fail before child launch.
- Missing configured skills sources fail without replacing existing skills.
- An unknown config or state field fails parsing.
- Child spawn failures name the configured executable and record an event with
  no exit code.
