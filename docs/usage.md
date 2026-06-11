# Usage

## Build & install

```sh
cargo build --release
cp target/release/rtr /usr/local/bin/   # or anywhere on PATH
```

Requires macOS (Apple Silicon) and a Rust toolchain.

## The codex walkthrough

The end-to-end flow for switching between multiple codex subscriptions.

### 1. Initialize

```sh
rtr init
```

Writes `~/.config/rtr/config.toml` (with a `codex` example) and mints a local CA
under `~/.config/rtr/ca/`.

### 2. Trust the CA (one time)

```sh
rtr trust
```

Adds the CA to your **login keychain** (no sudo). `codex` verifies TLS against
the macOS trust store, so this is required for it. (`security` may show a single
GUI auth prompt.) Use `rtr trust --system` only if a tool consults the system
trust domain exclusively (needs sudo).

### 3. Capture what codex sends

With the starter profiles empty, run codex through rtr to observe — without
changing — the real outbound auth header:

```sh
rtr codex
```

While codex runs, every intercepted request to `api.openai.com` / `chatgpt.com`
is appended to a per-run capture file. After it exits:

```sh
rtr status                              # shows the captures path under runs/
cat ~/.local/state/rtr/runs/codex/*/capture.jsonl | tail -1
```

Each line is one request, e.g.:

```json
{"ts":"…","method":"POST","url":"https://api.openai.com/v1/responses",
 "host":"api.openai.com","headers":[["authorization","Bearer sk-real-token…"], …]}
```

The capture file stores the **real** values so you can see exactly what to
replace. (Terminal output redacts secrets unless you pass `--show-secrets`.)

### 4. Author the swap

Edit `~/.config/rtr/config.toml` and paste each subscription's token:

```toml
[tools.codex.profiles.codex-1]
set = { Authorization = "Bearer sk-token-for-subscription-1" }

[tools.codex.profiles.codex-2]
set = { Authorization = "Bearer sk-token-for-subscription-2" }
```

### 5. Switch and run

```sh
rtr switch codex codex-1     # or: rtr switch codex-1  (name is unique)
rtr codex                    # codex now talks to OpenAI with codex-1's token

rtr switch codex-2
rtr codex                    # …now with codex-2's token
```

The rewrite replaces the `Authorization` header on every request to the target
hosts before it reaches OpenAI.

## Commands

| Command | What it does |
| --- | --- |
| `rtr init [--force]` | Scaffold `config.toml` and mint the CA. |
| `rtr <tool>` / `rtr run <tool> [-- args]` | Run the tool with interception. Extra args pass through. |
| `rtr run --log <tool>` | Also pipe + tee the tool's stdout/stderr to `output.log` (may degrade TUIs). |
| `rtr run --show-secrets <tool>` | Don't redact secret header values in terminal output. |
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
active  = "codex-1"          # default active profile (overridden by `rtr switch`)

[tools.<name>.profiles.<profile>]
set    = { Authorization = "Bearer …", X-Org = "org-123" }   # overwrite/add
remove = ["X-Trace-Id"]                                       # delete
```

The file is created `0600` because it holds tokens. `rtr switch` writes the live
selection to `~/.local/state/rtr/state.toml`, never to this file, so your
comments and formatting survive.

## Environment variables

- `RTR_CONFIG_DIR`, `RTR_STATE_DIR` — override the config/state locations.
- `RTR_LOG` — `tracing` filter (e.g. `RTR_LOG=warn` to quiet per-request logs).

## Where the logs go

`rtr run` keeps the child's terminal clean by routing the proxy's own logs (and
hudsucker's) to `<run_dir>/rtr.log` rather than stderr. The per-run dir is printed
at startup (`rtr: logs -> …`). Set `RTR_LOG=debug` for more detail. This matters
for TUIs like `codex`, whose screen would otherwise be corrupted by log lines.

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
  ignores proxy env vars. Check `hosts`, and see the fallback note below.
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
