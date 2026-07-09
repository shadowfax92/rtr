<div align="center">

# 🔁 rtr

**Per-binary profile launcher for Claude Code and Codex subscriptions.**

*Give each subscription its own native tool home and rotate profiles on each run.*

</div>

`rtr` launches Claude Code or Codex with a selected per-profile native home. It
is built for switching between subscriptions without mutating global tool auth
state or changing system-wide networking.

- **Native profile homes** — `rtr claude` and `rtr codex` set
  `CLAUDE_CONFIG_DIR` or `CODEX_HOME` under
  `~/.local/state/rtr/homes/<tool>/<profile>/`
- **Fresh skills sync** — each run replaces only `<profile home>/skills` from
  the tool default or configured source
- **Simple onboarding** — `rtr add` creates a profile and launches the tool to
  log in inside its native home
- **Subscription profiles** — choose one profile with `--profile/-p` or use
  equal round-robin selection across enabled profiles
- **Process-scoped proxy** — only the spawned child gets proxy and CA env vars;
  there is no VPN, routing-table change, or system-wide interception
- **Host-scoped rewrites** — first-class commands use built-in target hosts;
  legacy/custom `rtr run` tools use configured hosts and header rewrites
- **Artifact-free defaults** — normal launches inherit the terminal and do not
  create per-run capture or log files
- **Opt-in transcripts** — `--log` creates a private `output.log` and proxy
  diagnostics for the requested run

## Install

Requires macOS / Apple Silicon and a Rust toolchain.

```sh
make
make install
make install PREFIX=/usr/local
```

The default install path is `~/.cargo/bin`; make sure it is on `PATH`, or pass a
different `PREFIX`.

## Quick start

```sh
rtr init
rtr trust
rtr add codex --profile personal
rtr add claude --profile work
```

`rtr add` creates the profile and immediately launches the selected native home
for the tool's login flow. Run it again later with:

```sh
rtr codex --profile personal
rtr claude --profile work
```

## Why it works

The login state stays inside that profile's `CODEX_HOME` or
`CLAUDE_CONFIG_DIR`. Global `~/.codex` and shared Claude config are not mutated.
For Claude, rtr sets both config and secure-storage namespaces to the same home:

```text
CODEX_HOME=<state>/homes/codex/<profile>
CLAUDE_CONFIG_DIR=<state>/homes/claude/<profile>
CLAUDE_SECURESTORAGE_CONFIG_DIR=<state>/homes/claude/<profile>
<native home>/skills refreshed before launch
```

For Claude Code, `CLAUDE_CONFIG_DIR` is the documented user config boundary for
settings, app state, session history, plugins, and side-by-side accounts. Claude
stores credential files there on Linux and Windows; on macOS the credential
secret remains in Keychain. Claude Code 2.1.205 qualifies its Keychain service
by config directory, so each `rtr` profile still gets a distinct login entry.
`rtr` pins both Claude config and secure-storage namespaces to that boundary but
never reads or copies Claude credentials.

## Commands

```sh
rtr init [--force]

rtr add claude --profile work
rtr add codex --profile personal
rtr claude
rtr claude --profile work
rtr claude -p work
rtr codex
rtr codex --profile personal

rtr ls
rtr show claude/work
rtr show claude/work --show-secrets
rtr stats --today

rtr run <tool> [-- tool args...]
rtr switch <tool> <profile>
rtr status [tool]

rtr trust [--system]
rtr untrust [--system]
rtr ca path
rtr ca show
```

Tool arguments follow the configured command. Put rtr-owned flags
(`--profile/-p` and `--log`) before tool args. Use `--` when the tool itself
needs one of those names.

```sh
rtr claude --effort xhigh --model claude-fable-5
rtr codex --dangerously-bypass-approvals-and-sandbox -m gpt-5.5
rtr codex --log -- --profile native-tool-flag
```

`rtr switch` applies to the lower-level `rtr run <tool>` path. First-class
Claude and Codex commands use one-run forced profiles or round-robin selection.

## Config

Location: `~/.config/rtr/config.toml`. The file is created `0600` because legacy
rewrite profiles can contain tokens.

```toml
[proxy]
port = 62888

[tools.codex]
command = ["codex"]
hosts = ["chatgpt.com"]
selection = "round-robin"
skills_source = "~/.skills"

[tools.codex.profiles.personal]
enabled = true

[tools.claude]
command = ["claude"]
hosts = [".anthropic.com"]
selection = "round-robin"
skills_source = "~/.skills"

[tools.claude.profiles.work]
enabled = true
```

| Field | Description |
| --- | --- |
| `command` | Program and base args to spawn; user args are appended |
| `hosts` | Exact hosts or dot-prefixed suffixes for legacy/custom interception |
| `selection` | `round-robin` for first-class Claude/Codex commands |
| `skills_source` | Optional directory copied fresh to `<profile home>/skills` |
| `enabled` | Optional profile flag; absent means enabled |
| `set` / `remove` | Legacy/custom headers to overwrite or delete |
| `metadata` | Stored profile metadata that is never rewritten |

First-class runs ignore stored header rewrites and use the selected native home
as the identity boundary. If `skills_source` is omitted, rtr uses
`~/.codex/skills` or `~/.claude/skills`; a missing default means no synced
skills. Relative sources resolve from the rtr config directory.

## State and logs

```text
~/.local/state/rtr/
  state.toml
  usage.jsonl
  homes/
    codex/<profile>/
    claude/<profile>/
```

Normal launches do not create a run directory. With `--log`, rtr creates:

```text
~/.local/state/rtr/runs/<tool>/<timestamp-pid>/
  output.log
  rtr.log
```

`output.log` contains the child's stdout/stderr transcript. `rtr.log` contains
proxy diagnostics. Both are opt-in; `--log` can degrade full-screen TUIs.
`RTR_LOG` controls the diagnostics filter for that explicit logging path.

First-class profile homes live under
`~/.local/state/rtr/homes/<tool>/<profile>/`. `rtr codex` sets `CODEX_HOME` to
that directory; `rtr claude` sets `CLAUDE_CONFIG_DIR`. First-class runs do not
mutate global `~/.codex` or shared Claude config. Before launching, they replace
`<profile home>/skills` from `skills_source` when configured, otherwise from
`~/.codex/skills` or `~/.claude/skills`. Explicit sources must exist; missing
defaults simply leave no synced skills. Relative `skills_source` paths resolve
from the rtr config directory. For Claude, this seeds personal skills only:
settings, commands, agents, plugins, auth state, and sessions stay owned by the
selected profile. Project `.claude/skills` and other project configuration still
load from the working tree. Symlinked skill directories remain linked to their
original targets after the copy.

## Trust model

`rtr` points only the child process at its local proxy. The child receives
`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `SSL_CERT_FILE`,
`NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`, and
`GIT_SSL_CAINFO`.

Tools that verify against the macOS trust store need a one-time:

```sh
rtr trust
```

The CA is per-user, its private key is stored `0600`, and trust can be removed
with `rtr untrust`.

## Docs

- [docs/usage.md](docs/usage.md) — profile setup, commands, config, and troubleshooting
- [docs/design.md](docs/design.md) — chosen approach and trust model
- [docs/architecture.md](docs/architecture.md) — modules, runtime flow, and storage

## Status

macOS / Apple Silicon. Built in Rust on
[`hudsucker`](https://crates.io/crates/hudsucker). Per-binary scoping is via
proxy env vars; rtr deliberately does not perform system-wide interception.
