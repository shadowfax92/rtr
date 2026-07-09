# Usage

## Build and install

```sh
make
make install
make install PREFIX=/usr/local
```

Requires macOS (Apple Silicon) and a Rust toolchain.

## Set up subscription profiles

Initialize rtr and trust its local CA:

```sh
rtr init
rtr trust
```

`rtr add` creates the named profile, prepares its native home and skills, then
launches the tool for login. The selected home becomes the profile's source of
truth for auth and tool state.

```sh
rtr add claude --profile work
rtr add codex --profile personal
```

After the child exits, rtr prints the command to run the profile:

```sh
rtr codex --profile personal
rtr claude --profile work
```

Codex receives
`CODEX_HOME=~/.local/state/rtr/homes/codex/<profile>/`. Claude receives
`CLAUDE_CONFIG_DIR=~/.local/state/rtr/homes/claude/<profile>/`. These directories
are owner-only and isolated from the global tool homes.

Before each launch, rtr replaces `<profile home>/skills` from `skills_source` or
the tool default (`~/.codex/skills` or `~/.claude/skills`). A missing explicit
source is an error; a missing default leaves the profile with no synced skills.

## Run profiles

```sh
rtr claude
rtr claude --profile work
rtr claude -p work
rtr codex
rtr codex --profile personal
```

Without `--profile`, rtr rotates equally across enabled profiles and persists
the next cursor in `state.toml`. Forced selection validates the profile without
changing that cursor. Each completed or failed launch appends a usage event, so
`rtr stats --today` can report distribution and failure percentages.

Claude Code stores user settings, app state, session history, and installed
plugins under the selected config directory. Linux and Windows credential files
also live there; macOS credential secrets remain in Keychain. Claude Code 2.1.205
uses a distinct path-qualified Keychain service for each config directory, so
profiles can keep separate logins even though the secret is stored outside the
profile directory. rtr sets `CLAUDE_SECURESTORAGE_CONFIG_DIR` to the same path as
`CLAUDE_CONFIG_DIR`, preventing an inherited secure-storage override from
sharing one Keychain entry across profiles. rtr copies none of that shared state
into a new profile. It seeds only personal skills, so commands, agents, plugins,
settings, and sessions can differ by profile. Project `.claude/*` files still
load normally from the working tree.

Tool arguments are appended to the configured command:

```sh
rtr claude --effort xhigh --model claude-fable-5
rtr codex --dangerously-bypass-approvals-and-sandbox -m gpt-5.5
```

Put rtr-owned flags (`--profile/-p` and `--log`) before tool args. Use `--` if
the child tool needs one of those same flag names.

Adding a duplicate profile returns an error before changing its config, home,
skills, or launching the child. Run the existing profile normally instead.

## Commands

| Command | What it does |
| --- | --- |
| `rtr init [--force]` | Scaffold `config.toml` and mint the CA. |
| `rtr add <tool> --profile <name>` | Create a Claude/Codex profile and launch the tool for login. |
| `rtr claude [--profile/-p <name>] [tool args...]` | Run Claude with forced or round-robin profile selection. |
| `rtr codex [--profile/-p <name>] [tool args...]` | Run Codex with forced or round-robin profile selection. |
| `rtr ls` | List configured Claude/Codex profiles. |
| `rtr show <tool>/<profile> [--show-secrets]` | Show one profile, redacted by default. |
| `rtr stats [--today]` | Show per-profile run counts and failure percentages. |
| `rtr <tool>` / `rtr run <tool> [-- args]` | Run any configured tool through the legacy generic path. |
| `rtr <tool> --log` | Tee child output and proxy diagnostics to a private run directory. |
| `rtr switch <tool> <profile>` | Set the active profile for the legacy run path. |
| `rtr switch <profile>` | Same when the profile name is unique across tools. |
| `rtr status [tool]` | Show tools, active profiles, hosts, proxy port, CA, and trust state. |
| `rtr trust [--system]` | Trust the CA in the login or system keychain. |
| `rtr untrust [--system]` | Remove the CA's trust settings. |
| `rtr ca path` / `rtr ca show` | Print the CA certificate path or PEM. |

## Config reference

```toml
[proxy]
port = 62888

[tools.<name>]
command = ["codex"]
hosts = ["chatgpt.com"]
selection = "round-robin"
skills_source = "~/.skills"

[tools.<name>.profiles.<profile>]
enabled = true
set = { Authorization = "Bearer …" }
remove = ["X-Trace-Id"]
```

`hosts` is an exact hostname or dot-prefixed suffix. `.chatgpt.com` matches the
apex and subdomains; `chatgpt.com` is exact. `hosts = ["*"]` or an omitted list
intercepts every host reached by that child. First-class Claude/Codex commands
use built-in runtime scopes instead of configured `hosts` and do not apply
stored header rewrites.

First-class `rtr claude` and `rtr codex` runs refresh
`<profile home>/skills` before launching. If `skills_source` is configured, that
directory must exist and is copied after deleting the old destination. If it is
omitted, rtr defaults to `~/.claude/skills` or `~/.codex/skills`; a missing
default removes any stale destination and continues with no synced skills.
Relative `skills_source` paths resolve from the rtr config directory. Claude
skill folders may be symlinks; links within the copied tree keep their relative
form, while relative links outside it become absolute without resolving symlink
aliases. Their `SKILL.md` remains readable from every profile home and later
alias retargeting still takes effect. Codex symlink text is unchanged.

The config file is `0600`. Round-robin cursors and legacy switch state live in
`~/.local/state/rtr/state.toml`.

## Logging

Default launches inherit the terminal and create no per-run directory. With
`--log`, rtr prints and writes:

```text
~/.local/state/rtr/runs/<tool>/<timestamp-pid>/output.log
~/.local/state/rtr/runs/<tool>/<timestamp-pid>/rtr.log
```

The first file is the child transcript; the second is proxy diagnostics. Set
`RTR_LOG=debug` to increase diagnostic detail. Drop `--log` if a full-screen TUI
renders incorrectly.

## Troubleshooting

- **TLS or certificate errors** — run `rtr trust` for macOS trust-store clients;
  other stacks receive CA env vars automatically.
- **Proxy bind error** — another rtr process owns the configured port; stop it
  or set `[proxy] port = 0` for an ephemeral port.
- **No eligible profiles** — run `rtr add <tool> --profile <name>`.
- **A profile lacks preferences or skills** — native homes are isolated by
  design; configure `skills_source` for shared skill definitions. For Claude,
  keep profile-specific settings, commands, agents, and plugins inside that
  profile's `CLAUDE_CONFIG_DIR`; project `.claude/*` files need no copying.
- **Regenerating the CA** — run `rtr untrust` before deleting the CA files so an
  old trusted root does not remain in the keychain.
