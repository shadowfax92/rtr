# Usage

## Build & install

```sh
make                    # builds bin/rtr
make install            # installs to ~/.cargo/bin/rtr
make install PREFIX=/usr/local
```

Requires macOS (Apple Silicon) and a Rust toolchain. The default install path
is `~/.cargo/bin`; make sure it is on your `PATH`, or pass a different `PREFIX`.

## The setup walkthrough

The end-to-end flow for switching between multiple Codex or Claude accounts.

### 1. Initialize

```sh
rtr init
```

Writes `~/.config/rtr/config.toml` (with Codex and Claude examples) and mints a
local CA under `~/.config/rtr/ca/`.

### 2. Trust the CA (one time)

```sh
rtr trust
```

Adds the CA to your **login keychain** (no sudo). `codex` verifies TLS against
the macOS trust store, so this is required for it. (`security` may show a single
GUI auth prompt.) Use `rtr trust --system` only if a tool consults the system
trust domain exclusively (needs sudo).

### 3. Run setup

Run setup for the CLI you want to capture:

```sh
rtr setup codex
rtr setup claude
```

`setup` makes sure the config has a tool entry, prompts you to press Enter, then
launches the real CLI through the proxy with no active rewrite. Authenticate
inside the child CLI, make one request if needed, then exit it. `rtr` imports the
last captured `Authorization` header into the selected profile and makes it
active.

The default profile is `<tool>-1`; pass a profile name to import somewhere else:

```sh
rtr setup codex codex-2
rtr setup claude claude-2
```

### 4. Run with profiles

```sh
rtr codex                    # active/default Codex profile
rtr claude                   # active/default Claude profile
rtr claude claude-2          # one-shot override, does not change state
rtr switch claude claude-2   # persistent switch
```

The rewrite replaces the `Authorization` header on every request to the target
hosts before it reaches the upstream API.

### Manual capture reference

Every intercepted request is appended to a per-run capture file:

```sh
rtr status
cat ~/.local/state/rtr/runs/codex/*/capture.jsonl | tail -1
cat ~/.local/state/rtr/runs/claude/*/capture.jsonl | tail -1
```

`rtr auth list` groups auth-like headers by host/header and redacts values by
default:

```text
capture: /Users/me/.local/state/rtr/runs/codex/20260611-144724-7450/capture.jsonl
host        header          count  latest                    value
chatgpt.com authorization   27     2026-06-11T21:47:26Z      Bearer «redacted»
```

The capture file stores the **real** values so you can see exactly what to
replace. Use `rtr auth list codex --show-secrets` only when you intentionally
want the real values printed in your terminal.

### Manual import reference

If you want to inspect first or import a non-default host/header, use `auth`
directly:

```sh
rtr auth list codex
rtr auth import codex codex-1 --host chatgpt.com --header authorization
```

`auth import` writes the real value into `~/.config/rtr/config.toml` but does not
print it. If a capture has more than one auth-like header, the command asks you
to narrow the match with `--host` and/or `--header`.

## Claude-style captures

Claude can emit auth-like headers for Anthropic plus MCP/tool-service hosts in
one run. Inspect first:

```sh
rtr auth list claude
```

Then import the exact host/header you want. If the tool exists but the profile
does not yet, create it explicitly:

```sh
rtr auth import claude claude-1 --create-profile \
  --host api.anthropic.com --header authorization
rtr switch claude claude-1
```

## Commands

| Command | What it does |
| --- | --- |
| `rtr init [--force]` | Scaffold `config.toml` and mint the CA. |
| `rtr setup <tool> [profile]` | Capture the tool's auth header and import it into a profile. |
| `rtr <tool>` / `rtr run <tool> [-- args]` | Run the tool with interception. Extra args pass through. |
| `rtr <tool> <profile>` | Run the tool once with a profile override. |
| `rtr run --log <tool>` | Also pipe + tee the tool's stdout/stderr to `output.log` (may degrade TUIs). |
| `rtr run --show-secrets <tool>` | Don't redact secret header values in terminal output. |
| `rtr auth list <tool>` | Summarize auth-like headers from the latest capture for a tool. |
| `rtr auth list <tool> --capture <path>` | Inspect a specific `capture.jsonl`. |
| `rtr auth import <tool> <profile>` | Import one captured auth-like header into a profile's `set` rewrites. |
| `rtr auth import <tool> <profile> --create-profile` | Create the profile if the tool already exists. |
| `rtr switch <tool> <profile>` | Set the active profile. |
| `rtr switch <profile>` | Same, when the profile name is unique across tools. |
| `rtr status [tool]` | Show tools, active profiles, hosts, proxy port, CA fingerprint, trust state. |
| `rtr trust [--system]` | Trust the CA in the login (or system) keychain. |
| `rtr untrust [--system]` | Remove the CA's trust settings. |
| `rtr ca path` / `rtr ca show` | Print the CA cert path / PEM. |

## config.toml reference

```toml
[proxy]
port = 62888                 # local MITM port (127.0.0.1 only); 0 = ephemeral

[tools.<name>]
command = ["codex"]          # program + base args; user args are appended
hosts   = ["api.openai.com", "chatgpt.com"]   # only these are intercepted
# A host entry is either an exact hostname or a dot-prefixed suffix that also
# covers subdomains: ".chatgpt.com" matches chatgpt.com AND cdn.chatgpt.com
# (anchored on a dot boundary, so it never matches evilchatgpt.com). Exact
# entries do NOT match subdomains — use the dot form if a tool uses them.
# Use ["*"] — or omit `hosts` entirely — to intercept ALL of the tool's traffic
# (everything it sends is MITM'd, so the CA must be trusted). Only a bare "*" is
# the wildcard; "*.openai.com" is not a glob — use the dot form. Named hosts keep
# the blast radius small and are the recommended default.
active  = "codex-1"          # default active profile (overridden by `rtr switch`)

[tools.<name>.profiles.<profile>]
set    = { Authorization = "Bearer …", X-Org = "org-123" }   # overwrite/add
remove = ["X-Trace-Id"]                                       # delete
```

The file is created `0600` because it holds tokens. `rtr switch` writes the live
selection to `~/.local/state/rtr/state.toml`, never to this file, so your
comments and formatting survive.

`rtr auth import` rewrites `config.toml` through the same `0600` secret-file
path. It serializes the TOML model, so hand-written comments in that file are
not preserved.

## Environment variables

- `RTR_CONFIG_DIR`, `RTR_STATE_DIR` — override the config/state locations.
- `RTR_LOG` — `tracing` filter (e.g. `RTR_LOG=warn` to quiet per-request logs).

## Where the logs go

`rtr run` keeps the child's terminal clean by routing the proxy's own logs (and
hudsucker's) to `<run_dir>/rtr.log` rather than stderr. The per-run dir is printed
at startup (`rtr: logs -> …`). Set `RTR_LOG=debug` for more detail. This matters
for TUIs like `codex`, whose screen would otherwise be corrupted by log lines.

`capture.jsonl` stores original request headers before rewrites. Use `rtr auth
list <tool>` for a redacted summary and `rtr auth import <tool> <profile>` to
copy one selected captured value into config.

WebSocket traffic (e.g. codex's `chatgpt.com/backend-api/codex/responses`) is
intercepted and the auth header on the upgrade is rewritten like any other
request. rtr disables WebSocket compression (`permessage-deflate`) on intercepted
connections because the proxy can't re-frame compressed messages — uncompressed
WS works transparently.

## Troubleshooting

- **TLS handshake / certificate errors from the tool to a target host** — the CA
  isn't trusted for that tool. For codex-style (keychain) tools, run `rtr trust`.
  For OpenSSL/Node/Python/curl tools it should "just work" via env vars; confirm
  the tool isn't ignoring `SSL_CERT_FILE`/`NODE_EXTRA_CA_CERTS`.
- **"binding proxy … another rtr already running?"** — a previous run still holds
  the port, or change `[proxy] port`.
- **Nothing in `capture.jsonl`** — the tool didn't hit a configured host, or it
  ignores proxy env vars. Check `hosts` (set `["*"]` to intercept everything),
  and see the fallback note below.
- **TUI looks wrong with `--log`** — `--log` pipes stdout; drop it (default
  inherits the terminal). Captures don't need `--log`.
- **Regenerating the CA** — run `rtr untrust` *before* deleting the CA files and
  re-running `rtr init`, otherwise the old CA can linger as a trusted root in
  your keychain. `rtr init` on its own reuses the existing CA and is safe.

> Signals: `rtr` sets `kill_on_drop` so the child won't be orphaned if `rtr`
> exits abnormally, and a terminal Ctrl-C reaches the child via the shared
> process group. A dedicated SIGTERM→graceful-shutdown handler is a future
> addition.

## Limitation: proxy-ignoring binaries

`rtr` scopes interception by setting proxy env vars on the child, so a binary
that ignores `HTTPS_PROXY` won't be intercepted. The heavier fallback for such
binaries is system-wide transparent interception (pf `rdr` to a local
transparent proxy, or a `NETransparentProxyProvider` network extension) — out of
scope for v1 and deliberately avoided because it can't cleanly target a single
binary and carries a large blast radius. `codex` honors proxy env vars, so it
doesn't need this.
