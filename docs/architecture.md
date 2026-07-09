# Architecture

`rtr` is a single Rust binary (library plus thin `main`) on Tokio.

## Modules

| Module | Responsibility |
| --- | --- |
| `cli` | Clap command surface and the `rtr <tool>` alias. |
| `config` | `config.toml` model, starter config, and switch resolution. |
| `tool_specs` | Claude/Codex runtime hosts, legacy import fields, and native-home env keys. |
| `import` | Historical request JSONL parsing and legacy profile persistence. |
| `selection` | Enabled-profile validation and round-robin advancement. |
| `usage` | Usage JSONL, local-day filtering, and stats rendering. |
| `state` | Legacy active profiles and round-robin cursors. |
| `rewrite` | Header set/remove validation, host matching, and redaction. |
| `ca` | Local CA generation, loading, and authority construction. |
| `keychain` | macOS trust installation, removal, and detection. |
| `proxy` | Host-scoped hudsucker handler and server lifecycle. |
| `runner` | Native-home preparation, child launch, optional tee, proxy lifecycle, and status. |
| `paths` | Config, state, CA, profile-home, and opt-in log paths. |

## First-class subscription flow

`rtr claude` and `rtr codex` select a configured profile. A forced
`--profile/-p` is validated without mutating state; otherwise selection advances
the per-tool round-robin cursor under a lock.

The runner creates the selected native home, refreshes its skills directory,
injects `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, and launches the configured command
plus user args. First-class runs use built-in runtime hosts and an empty rewrite
set, so native tool state remains the identity source of truth. A usage event is
appended after the child finishes or launch fails.

## Legacy run flow

```text
load config + state
  -> resolve active profile into Rewrites
  -> load or mint the local CA
  -> bind the loopback proxy
  -> spawn the child with proxy and CA env
  -> wait for child
  -> stop proxy
  -> propagate child exit code
```

The child receives proxy variables pointing at the bound loopback port and CA
variables pointing at rtr's certificate. `NO_PROXY` is cleared for that child.
Normal stdio is inherited. `--log` pipes stdout/stderr through a tee and creates
the run directory before proxy startup.

## Request path

```text
child -> CONNECT target -> proxy
  host outside scope -> blind tunnel
  host inside scope  -> MITM with rtr CA
    -> apply configured header rewrites
    -> remove WebSocket compression negotiation when needed
    -> forward upstream
```

The proxy does not persist requests or headers. Plain HTTP requests and
decrypted HTTPS requests share the same host-match and rewrite path.

For legacy/custom tools, `hosts = ["*"]` or an omitted host list matches every
host reached by the spawned child. First-class commands use their fixed runtime
scope regardless of configured `hosts`.

## Historical import path

`rtr import --from-capture` parses compatible historical JSONL offline. The
record schema is private to `import`; no runtime capture sink exists. Imported
headers can populate legacy/custom rewrite profiles, while first-class commands
continue to use native homes and empty rewrites.

## On-disk layout

```text
~/.config/rtr/
  config.toml
  ca/
    rtr-ca.cert.pem
    rtr-ca.key.pem

~/.local/state/rtr/
  state.toml
  usage.jsonl
  homes/
    codex/<profile>/
      skills/
    claude/<profile>/
      skills/
```

Default launches create no per-run artifact directory. Explicit `--log` adds:

```text
~/.local/state/rtr/runs/<tool>/<timestamp-pid>/
  output.log
  rtr.log
```

## Testing

- Unit tests cover config, selection, import parsing, rewrites, CA, keychain,
  paths, native-home preparation, usage, and status.
- `tests/proxy_e2e.rs` sends a real plain-HTTP proxy request and verifies the
  upstream sees the rewritten header.
- `tests/run_smoke.rs` verifies default artifact-free launches, opt-in tee
  output, native-home injection, args, skills refresh, usage, rewrites, and exit
  propagation.
