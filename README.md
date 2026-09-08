<img src="assets/rtr-icon.svg" alt="rtr routing mark: one command enters a selector and leaves on one of three profile lanes" width="72" height="72">

# rtr

Native profile launcher for Claude Code and Codex.

rtr gives each Claude or Codex profile its own native tool home, then launches
the real CLI directly. Use it when you want separate subscriptions, accounts,
settings, sessions, and skills without logging in and out.

## How it works

<img src="assets/rtr-flow.svg" alt="Launch flow. Two commands enter a selector: 'rtr codex -p personal' pins a profile and leaves the cursor unchanged, while a bare 'rtr codex' takes the next enabled profile in name order and advances the cursor. The selector feeds three isolated native homes — codex/oss, codex/personal, codex/work — each with its own CODEX_HOME. The selected lane, codex/personal, continues into 'exec codex', a direct child that inherits the terminal. The native home is created and its startup files synchronized under an exclusive lock before the cursor advances. Claude profiles receive CLAUDE_CONFIG_DIR and CLAUDE_SECURESTORAGE_CONFIG_DIR instead of CODEX_HOME.">

`--profile` pins a profile and leaves the rotation cursor unchanged; without it,
rtr takes the next enabled profile in name order. Either way the profile's native
home is prepared before the real CLI takes over the terminal.

## Install

```bash
make install
```

This builds a release binary and installs it to `~/.cargo/bin/rtr` by default.
Set `INSTALL_BINDIR` to choose another destination. The interactive session
picker is built into RTR; it requires a terminal and no additional executable.

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

When the child exits, rtr reports which profile ran and prints a copyable
profile-bound resume command on stderr.

Use `--` when a child argument should not be parsed by rtr:

```bash
rtr codex -- --profile native-codex-profile
```

`-p` / `--profile` remains an rtr option anywhere before `--`, so it can be
appended to a shell alias that already supplies native arguments. When known
singleton native options such as `--model` or `--effort` repeat, the rightmost
one wins and rtr removes the earlier duplicates before launching the child.

Pause a profile and bring it back later:

```bash
rtr disable codex --profile personal
rtr enable codex --profile personal
```

Temporarily use the real CLI's default home when an isolated profile home is
unusable, then restore isolation:

```bash
rtr bypass codex --profile personal
rtr codex --profile personal
rtr unbypass codex --profile personal
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

Discover the isolated homes owned by rtr:

```bash
rtr paths
rtr paths --json
```

The human output is for inspection. Local integrations such as `tokens` should
consume the versioned JSON contract instead of parsing presentation text.

Search every native conversation across all configured profiles:

```bash
rtr sessions
```

The picker searches the complete user/assistant dialogue plus native names,
first prompts, working directories, tools, profiles, and IDs. `Enter` forks the
selected conversation, `Ctrl-R` resumes it in place, and `Ctrl-F` explicitly
forks it. Rows show titles, relative ages, agent/profile, and project names as
space allows.
`Tab` cycles three previews: recent **Conversation**, highlighted search
**Matches**, and **Details** with full identity and requested launch settings.
The preview moves below the list in narrow terminals; `Ctrl-U` / `Ctrl-D`
scroll it and `Alt-P` toggles it.

Session metadata appears before background transcript indexing finishes. The
indexing counter tells you when full-dialogue search is complete. `Alt-H`
toggles current-directory scope, `Alt-T` cycles agents, `Alt-A` cycles profiles,
`Alt-R` refreshes, and `Alt-Y` copies the selected action's command. `F1` shows
all controls. Current-directory sessions come first when the query is empty.

Open an exact native ID or exact native name directly:

```bash
rtr resume <session-id-or-name>
rtr fork <session-id-or-name>
rtr fork <session-id> --profile work --to-profile personal
```

Without an exact selector, both commands open the same picker: **Enter follows
the command** (`fork` or `resume`), while `Ctrl-F` / `Ctrl-R` always choose the
explicit action. With `--to-profile`, Ctrl-R is unavailable. The launch line displays RTR's merged model and effort
arguments; unspecified settings are labeled as native defaults.

Use `--tool`, `--profile`, or `--here` to narrow the source. Forks select the next
enabled profile using the same round-robin cursor as normal launches. Optional
`--to-profile` chooses an enabled destination without advancing rotation. A fork
into another profile copies native history under a fresh ID and resumes it there;
the source stays unchanged. The menu's existing fork actions use this behavior too.

Resume always uses the original isolated home, even when that profile is disabled
or normally bypassed, without changing rotation. Forks also use isolated homes,
including destinations configured for ordinary bypass launches. Use
`rtr sessions --here` to browse the current directory's conversations, or add
`--list` / `--json` for noninteractive output.

Rename the active conversation with the native `/rename` command in either
Claude Code or Codex. RTR reads those native names rather than maintaining a
second naming database.

## Commands

```text
rtr init [--force]
rtr add <claude|codex> --profile <name>
rtr rm <claude|codex> --profile <name> [--yes]
rtr fix <claude|codex> --profile <name>
rtr config [--color <auto|always|never>] [edit]
rtr claude [-p|--profile <name>] [claude args...]
rtr codex  [-p|--profile <name>] [codex args...]
rtr enable <claude|codex> --profile <name>
rtr disable <claude|codex> --profile <name>
rtr bypass <claude|codex> --profile <name>
rtr unbypass <claude|codex> --profile <name>
rtr paths [--json] [--color <auto|always|never>]
rtr sessions [--tool <claude|codex>] [-p|--profile <name>] [--here]
             [-q|--query <text>] [--list|--json]
rtr resume [session-id-or-name] [--tool <claude|codex>]
           [-p|--profile <name>] [--here] [-- native args...]
rtr fork [session-id-or-name] [--tool <claude|codex>]
         [-p|--profile <name>] [--here] [--to-profile <name>] [-- native args...]
rtr ls [--today] [--color <auto|always|never>]
rtr show <claude|codex> --profile <name>
rtr status [tool]
```

`rtr ls` combines profile state with recorded launch counts. It includes unused
profiles with zero runs and separates historical usage for removed profiles.
Use `--today` for the current local day; otherwise counts cover all time. The
`FAILED` column counts non-zero or unavailable child exits.

`ls`, `paths`, and `config` use color when stdout is a terminal. Cyan identifies
agents and path basenames; enabled profiles are green, bypassed/missing homes
yellow, and nonzero failure counts red. Headers and secondary text are dimmed.
`NO_COLOR` disables automatic color when nonempty; `--color=always` or `never`
overrides detection. Pipes and redirects are plain by default, and JSON always
remains plain. `rtr config` still emits only the exact config path.

## Configuration

Run `rtr config` to print the resolved path, or `rtr config edit` to open an
existing config with `$VISUAL` or `$EDITOR`. The default path is
`~/.config/rtr/config.toml`.

```toml
[tools.claude]
command = ["claude"]
args = ["--effort", "max", "--model", "claude-opus-5"]
copy = [
  { source = "~/.skills", destination = "skills" },
  { source = "shared/CLAUDE.md", destination = "CLAUDE.md" },
]

[tools.claude.profiles.work]

[tools.codex]
command = ["codex"]
args = ["-m", "gpt-5.6-sol", "-c", "model_reasoning_effort=max"]

[tools.codex.profiles.personal]
```

`copy` belongs to the tool, so every non-bypassed isolated launch refreshes the
listed files or directories in whichever profile was selected. Sources use the
user home for `~/...` and the rtr config directory for relative paths.
Destinations use the selected profile home for both relative paths and `~/...`.
Omit `copy` to retain the built-in skills refresh (and optional legacy
`skills_source` override), or set `copy = []` to disable startup copying.

`args` supplies native CLI defaults to normal launches, resumes, and forks.
Explicit arguments replace matching `--model` / `--effort` defaults and matching
Codex `-c` keys; repeated dangerous-permission flags collapse to one.

Profiles are enabled by default. `rtr disable <tool> --profile <name>` flips
`enabled = false` in place — comments preserved, native home and sign-in kept —
and removes the profile from explicit selection and automatic rotation until
`rtr enable <tool> --profile <name>` restores it. You can also set
`enabled = false` by hand.

`rtr bypass <tool> --profile <name>` persists `bypass = true` and keeps
selecting the profile normally, but launches the real CLI with no native-home
override so it uses the default Claude or Codex home. rtr does not create the
isolated home or run startup synchronization during bypassed launches.
`rtr unbypass` restores isolated launches.

Claude receives profile-specific `CLAUDE_CONFIG_DIR` and
`CLAUDE_SECURESTORAGE_CONFIG_DIR`. Codex receives profile-specific
`CODEX_HOME`. rtr does not read or copy credentials.

## Profile Home Discovery

`rtr paths` lists the resolved isolated native home for every configured Claude
and Codex profile. It includes disabled, bypassed, and not-yet-created profiles
so historical usage remains discoverable. For a bypassed profile, `home` is
still its rtr-managed isolated home; current bypassed launches use the tool's
default home instead.

`rtr paths --json` emits the stable machine contract. Version 1 has a top-level
`version` and `profiles`; each profile has `tool`, `profile`, `home_env`, `home`,
`enabled`, `bypass`, and `exists`. The command only resolves and checks paths:
it does not create homes, run startup synchronization, or inspect credentials
or sessions.

## Files

| Path | Purpose |
|---|---|
| `~/.config/rtr/config.toml` | Tool and profile config |
| `~/.local/state/rtr/homes/<tool>/<profile>/` | Isolated native tool home |
| `~/.local/state/rtr/state.toml` | Rotation cursors |
| `~/.local/state/rtr/usage.jsonl` | Launch history and exit codes |

Set `RTR_CONFIG_DIR` and `RTR_STATE_DIR` to override the two base directories.

## More Detail

See [docs/usage.md](docs/usage.md) for config fields, startup synchronization,
profile selection rules, errors, and environment details.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

See [docs/design.md](docs/design.md) and
[docs/architecture.md](docs/architecture.md) for design and internals.
