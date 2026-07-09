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

Profiles are enabled by default. Set `enabled = false` to exclude one from both
automatic and forced selection.

## Launch a Profile Again

```bash
rtr claude --profile work
rtr codex --profile personal
```

All tool-owned state remains isolated under rtr's state directory.

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

## Errors

- Missing config points to `rtr init`.
- Unknown, missing, or disabled profiles fail before child launch.
- Missing configured skills sources fail without replacing existing skills.
- An unknown config or state field fails parsing.
- Child spawn failures name the configured executable and record an event with
  no exit code.
