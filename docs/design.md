# Design

## Goal

Intercept the HTTPS traffic of **one specific binary** (e.g. `codex` →
`api.openai.com`) and rewrite its auth headers, so a user can switch between
multiple subscriptions for that tool. macOS, Apple Silicon. Prefer per-binary
scoping over system-wide routing.

## Chosen approach: in-process MITM proxy, scoped to the spawned child

`rtr` launches the target binary itself. Because it owns the child's
environment, it points the child at a local [`hudsucker`](https://crates.io/crates/hudsucker)
MITM HTTPS proxy via `HTTPS_PROXY`/`HTTP_PROXY` and scopes interception to that
process alone — no routing tables, no VPN, no kernel/network extension.

The proxy intercepts only the tool's configured **target hosts**; everything else
the child talks to is blind-tunneled end-to-end (no forged certificate, nothing
broken). For intercepted requests it records the original headers to a per-run
capture file, then applies the active profile's header rewrites before
forwarding upstream.

TLS interception needs the child to trust a CA `rtr` mints locally. Two
mechanisms, because tools differ (see the trust model below).

Switching subscriptions is just flipping which profile is active; it applies on
the next run.

### Why this fits `codex` specifically

The decision was grounded by probing the real `codex` 0.139.0 binary:

- It references `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` and contains
  "tunneling HTTPS over proxy" → it honors proxy env vars (it's a `reqwest`
  client). **Routing works.**
- It links `rustls-platform-verifier` + `security-framework` + `SecTrustEvaluate`
  → it validates server certs against the **macOS trust store**, not
  `SSL_CERT_FILE`. **Trust requires the keychain** (`rtr trust`).

## Trust model

| Tool's TLS stack | How it trusts the rtr CA | sudo? |
| --- | --- | --- |
| OpenSSL / curl / git | `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `GIT_SSL_CAINFO` (set on child) | no |
| Node.js | `NODE_EXTRA_CA_CERTS` (set on child) | no |
| Python requests | `REQUESTS_CA_BUNDLE` (set on child) | no |
| macOS Security.framework / rustls-platform-verifier (**codex**) | `rtr trust` → login keychain | no |
| system trust domain only | `rtr trust --system` → System.keychain | yes |
| statically-pinned / webpki-roots-only | not interceptable without recompiling the tool | — |

`rtr run` always sets the env vars and, when the tool has target hosts, checks
whether the CA is keychain-trusted; if not, it prints the one-time `rtr trust`
command and proceeds (TLS to target hosts fails loudly until trusted — a clear
signal, not silent breakage).

The CA is per-user, minted locally, its private key stored `0600`, and removable
with `rtr untrust`.

## Key decisions

- **hudsucker for MITM**, using its re-exported `rcgen`/`rustls` so versions stay
  aligned. It provides exactly the three hooks needed: `should_intercept` (host
  scoping), `handle_request` (capture + rewrite), and `RcgenAuthority` (per-host
  leaf forging from our CA).
- **Capture is independent of rewrite.** With no profile (or an empty one),
  `rtr run` still intercepts and records — this is the "discover the real
  Authorization header" mode the workflow starts from. Capture stores the
  *original* request; rewrites are applied afterward toward the upstream.
- **Host-scoped interception**, not intercept-all — protects unrelated/pinned
  traffic and keeps the forged-cert surface minimal.
- **Secrets in a `0600` config.toml** (plaintext) — matches the requested
  ergonomics. Keychain-backed secret references are a future step.
- **Default stdio is inherited** so TUIs like `codex` render normally; request
  capture happens in the proxy regardless. `--log` opts into piping + tee'ing a
  transcript to `output.log` (may degrade a full-screen TUI).

## Rejected alternatives

- **System-wide transparent intercept (pf `rdr` / `NETransparentProxyProvider`):**
  catches binaries that ignore proxy env vars, but is system-wide (can't cleanly
  scope to one binary), needs sudo/entitlements/a signed+notarized network
  extension, and has a large blast radius. Kept only as a documented fallback for
  proxy-ignoring binaries; `codex` is not one.
- **DYLD interposition (`DYLD_INSERT_LIBRARIES`):** SIP and the hardened runtime
  block dylib injection into signed binaries like `codex`. Infeasible.
- **eBPF:** Linux-only; unavailable on macOS.

## Non-goals (v1)

System-wide interception; live re-routing of an already-running process;
response-body/WebSocket rewriting; path/method-scoped rules (host-scoped only);
Keychain-backed secret storage; Linux/Windows.
