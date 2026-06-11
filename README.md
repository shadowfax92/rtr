# rtr — per-binary MITM header rewriter

`rtr` launches a specific binary (e.g. `codex`) with its HTTPS traffic routed
through a local man-in-the-middle proxy, so you can **capture and rewrite the
outbound auth headers** for that one process — and flip between multiple
subscriptions/accounts by switching a profile.

It's "mitmproxy, but scoped to one binary and aimed at swapping `Authorization`."

```sh
rtr init                      # scaffold ~/.config/rtr/config.toml + mint a local CA
rtr trust                     # trust the CA in your login keychain (one time, no sudo)
rtr codex                     # run codex through the proxy; captures land in a per-run file
# …inspect the captured Authorization header, paste tokens into config.toml…
rtr switch codex codex-2      # make subscription #2 active
rtr codex                     # now codex talks to OpenAI with codex-2's token
```

## Why this works (and its one caveat)

Because `rtr` **spawns** the child, it scopes interception to just that process
via `HTTPS_PROXY` — no system-wide routing, no VPN, no kernel extension. TLS is
intercepted with a CA `rtr` mints locally.

- Tools that read CA env vars (OpenSSL/Node/Python/curl/git) trust the CA with
  **no keychain change** — `rtr` sets `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`,
  `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`, `GIT_SSL_CAINFO` on the child.
- Tools that verify against the macOS trust store (`codex` uses
  `rustls-platform-verifier`) need a one-time `rtr trust` (login keychain, no
  sudo). `rtr run` tells you when this is needed.

## Docs

- [docs/usage.md](docs/usage.md) — install, the codex walkthrough, config and
  command reference, troubleshooting.
- [docs/design.md](docs/design.md) — the chosen approach and rejected
  alternatives (eBPF, pf/NetworkExtension, DYLD injection), and the trust model.
- [docs/architecture.md](docs/architecture.md) — modules, request flow, on-disk
  layout.

## Status

macOS / Apple Silicon. Built in Rust on [`hudsucker`](https://crates.io/crates/hudsucker).
Per-binary scoping via proxy env vars; v1 does not do system-wide interception.
