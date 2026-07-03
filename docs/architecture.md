# Architecture

`rtr` is a single Rust binary (lib + thin `main`) on the Tokio runtime.

## Modules

| Module | Responsibility |
| --- | --- |
| `cli` | clap command surface; `rtr <tool>` → `rtr run <tool>` alias (`normalize_args`). |
| `config` | `config.toml` model (`Config`/`Tool`/`Profile`), load/save, `init` scaffold, `switch` resolution. |
| `tool_specs` | First-class Claude/Codex capture hosts, runtime hosts, captured auth fields, metadata fields, and native-home env keys. |
| `import` | Capture JSONL parsing, auth-bundle extraction, profile persistence, redacted profile/list rendering. |
| `selection` | Enabled-profile listing, forced-profile validation, round-robin cursor advancement. |
| `usage` | Usage event JSONL append/read, local-day filtering, stats aggregation and rendering. |
| `state` | `state.toml` — legacy active profiles plus round-robin cursors, separate from `config.toml`. |
| `rewrite` | Pure header-rewrite engine (`Rewrites`: validated set/remove) + host matching + secret redaction. |
| `capture` | `CaptureRecord` + JSON-Lines sink (file or in-memory). |
| `ca` | Mint/load the local CA, fingerprint, build the hudsucker `RcgenAuthority`. |
| `keychain` | macOS `security` trust install/remove + detection (pure argv builders). |
| `proxy` | hudsucker `HttpHandler` (`RewriteHandler`) + `serve`. |
| `runner` | Legacy run, subscription run, capture run, child env injection, tee, proxy lifecycle, status. |
| `paths` | Config/state/CA/run-dir/profile-home locations (`RTR_CONFIG_DIR`/`RTR_STATE_DIR` overrides). |

## Subscription command flow

`rtr capture <tool> --profile <name>` resolves the first-class tool spec,
registers an empty enabled profile if the name is missing, creates/uses
`state/homes/<tool>/<profile>/`, injects the tool's native home env (`CODEX_HOME`
or `CLAUDE_CONFIG_DIR`), refreshes `<native home>/skills`, overrides the
intercept scope to the spec's capture hosts, and launches the configured command
with an empty rewrite set. The proxy still records original headers to
`capture.jsonl`; after the child exits, rtr prints `rtr <tool> --profile <name>`.

`rtr import <tool> --profile <name> --from-capture <path>` parses captured
records offline for legacy/custom header-rewrite profiles. Claude import keeps a
legacy rewrite only when `Authorization` is captured from
`api.anthropic.com` / `mcp-proxy.anthropic.com`, and stores
`x-organization-uuid` as metadata when present. Codex import keeps legacy
rewrites only when a complete `Authorization` + `chatgpt-account-id` bundle is
captured from exact `chatgpt.com` records; incomplete or ambiguous legacy
bundles are not stored. It can still create/update a profile entry, but
first-class runtime identity comes from the native home. Telemetry from
`ab.chatgpt.com` is ignored. Imports without matching tool traffic are rejected.

`rtr claude` / `rtr codex` choose a profile for one run. `--profile/-p`
validates and forces that profile without mutating state. Without a forced
profile, selection advances the per-tool round-robin cursor in `state.toml`.
After profile validation, the runner creates the selected native profile home,
refreshes `<native home>/skills` from `skills_source` or the tool default, saves
the next cursor, uses the spec's runtime hosts for scoped capture/logging, passes
an empty rewrite set, assembles child args as configured command plus per-run
tool args, then appends one usage event after launch completes or fails.
First-class runs do not mutate global `~/.codex` or shared Claude config.

## `rtr run <tool>` flow

```
load config + state ──► resolve active profile ──► validate into Rewrites
        │
        ├──► load/mint CA ──► build RcgenAuthority
        │                       │
        │                       └─(if CA not keychain-trusted)
        │                          print one-time `rtr trust` hint
        │
        ├──► bind 127.0.0.1:<port> (TcpListener) ──► spawn proxy task
        │
        └──► spawn child with env:
                 HTTP(S)_PROXY/ALL_PROXY ─► 127.0.0.1:<port>
                 NO_PROXY = ""            (nothing excluded)
                 SSL_CERT_FILE / NODE_EXTRA_CA_CERTS / REQUESTS_CA_BUNDLE /
                 CURL_CA_BUNDLE / GIT_SSL_CAINFO ─► CA cert
             (stdio inherited by default; --log pipes + tees to output.log)
                 │
child exits ─► signal proxy graceful shutdown ─► propagate exit code
```

The first-class subscription run reuses the same proxy lifecycle after replacing
the active-profile step with forced/round-robin selection, replacing configured
hosts with the spec runtime hosts, injecting `CODEX_HOME`/`CLAUDE_CONFIG_DIR`,
refreshing `<native home>/skills`, and replacing profile rewrites with an empty
rewrite set.

## Per-request path in the proxy

```
child ─CONNECT host:443─► proxy
        should_intercept(host ∈ target hosts)?
            no  ─► blind TCP tunnel (real cert, untouched)
            yes ─► MITM: present forged leaf (signed by rtr CA)
                     │
              decrypted request ─► handle_request:
                     skip if method == CONNECT
                     host ∈ target hosts?
                         record ORIGINAL headers ─► capture.jsonl
                         apply rewrite set (empty for first-class runs)
                     ─► forward upstream (real api.openai.com)
```

Plain-HTTP proxy requests skip `should_intercept` and go straight through
`handle_request`, so the same host-match + rewrite + capture logic applies.

For legacy/custom `rtr run`, a `hosts` of `["*"]` — or an omitted `hosts` —
matches every host, so the `host ∈ target hosts` checks are always true and the
tool's full traffic is MITM'd (still scoped to the spawned child, not
system-wide). First-class Claude/Codex commands ignore configured `hosts` during
runtime and use their spec scopes instead.

## On-disk layout

```
~/.config/rtr/
  config.toml                 # tools, hosts, profiles, proxy port (0600)
  ca/
    rtr-ca.cert.pem           # the CA cert (install via `rtr trust`)
    rtr-ca.key.pem            # CA private key (0600)

~/.local/state/rtr/
  state.toml                  # active profile per tool (set by `rtr switch`)
  usage.jsonl                 # selected subscription runs and exit codes
  homes/
    codex/<profile>/          # passed as CODEX_HOME
      skills/                 # fresh copy from skills_source or ~/.codex/skills
    claude/<profile>/         # passed as CLAUDE_CONFIG_DIR
      skills/                 # fresh copy from skills_source or ~/.claude/skills
  runs/<tool>/<timestamp-pid>/
    capture.jsonl             # one JSON object per intercepted request
    rtr.log                   # proxy/hudsucker logs (kept off the child's terminal)
    output.log                # child stdout+stderr transcript (only with --log)
```

## Testing

- Unit tests live beside each module; pure logic (rewrite, redaction, config,
  switch resolution, import extraction, profile selection, stats aggregation,
  CA-ness via `x509-parser`, keychain argv, env injection, status rendering) is
  tested directly.
- `tests/proxy_e2e.rs` drives a request through the real proxy over the
  plain-HTTP path and asserts the upstream saw the rewrite while the capture kept
  the original.
- `tests/run_smoke.rs` runs both the legacy `run_tool` path and the
  first-class subscription path against trivial children with an ephemeral proxy
  port, asserting tee output, native-home env injection, runtime arg order,
  skills refresh behavior, usage recording, legacy rewrite preservation, capture
  creation, and exit-code propagation.
