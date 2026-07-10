<img src="assets/rtr-icon.svg" alt="rtr routing mark: one command enters a selector and leaves on one of three profile lanes" width="72" height="72">

# rtr

Native profile launcher for Claude Code and Codex.

rtr gives each Claude or Codex profile its own native tool home, then launches
the real CLI directly. Use it when you want separate subscriptions, accounts,
settings, sessions, and skills without logging in and out.

## How it works

<img src="assets/rtr-flow.svg" alt="Launch flow. Two commands enter a selector: 'rtr codex -p personal' pins a profile and leaves the cursor unchanged, while a bare 'rtr codex' takes the next enabled profile in name order and advances the cursor. The selector feeds three isolated native homes — codex/oss, codex/personal, codex/work — each with its own CODEX_HOME. The selected lane, codex/personal, continues into 'exec codex', a direct child that inherits the terminal. The native home is created and its skills refreshed under an exclusive lock before the cursor advances. Claude profiles receive CLAUDE_CONFIG_DIR and CLAUDE_SECURESTORAGE_CONFIG_DIR instead of CODEX_HOME.">

`--profile` pins a profile and leaves the rotation cursor unchanged; without it,
rtr takes the next enabled profile in name order. Either way the profile's native
home is prepared before the real CLI takes over the terminal.

## Install

```bash
make install
```

This builds a release binary and installs it to `~/.cargo/bin/rtr` by default.
Set `INSTALL_BINDIR` to choose another destination.

## Quick Start

Create the starter config:

```bash
rtr init
```

Create profiles and sign in inside each isolated home:

```bash
rtr add claude --profile work
rtr add codex --profile personal
```

Run an explicit profile:

```bash
rtr claude --profile work --model claude-opus-4-6
rtr codex --profile personal -m gpt-5.5 -c model_reasoning_effort=xhigh
```

Omit `--profile` to rotate through enabled profiles for that tool:

```bash
rtr codex
rtr codex
```

Use `--` when a child argument should not be parsed by rtr:

```bash
rtr codex -- --profile native-codex-profile
```

Pause a profile and bring it back later:

```bash
rtr disable codex/personal
rtr enable codex/personal
```

Find or edit the active config, repair a profile in place, or remove one:

```bash
rtr config
rtr config edit
rtr fix codex --profile personal
rtr rm codex --profile personal
```

`rm` prints the exact native-home path and confirms before deleting its auth,
settings, and sessions. Use `--yes` only when confirmation is handled elsewhere.

## Commands

```text
rtr init [--force]
rtr add <claude|codex> --profile <name>
rtr rm <claude|codex> --profile <name> [--yes]
rtr fix <claude|codex> --profile <name>
rtr config [edit]
rtr claude [-p|--profile <name>] [claude args...]
rtr codex  [-p|--profile <name>] [codex args...]
rtr enable <tool>/<profile>
rtr disable <tool>/<profile>
rtr ls
rtr show <tool>/<profile>
rtr status [tool]
rtr stats [--today]
```

## Configuration

Run `rtr config` to print the resolved path, or `rtr config edit` to open an
existing config with `$VISUAL` or `$EDITOR`. The default path is
`~/.config/rtr/config.toml`.

```toml
[tools.claude]
command = ["claude"]

[tools.claude.profiles.work]

[tools.codex]
command = ["codex"]

[tools.codex.profiles.personal]
```

Profiles are enabled by default. `rtr disable <tool>/<profile>` flips
`enabled = false` in place — comments preserved, native home and sign-in kept —
and removes the profile from explicit selection and automatic rotation until
`rtr enable` restores it. You can also set `enabled = false` by hand.

Claude receives profile-specific `CLAUDE_CONFIG_DIR` and
`CLAUDE_SECURESTORAGE_CONFIG_DIR`. Codex receives profile-specific
`CODEX_HOME`. rtr does not read or copy credentials.

## Files

| Path | Purpose |
|---|---|
| `~/.config/rtr/config.toml` | Tool and profile config |
| `~/.local/state/rtr/homes/<tool>/<profile>/` | Isolated native tool home |
| `~/.local/state/rtr/state.toml` | Rotation cursors |
| `~/.local/state/rtr/usage.jsonl` | Launch history and exit codes |

Set `RTR_CONFIG_DIR` and `RTR_STATE_DIR` to override the two base directories.

## More Detail

See [docs/usage.md](docs/usage.md) for config fields, skill syncing, profile
selection rules, errors, and environment details.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

See [docs/design.md](docs/design.md) and
[docs/architecture.md](docs/architecture.md) for design and internals.
