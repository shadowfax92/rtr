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

Add profiles to `~/.config/rtr/config.toml`:

```toml
[tools.codex.profiles.personal]
enabled = true

[tools.claude.profiles.work]
enabled = true
```

Launch each profile and complete the tool's normal login flow:

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

Tool arguments are appended to the configured command:

```sh
rtr claude --effort xhigh --model claude-fable-5
rtr codex --dangerously-bypass-approvals-and-sandbox -m gpt-5.5
```

Put rtr-owned flags (`--profile/-p` and `--log`) before tool args. Use `--` if
the child tool needs one of those same flag names.

## Commands

| Command | What it does |
| --- | --- |
| `rtr init [--force]` | Scaffold `config.toml` and mint the CA. |
| `rtr claude [--profile/-p <name>] [tool args...]` | Run Claude with forced or round-robin selection. |
| `rtr codex [--profile/-p <name>] [tool args...]` | Run Codex with forced or round-robin selection. |
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

## Legacy offline import

The import command accepts historical request JSONL created by an older rtr
version or a compatible external tool:

```sh
rtr import claude --profile work --from-capture /path/to/requests.jsonl
rtr import codex --profile personal --from-capture /path/to/requests.jsonl
```

This is compatibility for legacy/custom header-rewrite profiles. Current rtr
runs do not produce request captures, and first-class Claude/Codex identity
comes from the native home.

Claude import recognizes `Authorization` from `api.anthropic.com` or
`mcp-proxy.anthropic.com` and stores `x-organization-uuid` as metadata. Codex
requires an unambiguous `Authorization` plus `chatgpt-account-id` bundle from
exact `chatgpt.com`; it ignores `ab.chatgpt.com` telemetry and cookies.

Existing profiles prompt before overwrite. Use `--force` for scripts or
`--no-overwrite` to reject conflicts. Output is redacted unless
`--show-secrets` is requested.

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
- **No eligible profiles** — add an enabled profile table to `config.toml`.
- **A profile lacks preferences or skills** — native homes are isolated by
  design; configure `skills_source` for shared skill definitions.
- **Imported rewrites are missing** — the historical JSONL must contain one
  complete, unambiguous tool-specific auth bundle.
- **Regenerating the CA** — run `rtr untrust` before deleting the CA files so an
  old trusted root does not remain in the keychain.
