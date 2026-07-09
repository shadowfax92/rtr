# rtr

**Native profile launcher for Claude Code and Codex.**

rtr gives each subscription profile its own tool home, refreshes that profile's
skills, selects a profile, and launches the real CLI as a direct child process.
Codex receives a profile-specific `CODEX_HOME`; Claude receives a
profile-specific `CLAUDE_CONFIG_DIR`.

## Features

- Isolated native homes for complete auth, settings, session, and account state
- First-class `rtr codex` and `rtr claude` commands with argument passthrough
- Explicit `--profile` selection or automatic round-robin rotation
- First-class `rtr add` onboarding for atomic profile creation and sign-in
- Native skills discovery: Codex inherits canonical user/repository roots and
  bridges only distinct legacy or configured roots; relocated links stay usable
- Private config, native-home, state, and usage files
- Usage counts and failure rates by tool and profile
- Child terminal ownership and shell-compatible exit codes

## Install

```bash
make install
```

This builds a release binary and installs it to `~/.cargo/bin/rtr` by default.
Override `INSTALL_BINDIR` to choose another destination.

## Quick Start

```bash
rtr init
```

Create a profile and sign in inside its isolated native home:

```bash
rtr add claude --profile work
rtr add codex --profile personal
```

This is equivalent to adding profile tables under the starter tool entries:

```toml
[tools.claude]
command = ["claude"]

[tools.claude.profiles.work]

[tools.codex]
command = ["codex"]

[tools.codex.profiles.personal]
```

Future launches reuse the full native state stored in those homes:

```bash
rtr claude --profile work --model claude-opus-4-6
rtr codex --profile personal -m gpt-5.5 -c model_reasoning_effort=xhigh
```

Claude receives both `CLAUDE_CONFIG_DIR` and
`CLAUDE_SECURESTORAGE_CONFIG_DIR` pointed at the same profile home. This keeps
settings, sessions, plugins, and the path-qualified macOS Keychain namespace
isolated together without rtr reading or copying credentials.

Omit `--profile` to rotate through enabled profiles in name order:

```bash
rtr codex
rtr codex
```

## Commands

```text
rtr init [--force]
rtr add <claude|codex> --profile <name>
rtr claude [-p|--profile <name>] [tool args...]
rtr codex  [-p|--profile <name>] [tool args...]
rtr ls
rtr show <tool>/<profile>
rtr status [tool]
rtr stats [--today]
```

Use `--` when a child argument would otherwise be read as an rtr option:

```bash
rtr codex -- --profile native-codex-profile
```

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
| `profiles.<name>.enabled` | Whether automatic and forced selection may use the profile |

Relative `skills_source` paths resolve from the rtr config directory. `~` and
`~/...` resolve from the user's home. When omitted, rtr uses
`~/.claude/skills` for Claude and `~/.codex/skills` for Codex. If the default
source is absent, the selected profile's stale skills directory is removed.

Skill directories may be symlinks. Links that point outside the copied tree are
rebased so they remain usable from each profile home; internal and dangling
links retain their relative form.

Codex keeps the real `HOME` and working directory, so `$HOME/.agents/skills`,
repository `.agents/skills`, and admin roots remain natively discoverable. rtr
bridges only a distinct legacy `~/.codex/skills` or configured source, excludes
source `.system`, and preserves the selected home's Codex-owned `.system` cache.

Configuration is strict: unsupported fields are rejected instead of ignored.

## Selection and Execution

`--profile` validates and uses the named enabled profile without advancing the
automatic cursor. Without `--profile`, rtr rotates through enabled profiles in
lexicographic order and persists the next cursor under the state directory.

Before advancing the cursor, rtr creates the native home and refreshes its
skills under an exclusive lock. A preflight failure therefore does not consume
a profile in the rotation.

`rtr add` serializes duplicate checks and config persistence under the config
lock, then launches the new profile for sign-in. Existing profiles are left
unchanged and must be launched with the normal tool command.

rtr then launches the configured command with the profile-specific native-home
variable and all runtime arguments. The child inherits the terminal. Normal
exit codes are returned unchanged; Unix signals map to `128 + signal`.

## Files

| Path | Purpose |
|---|---|
| `~/.config/rtr/config.toml` | Tool and profile configuration |
| `~/.local/state/rtr/homes/<tool>/<profile>/` | Isolated native tool home |
| `~/.local/state/rtr/state.toml` | Round-robin cursors |
| `~/.local/state/rtr/usage.jsonl` | Per-launch tool, profile, timestamp, and exit code |

Set `RTR_CONFIG_DIR` and `RTR_STATE_DIR` to override the two base directories.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

See [docs/usage.md](docs/usage.md), [docs/design.md](docs/design.md), and
[docs/architecture.md](docs/architecture.md) for more detail.
