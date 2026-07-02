<div align="center">

# 🔁 rtr

**Per-binary profile launcher and MITM capture for Claude Code and Codex subscriptions.**

*Give each subscription its own native tool home, capture its traffic, rotate profiles on each run.*

</div>

`rtr` launches Claude Code or Codex with a selected per-profile native home,
points only that child process at a local man-in-the-middle proxy, and captures
the outbound auth traffic it sends. It is built for switching between multiple
subscriptions without mutating global tool auth state or changing system-wide
networking.

- **Process-scoped** — only the spawned child gets `HTTPS_PROXY`; no VPN,
  routing table, kernel extension, or system-wide interception
- **Capture first** — run a tool once to record its real headers in
  `capture.jsonl` for inspection/import
- **Native profile homes** — first-class `rtr claude` / `rtr codex` set
  `CLAUDE_CONFIG_DIR` / `CODEX_HOME` under `~/.local/state/rtr/homes/...`
- **Subscription profiles** — `rtr claude` and `rtr codex` select enabled
  profiles with equal round-robin by default, or `--profile/-p` for one run
- **Host-scoped MITM** — first-class Claude/Codex commands use built-in target
  hosts for capture/logging; custom `rtr run` tools use configured hosts and
  legacy header rewrites
- **Local CA** — `rtr` mints a per-user CA and tells the child how to trust it
  through env vars, with `rtr trust` for macOS trust-store clients
- **TUI-friendly logs** — proxy logs and captures go to the run directory instead
  of corrupting full-screen tools

---

## Install

Requires macOS / Apple Silicon and a Rust toolchain.

```sh
make                         # builds bin/rtr
make install                 # installs to ~/.cargo/bin/rtr
make install PREFIX=/usr/local
```

The default install path is `~/.cargo/bin`; make sure it is on your `PATH`, or
pass a different `PREFIX`.

## Quick Start

```sh
rtr init                      # create ~/.config/rtr/config.toml and mint a local CA
rtr trust                     # trust the CA in your login keychain for keychain clients
rtr capture codex --profile personal   # log in inside this profile's CODEX_HOME
rtr import codex --profile personal --from-capture ~/.local/state/rtr/runs/codex/.../capture.jsonl
rtr codex                     # run Codex through the selected subscription profile
```

Claude uses the same split capture/import workflow:

```sh
rtr capture claude --profile work
rtr import claude --profile work --from-capture ~/.local/state/rtr/runs/claude/.../capture.jsonl
rtr claude --profile work
```

## Why It Works

`rtr` owns the child process, so it can set both the tool's native home env and
the proxy env only for that process. First-class profile identity comes from:

```text
CODEX_HOME=<state>/homes/codex/<profile>
CLAUDE_CONFIG_DIR=<state>/homes/claude/<profile>
```

Those dirs are created owner-only and are separate from global `~/.codex` or the
shared Claude config. TLS is intercepted with a CA that `rtr` mints locally.

Tools that read CA env vars trust the CA without touching the keychain:

```text
SSL_CERT_FILE
NODE_EXTRA_CA_CERTS
REQUESTS_CA_BUNDLE
CURL_CA_BUNDLE
GIT_SSL_CAINFO
```

Tools that verify against the macOS trust store, including `codex` via
`rustls-platform-verifier`, need one-time login-keychain trust:

```sh
rtr trust
```

`rtr run` prints this hint when the active tool needs it.

## Commands

### Setup

```sh
rtr init [--force]            # scaffold config.toml and mint/load the CA
```

### Capture and Import

```sh
rtr capture claude --profile work
rtr import claude --profile work --from-capture /path/to/capture.jsonl
rtr capture codex --profile personal
rtr import codex --profile personal --from-capture /path/to/capture.jsonl
rtr import codex --profile personal --from-capture /path/to/capture.jsonl --show-secrets
```

### Run Profiles

```sh
rtr claude                    # round-robin across enabled Claude profiles
rtr claude --profile work     # force one profile for this run only
rtr claude -p work
rtr claude --preset opus-max -- extra args
rtr codex
rtr codex --profile personal
rtr codex --preset gpt55-xhigh -- extra args
rtr run codex -- --login      # legacy generic run path still exists
```

### Inspect

```sh
rtr ls                        # list Claude/Codex profiles and presets
rtr show claude/work
rtr show claude/work --show-secrets
rtr stats --today             # per-profile run counts and failed-run %
rtr status [tool]             # legacy status: tool, profile, host, CA, trust state
cat ~/.local/state/rtr/runs/codex/*/capture.jsonl | tail -1
tail -f ~/.local/state/rtr/runs/codex/*/rtr.log
```

`rtr switch` remains for the lower-level `rtr run <tool>` path. First-class
`rtr claude` and `rtr codex` ignore persistent active-profile overrides and use
one-run forced profiles or round-robin selection.

### Trust / CA Management

```sh
rtr trust                     # trust the CA in the login keychain
rtr trust --system            # trust the CA in the system keychain with sudo
rtr untrust                   # remove login-keychain trust
rtr ca path                   # print the CA certificate path
rtr ca show                   # print the CA certificate PEM
```

## Config

Location: `~/.config/rtr/config.toml`. The file is created `0600` because it can
hold tokens.

```toml
[proxy]
port = 62888

[tools.codex]
command = ["codex"]
hosts = ["chatgpt.com"]
selection = "round-robin"
default_preset = "gpt55-xhigh"

[tools.codex.presets.gpt55-xhigh]
args = ["-m", "gpt-5.5", "-c", "model_reasoning_effort=xhigh"]

[tools.codex.profiles.personal]
set = {}

[tools.claude]
command = ["claude"]
hosts = [".anthropic.com"]
selection = "round-robin"

[tools.claude.profiles.work]
set = {}
[tools.claude.profiles.work.metadata]
x-organization-uuid = "captured-for-display-only"
```

| Field | Description |
|-------|-------------|
| `command` | Program and base args to spawn; user args are appended |
| `hosts` | Exact hostnames or dot-prefixed suffixes for legacy/custom `rtr run` interception |
| `selection` | `round-robin` for first-class subscription commands |
| `default_preset` | Named preset used when `--preset` is omitted |
| `presets.<name>.args` | Args inserted after `command` and before CLI trailing args |
| `enabled` | Optional profile flag; absent means enabled |
| `set` | Legacy/custom `rtr run` headers to add or overwrite before forwarding upstream |
| `remove` | Headers to delete before forwarding upstream |
| `metadata` | Captured/displayed metadata that is not rewritten |

`rtr switch` writes the live selection to
`~/.local/state/rtr/state.toml`, so comments and formatting in `config.toml`
survive.

## Run Files

Each run writes under `~/.local/state/rtr/runs/<tool>/<timestamp-pid>/`:

| File | Contents |
|------|----------|
| `capture.jsonl` | One JSON object per intercepted request, with original headers |
| `rtr.log` | Proxy and `hudsucker` logs kept off the child's terminal |
| `output.log` | Child stdout/stderr transcript, only when `--log` is used |

`capture.jsonl` always stores the original header values. Request previews in
`rtr.log` are redacted unless you run with `--log --show-secrets`.

Subscription run usage is appended to `~/.local/state/rtr/usage.jsonl` so
`rtr stats --today` can report distribution and failed-run percentages.

First-class profile homes live under
`~/.local/state/rtr/homes/<tool>/<profile>/`. `rtr codex` sets `CODEX_HOME` to
that directory; `rtr claude` sets `CLAUDE_CONFIG_DIR`. First-class runs do not
mutate global `~/.codex` or shared Claude config.

Interception is **host-scoped**. First-class `rtr claude` / `rtr codex` runs use
the built-in runtime hosts (`.anthropic.com` and exact `chatgpt.com`) for scoped
capture/logging and do not apply imported auth headers as runtime rewrites.
Legacy/custom
`rtr run <tool>` intercepts the hosts listed in that tool's `config.toml` entry;
set `hosts = ["*"]` — or omit `hosts` — to intercept *all* of that tool's
traffic (still only the spawned child, never system-wide).

## Docs

- [docs/usage.md](docs/usage.md) — install, the codex walkthrough, full command
  reference, config details, and troubleshooting
- [docs/design.md](docs/design.md) — chosen approach, rejected alternatives, and
  trust model
- [docs/architecture.md](docs/architecture.md) — modules, request flow, and
  on-disk layout

## Status

macOS / Apple Silicon. Built in Rust on
[`hudsucker`](https://crates.io/crates/hudsucker). Per-binary scoping is via
proxy env vars; v1 deliberately does not do system-wide interception.

---

> Personal tool built for my own workflow. It intercepts real auth headers, so
> read the trust model before adapting it.
