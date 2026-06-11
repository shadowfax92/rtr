<div align="center">

# 🔁 rtr

**Per-binary MITM header rewriter for swapping auth profiles.**

*Capture what one tool sends, rewrite it on the next run.*

</div>

`rtr` launches one configured binary, points only that child process at a local
man-in-the-middle proxy, captures the outbound auth headers it sends, and can
rewrite those headers from a selected profile. It is built for workflows like
switching between multiple `codex` subscriptions without changing system-wide
networking.

- **Process-scoped** — only the spawned child gets `HTTPS_PROXY`; no VPN,
  routing table, kernel extension, or system-wide interception
- **Capture first** — run a tool once to record its real headers in
  `capture.jsonl`, then decide what to rewrite
- **Profile switching** — `rtr switch` flips the active profile for the next run,
  while your hand-authored `config.toml` stays intact
- **Host-scoped MITM** — only configured hosts are decrypted; everything else is
  blind-tunneled
- **Local CA** — `rtr` mints a per-user CA and tells the child how to trust it
  through env vars, with `rtr trust` for macOS trust-store clients
- **TUI-friendly logs** — proxy logs and captures go to the run directory instead
  of corrupting full-screen tools

---

## Install

Requires macOS / Apple Silicon and a Rust toolchain.

```sh
cargo build --release
cp target/release/rtr /usr/local/bin/   # or anywhere on PATH
```

## Quick Start

```sh
rtr init                      # create ~/.config/rtr/config.toml and mint a local CA
rtr trust                     # trust the CA in your login keychain for codex-style clients
rtr codex                     # run codex through the proxy and capture real headers
rtr status                    # show profiles, hosts, CA fingerprint, and trust state
```

After the first run, inspect the captured header:

```sh
cat ~/.local/state/rtr/runs/codex/*/capture.jsonl | tail -1
```

Paste the headers you want to swap into `~/.config/rtr/config.toml`, then switch
profiles:

```sh
rtr switch codex codex-2      # make subscription #2 active for codex
rtr codex                     # codex now talks to OpenAI with codex-2's token
```

## Why It Works

`rtr` owns the child process, so it can scope interception to that process by
setting proxy env vars such as `HTTPS_PROXY`. TLS is intercepted with a CA that
`rtr` mints locally.

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

### Run and Capture

```sh
rtr codex                     # bare-tool alias for: rtr run codex
rtr run codex -- --login      # pass args through to the child
rtr run --log codex           # tee child output and write redacted request previews
rtr run --log --show-secrets codex  # write unredacted request previews to rtr.log
```

### Switch Profiles

```sh
rtr switch codex codex-1      # explicit tool + profile
rtr switch codex-2            # profile-only form, when the name is unique
```

### Inspect

```sh
rtr status [tool]             # show tool, profile, host, CA, and trust state
cat ~/.local/state/rtr/runs/codex/*/capture.jsonl | tail -1
tail -f ~/.local/state/rtr/runs/codex/*/rtr.log
```

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
hosts = ["api.openai.com", "chatgpt.com"]
active = "codex-1"

[tools.codex.profiles.codex-1]
set = { Authorization = "Bearer sk-token-for-subscription-1" }

[tools.codex.profiles.codex-2]
set = { Authorization = "Bearer sk-token-for-subscription-2" }
```

| Field | Description |
|-------|-------------|
| `command` | Program and base args to spawn; user args are appended |
| `hosts` | Exact hostnames or dot-prefixed suffixes to intercept |
| `active` | Default active profile, overridden by `rtr switch` state |
| `set` | Headers to add or overwrite before forwarding upstream |
| `remove` | Headers to delete before forwarding upstream |

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
