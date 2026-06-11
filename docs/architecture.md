# Architecture

`rtr` is a single Rust binary (lib + thin `main`) on the Tokio runtime.

## Modules

| Module | Responsibility |
| --- | --- |
| `cli` | clap command surface; `rtr <tool>` → `rtr run <tool>` alias (`normalize_args`). |
| `config` | `config.toml` model (`Config`/`Tool`/`Profile`), load/save, `init` scaffold, `switch` resolution. |
| `state` | `state.toml` — the live active-profile selection set by `switch`, kept separate so `config.toml` stays hand-editable. |
| `rewrite` | Pure header-rewrite engine (`Rewrites`: validated set/remove) + host matching + secret redaction. |
| `capture` | `CaptureRecord` + JSON-Lines sink (file or in-memory). |
| `ca` | Mint/load the local CA, fingerprint, build the hudsucker `RcgenAuthority`. |
| `keychain` | macOS `security` trust install/remove + detection (pure argv builders). |
| `proxy` | hudsucker `HttpHandler` (`RewriteHandler`) + `serve`. |
| `runner` | `run` (spawn child, inject env, tee, proxy lifecycle) and `status`. |
| `paths` | Config/state/CA/run-dir locations (`RTR_CONFIG_DIR`/`RTR_STATE_DIR` overrides). |

## `rtr run <tool>` flow

```
load config + state ──► resolve active profile ──► validate into Rewrites
        │
        ├──► load/mint CA ──► build RcgenAuthority
        │                       │
        │                       └─(if tool has hosts && CA not keychain-trusted)
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
                         apply active profile's set/remove
                     ─► forward upstream (real api.openai.com)
```

Plain-HTTP proxy requests skip `should_intercept` and go straight through
`handle_request`, so the same host-match + rewrite + capture logic applies.

## On-disk layout

```
~/.config/rtr/
  config.toml                 # tools, hosts, profiles, proxy port (0600)
  ca/
    rtr-ca.cert.pem           # the CA cert (install via `rtr trust`)
    rtr-ca.key.pem            # CA private key (0600)

~/.local/state/rtr/
  state.toml                  # active profile per tool (set by `rtr switch`)
  runs/<tool>/<timestamp-pid>/
    capture.jsonl             # one JSON object per intercepted request
    rtr.log                   # proxy/hudsucker logs (kept off the child's terminal)
    output.log                # child stdout+stderr transcript (only with --log)
```

## Testing

- Unit tests live beside each module; pure logic (rewrite, redaction, config,
  switch resolution, CA-ness via `x509-parser`, keychain argv, env injection,
  status rendering) is tested directly.
- `tests/proxy_e2e.rs` drives a request through the real proxy over the
  plain-HTTP path and asserts the upstream saw the rewrite while the capture kept
  the original.
- `tests/run_smoke.rs` runs the full `run_tool` against a trivial child with an
  ephemeral proxy port and asserts tee output + capture + exit-code propagation.
