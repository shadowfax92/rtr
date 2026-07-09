# Design

## Goal

Launch Claude Code or Codex with a selected native profile home, capture its
HTTPS traffic for inspection, and preserve legacy per-binary header rewrites for
custom `rtr run` tools. macOS, Apple Silicon. Prefer per-binary scoping over
system-wide routing.

## Chosen approach: native profile homes plus child-scoped MITM

`rtr` launches the target binary itself. Because it owns the child's
environment, it points the child at a local [`hudsucker`](https://crates.io/crates/hudsucker)
MITM HTTPS proxy via `HTTPS_PROXY`/`HTTP_PROXY` and scopes interception to that
process alone — no routing tables, no VPN, no kernel/network extension.

For first-class Claude/Codex profiles, the selected account boundary is the
tool's own home directory. `rtr codex` sets `CODEX_HOME` to
`~/.local/state/rtr/homes/codex/<profile>/`; `rtr claude` sets
`CLAUDE_CONFIG_DIR` to `~/.local/state/rtr/homes/claude/<profile>/`. rtr creates
those dirs owner-only and does not mutate global `~/.codex` or shared Claude
config during first-class runs. It does refresh `<profile home>/skills` from the
tool default or a configured `skills_source` before launching, so skill
definitions follow the selected native home without merging stale destination
state.

Claude Code treats `CLAUDE_CONFIG_DIR` as its user config boundary for settings,
app state, session history, plugins, and multiple accounts. Credential files are
also stored there on Linux and Windows; macOS keeps credential secrets in the
system Keychain. Claude Code 2.1.205 path-qualifies the Keychain service by
config directory, preserving distinct login entries across rtr profiles. rtr
therefore sets both `CLAUDE_CONFIG_DIR` and
`CLAUDE_SECURESTORAGE_CONFIG_DIR` to the selected native boundary without
reading or copying credentials. Only personal `skills/` are seeded from shared
state. Commands, agents, plugins, settings, and sessions remain profile-owned,
while project `.claude/*` files continue to load from the working tree.

The proxy intercepts only the tool's **target hosts**; everything else the child
talks to is blind-tunneled end-to-end (no forged certificate, nothing broken).
First-class Claude/Codex commands use built-in target hosts from their tool spec;
legacy/custom `rtr run` tools use the configured hosts. For intercepted requests
the proxy records the original headers to a per-run capture file. First-class
runs use empty rewrites so freshly refreshed native credentials are not
overwritten by stale captured headers; legacy/custom `rtr run` applies the
selected profile's header rewrites before forwarding upstream.

TLS interception needs the child to trust a CA `rtr` mints locally. Two
mechanisms, because tools differ (see the trust model below).

First-class `rtr claude` and `rtr codex` runs select a profile for one run:
`--profile/-p` forces a profile, otherwise equal round-robin advances across
enabled profiles. The lower-level `rtr run <tool>` path still supports the older
active-profile model.

### Why this fits CLI clients

The decision was grounded by probing real CLI clients:

- They honor `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`, so child-scoped
  proxy env vars route their traffic through rtr.
- Codex uses `CODEX_HOME` for config/auth and can key auth storage by that home;
  Claude Code uses `CLAUDE_CONFIG_DIR` for profile-specific config and
  credentials.
- Some clients validate server certs against the **macOS trust store**, not only
  CA env vars. Those need one-time keychain trust (`rtr trust`).

## Trust model

| Tool's TLS stack | How it trusts the rtr CA | sudo? |
| --- | --- | --- |
| OpenSSL / curl / git | `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `GIT_SSL_CAINFO` (set on child) | no |
| Node.js | `NODE_EXTRA_CA_CERTS` (set on child) | no |
| Python requests | `REQUESTS_CA_BUNDLE` (set on child) | no |
| macOS Security.framework / rustls-platform-verifier (**codex**) | `rtr trust` → login keychain | no |
| system trust domain only | `rtr trust --system` → System.keychain | yes |
| statically-pinned / webpki-roots-only | not interceptable without recompiling the tool | — |

`rtr run` always sets the env vars and checks whether the CA is keychain-trusted;
if not, it prints the one-time `rtr trust` command and proceeds (TLS to
intercepted hosts fails loudly until trusted — a clear signal, not silent
breakage).

The CA is per-user, minted locally, its private key stored `0600`, and removable
with `rtr untrust`.

## Key decisions

- **hudsucker for MITM**, using its re-exported `rcgen`/`rustls` so versions stay
  aligned. It provides exactly the three hooks needed: `should_intercept` (host
  scoping), `handle_request` (capture + rewrite), and `RcgenAuthority` (per-host
  leaf forging from our CA).
- **Native homes are the first-class identity boundary.** Codex and Claude own
  login, refresh, account, and session state for the selected profile; rtr does
  not switch accounts by editing a shared auth file. On macOS, Claude credentials
  remain in path-qualified Keychain entries rather than physically inside the
  profile home.
- **Claude secure storage follows the selected home.** Claude 2.1.205 lets
  `CLAUDE_SECURESTORAGE_CONFIG_DIR` override the path used to qualify its
  Keychain service. rtr sets it to the same profile home as `CLAUDE_CONFIG_DIR`
  so an inherited value cannot merge otherwise isolated profiles.
- **Skills are copied fresh, not merged.** First-class runs delete and recreate
  `<profile home>/skills` from `skills_source`, defaulting to `~/.codex/skills`
  or `~/.claude/skills`. Missing explicit sources are configuration errors;
  missing defaults mean no skills to sync. Relative configured paths resolve
  from the rtr config directory. Relative symlinks to skills outside the source
  tree become absolute without resolving symlink aliases, so relocation does
  not break them and later alias retargeting still works. Codex links keep their
  original text.
- **Claude inheritance stops at skills.** User commands, agents, plugins,
  settings, auth state, and sessions are not copied from `~/.claude`; each
  profile can create its own. Project `.claude/*` behavior is unchanged.
- **Capture is independent of rewrite.** `rtr capture` and first-class
  subscription runs record original requests without applying captured auth
  rewrites. Legacy/custom `rtr run` still uses configured set/remove rewrites.
- **Capture is onboarding for first-class profiles.** `rtr capture <tool>
  --profile <name>` creates the empty enabled profile if needed, launches the
  tool against that native home, and records evidence with no rewrites.
- **Import is legacy/custom rewrite support.** `rtr import ... --from-capture
  ...` validates matching tool traffic and can store captured legacy
  rewrite/metadata fields, but first-class runtime identity comes from the
  native home.
- **Tool specs are first-class for Claude/Codex.** Specs define capture hosts,
  runtime hosts, metadata headers, and native home env keys. Claude keeps
  `Authorization` as a legacy rewrite and `x-organization-uuid` as metadata when
  present. Codex keeps a complete `Authorization` plus `chatgpt-account-id`
  legacy bundle when present, while avoiding global cookie or telemetry rewrites.
- **Host-scoped interception by default** — named hosts protect unrelated/pinned
  traffic and keep the forged-cert surface minimal. First-class Claude/Codex
  runs keep fixed runtime scopes; legacy/custom tools opt into intercept-all with
  `hosts = ["*"]` (or by omitting `hosts`). That still scopes to the spawned
  child via proxy env vars — it is not system-wide interception.
- **Legacy secrets may exist in a `0600` config.toml** — imported headers remain
  available for compatibility with `rtr run`, but first-class runtime identity
  comes from native homes.
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
- **Mutating global Codex/Claude auth state on switch:** races other sessions,
  fights in-memory auth snapshots, and leaks the effect beyond the spawned child.
- **Header-only first-class account switching:** too brittle for Codex/Claude
  because native auth includes refresh state, account IDs, keychain-backed
  credentials, and agent/session identity.
- **eBPF:** Linux-only; unavailable on macOS.

## Verified Claude Code contract

- [Environment variables](https://code.claude.com/docs/en/env-vars) documents
  `CLAUDE_CONFIG_DIR` as the override for user settings, session history,
  plugins, and non-macOS credential files, intended for side-by-side accounts.
- [The `.claude` directory](https://code.claude.com/docs/en/claude-directory)
  separates global user state from project `.claude/*` state and lists the
  application data held under the user config boundary.
- [Skills](https://code.claude.com/docs/en/slash-commands) documents personal
  skills at `~/.claude/skills/<name>/SKILL.md` and supports symlinked skill
  directories in Claude Code 2.1.203 and later.
- [Authentication](https://code.claude.com/docs/en/team) documents macOS
  Keychain storage and config-directory credential files on Linux and Windows.
- Claude Code 2.1.205 was also checked locally: a temporary
  `CLAUDE_CONFIG_DIR` received `.claude.json`, backups, and a generated
  `skills/<name>` plugin, and a second temporary directory had independent app
  state. The default config reported its existing login while both temporary
  config dirs reported no login, and the macOS Keychain showed distinct
  `Claude Code-credentials-<suffix>` services. Inspection of the installed
  executable confirmed the suffix is derived from the normalized secure-storage
  config path and that `CLAUDE_SECURESTORAGE_CONFIG_DIR` takes precedence.

## Non-goals (v1)

Arbitrary third-party subscription onboarding; weighted profile selection;
session-resume migration between Claude config dirs; global auth/config seeding
into new profile homes; system-wide interception; live re-routing of an
already-running process; response-body/WebSocket rewriting; path/method-scoped
rules (host-scoped only); Keychain-backed secret storage; Linux/Windows.
