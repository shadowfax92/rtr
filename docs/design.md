# Design

## Goal

Launch Claude Code or Codex with a selected native profile home while preserving
legacy per-binary header rewrites for custom tools. Scope networking to the
spawned child and avoid collecting request data.

## Chosen approach: native homes plus child-scoped proxying

`rtr` owns the child process, so it can set the tool's native home and proxy env
only for that process. There are no routing-table changes, VPNs, kernel
extensions, or system-wide interception.

For first-class profiles, the account boundary is the downstream tool's own
home. Codex receives `CODEX_HOME`; Claude Code receives `CLAUDE_CONFIG_DIR`.
Each points at `~/.local/state/rtr/homes/<tool>/<profile>/`. rtr creates those
directories owner-only and refreshes their skills from the tool default or a
configured `skills_source`.

The proxy intercepts only target hosts. Everything outside the scope is blind
tunneled end-to-end. First-class Claude/Codex commands use built-in target hosts
and empty rewrites. Legacy/custom `rtr run` uses configured hosts and the active
profile's header rewrites.

Requests and headers are not persisted. Default launches inherit the terminal
and create no run directory. `--log` deliberately opts into a child transcript
and proxy diagnostics without enabling request recording.

## Trust model

| Tool TLS stack | How it trusts the rtr CA | sudo? |
| --- | --- | --- |
| OpenSSL / curl / git | `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `GIT_SSL_CAINFO` | no |
| Node.js | `NODE_EXTRA_CA_CERTS` | no |
| Python requests | `REQUESTS_CA_BUNDLE` | no |
| macOS trust store / platform verifier | `rtr trust` in login keychain | no |
| system trust domain only | `rtr trust --system` | yes |
| pinned or bundled roots only | not interceptable without changing the tool | — |

The CA is generated locally, its private key is stored `0600`, and trust is
removable with `rtr untrust`.

## Key decisions

- **Add is onboarding for first-class profiles.** `rtr add <tool> --profile
  <name>` creates a new empty enabled profile, prepares its native home and
  skills, then launches the tool for login through the normal runtime path.
- **Duplicate adds fail before mutation.** A cross-process config lock protects
  the atomic check-and-write; the losing add never prepares a home or launches.
- **Native homes own identity.** Login, refresh, account, config, and session
  state move together instead of being approximated with header replacement.
- **Skills are replaced, not merged.** Every run starts from the configured
  source without stale destination files.
- **No runtime request capture.** The proxy only decides interception and
  applies rewrites; it has no sink for original headers.
- **Default runs are artifact-free.** Per-run directories exist only when the
  user asks for `--log`.
- **Host-scoped interception is the default.** Fixed Claude/Codex scopes and
  explicit custom scopes keep the forged-certificate surface narrow.
- **WebSocket compression is disabled on intercepted upgrades.** hudsucker
  cannot forward negotiated compressed frames, so stripping the extension keeps
  rewritten upgrades usable.

## Rejected alternatives

- **Header-only first-class switching** — native auth includes refresh state,
  account IDs, keychain state, and session identity.
- **Mutating global auth state** — races concurrent sessions and leaks beyond
  the spawned child.
- **System-wide transparent interception** — requires elevated networking or a
  signed extension and has a much larger blast radius.
- **DYLD interposition** — hardened runtime and SIP block injection into signed
  clients.
- **Keeping request recording but hiding its banner** — still persists secrets
  and fails the privacy goal.

## Non-goals

System-wide routing; arbitrary third-party onboarding; weighted selection;
session migration between native homes; response-body or WebSocket payload
rewriting; path/method rules; keychain-backed profile secrets; Linux or Windows.
