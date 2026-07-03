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

The first-class workflow supports Claude Code and Codex. Each profile owns a
native tool home under `~/.local/state/rtr/homes/...`; rtr selects that home for
the spawned child with `CLAUDE_CONFIG_DIR` or `CODEX_HOME`.

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

### 3. Create and log into a profile

Capture creates the named profile if it is missing, launches the tool with that
profile's native home, and applies no rewrites. Log in to the target
subscription, send `hello`, then exit. The selected home now contains the
profile's login state.

```sh
rtr capture claude --profile work
rtr capture codex --profile personal
```

After the child exits, rtr prints the capture path and the command to run the
profile:

```sh
rtr codex --profile personal
```

Each capture line is one request, e.g.:

```json
{"ts":"...","method":"GET","url":"https://chatgpt.com/backend-api/codex/models",
 "host":"chatgpt.com","headers":[["authorization","Bearer ..."],["chatgpt-account-id","..."]]}
```

The capture file stores the real values. Show/import output redacts them unless
you pass `--show-secrets`.

### 4. Run with profiles

```sh
rtr claude                   # equal round-robin across enabled Claude profiles
rtr claude --profile work    # force one profile for this run only
rtr claude -p work
rtr codex
rtr codex --profile personal
```

`rtr codex` creates/uses `~/.local/state/rtr/homes/codex/<profile>/` and sets
`CODEX_HOME` for the child. `rtr claude` creates/uses
`~/.local/state/rtr/homes/claude/<profile>/` and sets `CLAUDE_CONFIG_DIR`.
Before spawning, rtr replaces `<profile home>/skills` from the tool default or
configured source. Global `~/.codex` and shared Claude config are not mutated by
first-class runs.

Every selected run is recorded, successful or failed. `rtr stats --today` shows
per-profile run counts and failed-run percentages.

### 5. Optional legacy header import

First-class `rtr claude` and `rtr codex` do not use captured bearer headers as
the runtime account switch; the selected native home is the source of truth.
Import remains available for legacy/custom `rtr run` profiles that still opt
into header rewrites.

Import extracts the tool-specific legacy auth bundle and saves rewrite metadata
in `~/.config/rtr/config.toml`:

```sh
rtr import claude --profile work --from-capture /path/to/capture.jsonl
rtr import codex --profile personal --from-capture /path/to/capture.jsonl
```

If the profile already exists, import prompts before overwriting. Use `--force`
for scripts or `--no-overwrite` to reject conflicts without prompting.

If a capture does not include legacy auth headers, import still registers an
enabled native-home profile with no runtime rewrites as long as it contains
matching tool traffic.

Claude legacy import recognizes:

- captured legacy rewrite: `Authorization`
- metadata only: `x-organization-uuid` when present
- runtime host scope: `.anthropic.com`

Codex legacy import recognizes:

- captured legacy rewrites: `Authorization`, `chatgpt-account-id`
- ignored: `Cookie`, `ab.chatgpt.com` telemetry, `statsig-api-key`
- runtime host scope: exact `chatgpt.com`

### 6. Per-run tool args

Runtime order is:

```text
configured command + per-run tool args
```

Examples:

```sh
rtr claude --effort xhigh --model claude-fable-5 --dangerously-skip-permissions
rtr codex --dangerously-bypass-approvals-and-sandbox -m gpt-5.5 -c model_reasoning_effort=xhigh
```

Tool flags that rtr does not own can be passed directly. Put rtr-owned flags
(`--profile/-p`, `--log`, `--show-secrets`) before tool args. If the tool itself
needs one of those same flag names, put `--` before the tool args.

## Commands

| Command | What it does |
| --- | --- |
| `rtr init [--force]` | Scaffold `config.toml` and mint the CA. |
| `rtr capture <tool> --profile <name>` | Create/use a Claude/Codex profile, launch it with its native home, and capture traffic. |
| `rtr import <tool> --profile <name> --from-capture <path>` | Legacy/custom: extract captured headers into rewrite settings. |
| `rtr claude [--profile/-p <name>] [tool args...]` | Run Claude with forced or round-robin profile selection. |
| `rtr codex [--profile/-p <name>] [tool args...]` | Run Codex with forced or round-robin profile selection. |
| `rtr ls` | List configured Claude/Codex profiles. |
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
skills_source = "~/.skills"  # optional: copied fresh to <profile home>/skills

[tools.<name>.profiles.<profile>]
enabled = true                                               # default if omitted
set    = { Authorization = "Bearer …" }                      # legacy rtr run overwrite/add
remove = ["X-Trace-Id"]                                       # delete
[tools.<name>.profiles.<profile>.metadata]
x-organization-uuid = "stored for display, not rewritten"
```

The file is created `0600` because it holds tokens. Round-robin cursors and
legacy `rtr switch` state live in `~/.local/state/rtr/state.toml`.

First-class `rtr claude` and `rtr codex` runs refresh
`<profile home>/skills` before launching. If `skills_source` is configured, that
directory must exist and is copied after deleting the old destination. If it is
omitted, rtr defaults to `~/.claude/skills` or `~/.codex/skills`; a missing
default removes any stale destination and continues with no synced skills.
Relative `skills_source` paths resolve from the rtr config directory.

## Environment variables

- `RTR_CONFIG_DIR`, `RTR_STATE_DIR` — override the config/state locations.
- `RTR_LOG` — `tracing` filter (e.g. `RTR_LOG=warn` to quiet per-request logs).

## Where the logs go

`rtr run` keeps the child's terminal clean by routing the proxy's own logs (and
hudsucker's) to `<run_dir>/rtr.log` rather than stderr. The per-run dir is printed
at startup (`rtr: logs -> …`). Set `RTR_LOG=debug` for more detail. This matters
for TUIs like `codex`, whose screen would otherwise be corrupted by log lines.

WebSocket traffic (e.g. codex's `chatgpt.com/backend-api/codex/responses`) is
intercepted/captured for first-class runs. Legacy/custom `rtr run` rewrites also
apply to the upgrade request. rtr disables WebSocket compression
(`permessage-deflate`) on intercepted connections because the proxy can't
re-frame compressed messages — uncompressed WS works transparently.

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
- **Import saved no legacy rewrites** — first-class runs still use the selected
  native home for identity. Legacy rewrites are stored only when the capture has
  a complete tool bundle: Claude `Authorization`, or Codex `Authorization` plus
  `chatgpt-account-id` from exact `chatgpt.com` traffic. Incomplete or ambiguous
  legacy bundles are discarded.
- **A profile starts without my usual Codex/Claude preferences** — first-class
  profile homes start isolated so rtr does not copy global auth credentials by
  accident. Put shared skill definitions in `skills_source = "~/.skills"` if you
  want each selected profile home to receive a fresh copy on launch.
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
