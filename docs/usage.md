# Usage

## Build & install

```sh
make                    # builds bin/rtr
make install            # installs to ~/.cargo/bin/rtr
make install PREFIX=/usr/local
```

Requires macOS (Apple Silicon) and a Rust toolchain. The default install path
is `~/.cargo/bin`; make sure it is on your `PATH`, or pass a different `PREFIX`.

## Subscription profile workflow

The first-class workflow supports Claude Code and Codex.

### 1. Initialize

```sh
rtr init
```

Writes `~/.config/rtr/config.toml` with Claude/Codex entries and mints a local
CA under `~/.config/rtr/ca/`.

### 2. Trust the CA (one time)

```sh
rtr trust
```

Adds the CA to your **login keychain** (no sudo). Keychain-verifying clients
need this before intercepted TLS works. (`security` may show a single GUI auth
prompt.) Use `rtr trust --system` only if a tool consults the system trust
domain exclusively (needs sudo).

### 3. Capture one subscription

Capture launches the tool through rtr with no rewrites. Follow the printed
logout/login/send-hello/exit instructions so the capture contains the target
subscription's auth-bearing requests.

```sh
rtr capture claude --profile work
rtr capture codex --profile personal
```

After the child exits, rtr prints the capture path and exact import command:

```sh
rtr import codex --profile personal --from-capture ~/.local/state/rtr/runs/codex/.../capture.jsonl
```

Each capture line is one request, e.g.:

```json
{"ts":"...","method":"GET","url":"https://chatgpt.com/backend-api/codex/models",
 "host":"chatgpt.com","headers":[["authorization","Bearer ..."],["chatgpt-account-id","..."]]}
```

The capture file stores the real values. Import/show output redacts them unless
you pass `--show-secrets`.

### 4. Import the auth bundle

Import extracts the tool-specific auth bundle and saves it into
`~/.config/rtr/config.toml`:

```sh
rtr import claude --profile work --from-capture /path/to/capture.jsonl
rtr import codex --profile personal --from-capture /path/to/capture.jsonl
```

If the profile already exists, import prompts before overwriting. Use `--force`
for scripts or `--no-overwrite` to reject conflicts without prompting.

Claude imports:

- required rewrite: `Authorization`
- metadata only: `x-organization-uuid` when present
- runtime host scope: `.anthropic.com`

Codex imports:

- required rewrites: `Authorization`, `chatgpt-account-id`
- ignored: `Cookie`, `ab.chatgpt.com` telemetry, `statsig-api-key`
- runtime host scope: exact `chatgpt.com`

### 5. Run with profiles

```sh
rtr claude                   # equal round-robin across enabled Claude profiles
rtr claude --profile work    # force one profile for this run only
rtr claude -p work
rtr codex
rtr codex --profile personal
```

Every selected run is recorded, successful or failed. `rtr stats --today` shows
per-profile run counts and failed-run percentages.

### 6. Presets and trailing args

Tool presets live under the tool, not under profiles:

```toml
[tools.codex]
command = ["codex"]
default_preset = "gpt55-xhigh"

[tools.codex.presets.gpt55-xhigh]
args = ["-m", "gpt-5.5", "-c", "model_reasoning_effort=xhigh"]
```

Runtime order is:

```text
configured command + preset args + trailing CLI args
```

Examples:

```sh
rtr claude --preset opus-max -- extra args
rtr codex --preset gpt55-xhigh -- extra args
```

## Commands

| Command | What it does |
| --- | --- |
| `rtr init [--force]` | Scaffold `config.toml` and mint the CA. |
| `rtr capture <tool> --profile <name>` | Launch Claude/Codex with no rewrites and capture auth traffic. |
| `rtr import <tool> --profile <name> --from-capture <path>` | Extract and save a subscription auth bundle. |
| `rtr claude [--profile/-p <name>] [--preset <name>] [-- args]` | Run Claude with forced or round-robin profile selection. |
| `rtr codex [--profile/-p <name>] [--preset <name>] [-- args]` | Run Codex with forced or round-robin profile selection. |
| `rtr ls` | List configured Claude/Codex profiles and presets. |
| `rtr show <tool>/<profile> [--show-secrets]` | Show one profile, redacted by default. |
| `rtr stats [--today]` | Show per-profile run counts and failure percentages. |
| `rtr <tool>` / `rtr run <tool> [-- args]` | Legacy generic run path for other configured tools. |
| `rtr run --log <tool>` | Also pipe + tee the tool's stdout/stderr to `output.log` (may degrade TUIs). |
| `rtr run --show-secrets <tool>` | Don't redact secret header values in terminal output. |
| `rtr switch <tool> <profile>` | Set the active profile for the legacy `rtr run` path. |
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
hosts   = ["chatgpt.com"]    # legacy/custom rtr run intercept scope
# A host entry is either an exact hostname or a dot-prefixed suffix that also
# covers subdomains: ".chatgpt.com" matches chatgpt.com AND cdn.chatgpt.com
# (anchored on a dot boundary, so it never matches evilchatgpt.com). Exact
# entries do NOT match subdomains — use the dot form if a tool uses them.
# Use ["*"] — or omit `hosts` entirely — to intercept ALL of the tool's traffic
# (everything it sends is MITM'd, so the CA must be trusted). Only a bare "*" is
# the wildcard; "*.openai.com" is not a glob — use the dot form. Named hosts keep
# the blast radius small and are the recommended default.
# First-class rtr claude/codex runs use built-in runtime hosts instead.
selection = "round-robin"    # first-class claude/codex runtime selection
default_preset = "xhigh"

[tools.<name>.presets.xhigh]
args = ["-m", "gpt-5.5"]

[tools.<name>.profiles.<profile>]
enabled = true                                               # default if omitted
set    = { Authorization = "Bearer …" }                      # overwrite/add
remove = ["X-Trace-Id"]                                       # delete
[tools.<name>.profiles.<profile>.metadata]
x-organization-uuid = "stored for display, not rewritten"
```

The file is created `0600` because it holds tokens. Round-robin cursors and
legacy `rtr switch` state live in `~/.local/state/rtr/state.toml`.

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
  ignores proxy env vars. For first-class capture, check that you completed the
  printed login/hello/exit flow; for generic runs, check `hosts` (set `["*"]` to
  intercept everything), and see the fallback note below.
- **Import says a required field is missing** — the capture did not include the
  target backend traffic. Claude needs Anthropic-family requests with
  `Authorization`; Codex needs exact `chatgpt.com` requests with both
  `Authorization` and `chatgpt-account-id`.
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
