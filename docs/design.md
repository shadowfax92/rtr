# Design

## Goal

Launch Claude Code or Codex with a selected native profile home while preserving
legacy per-binary header rewrites for custom tools. Scope networking to the
spawned child and avoid collecting request data.

## Chosen approach: native homes plus child-scoped proxying

`rtr` owns the child process, so it can set the tool's native home and proxy env
only for that process. There are no routing-table changes, VPNs, kernel
extensions, or system-wide interception.

For first-class Claude/Codex profiles, the selected account boundary is the
tool's own home directory. `rtr codex` sets `CODEX_HOME` to
`~/.local/state/rtr/homes/codex/<profile>/`; `rtr claude` sets
`CLAUDE_CONFIG_DIR` to `~/.local/state/rtr/homes/claude/<profile>/`. rtr creates
those dirs owner-only and does not mutate global `~/.codex` or shared Claude
config during first-class runs. Codex keeps the real `HOME`, so its canonical
user skills under `$HOME/.agents/skills` and repository/admin roots stay
discoverable. rtr copies only a distinct legacy or configured Codex skill root
into the selected home. Claude retains its configured/default fresh-copy path.

The proxy intercepts only target hosts. Everything outside the scope is blind
tunneled end-to-end. First-class Claude/Codex commands use built-in target hosts
and empty rewrites. Legacy/custom `rtr run` uses configured hosts and the active
profile's header rewrites.

Requests and headers are not persisted. Default launches inherit the terminal
and create no run directory. `--log` deliberately opts into a child transcript
and proxy diagnostics without enabling request recording.

### Verified Codex contract

The implementation follows current official documentation and the installed
Codex 0.144.0 source:

- [`CODEX_HOME` is the full state root](https://learn.chatgpt.com/docs/config-file/environment-variables)
  for config, auth, logs, sessions, skills, and package metadata; an explicitly
  set directory must already exist.
- [Current skill discovery](https://learn.chatgpt.com/docs/build-skills) uses
  `$HOME/.agents/skills` for personal skills, `.agents/skills` for repositories,
  `/etc/codex/skills` for admin skills, and bundled system skills from Codex.
- [The installed home resolver](https://github.com/openai/codex/blob/e0a9ff6938d85db1a7b11a693b6aa2bc31fe5a55/codex-rs/utils/home-dir/src/lib.rs#L5-L60)
  validates and canonicalizes an explicit home.
- [The installed skill loader](https://github.com/openai/codex/blob/e0a9ff6938d85db1a7b11a693b6aa2bc31fe5a55/codex-rs/core-skills/src/loader.rs#L317-L363)
  keeps `$CODEX_HOME/skills` as a legacy root while also loading the canonical
  user, system, and admin roots.
- [Bundled skills are generated](https://github.com/openai/codex/blob/e0a9ff6938d85db1a7b11a693b6aa2bc31fe5a55/codex-rs/skills/src/lib.rs#L17-L55)
  under the selected `$CODEX_HOME/skills/.system`; they are not portable user
  content.
- [File auth lives under the selected home](https://github.com/openai/codex/blob/e0a9ff6938d85db1a7b11a693b6aa2bc31fe5a55/codex-rs/login/src/auth/storage.rs#L150-L152),
  and [keyring entries are keyed by its canonical path](https://github.com/openai/codex/blob/e0a9ff6938d85db1a7b11a693b6aa2bc31fe5a55/codex-rs/login/src/auth/storage.rs#L235-L244).

Context7's `/openai/codex` index reported the same roots. A local app-server
`skills/list` probe against Codex 0.144.0 confirmed that a custom `CODEX_HOME`
simultaneously loads canonical user skills, legacy home skills, and regenerated
bundled skills.

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

- **Native homes own identity.** Login, refresh, account, config, and session
  state move together instead of being approximated with header replacement.
- **Codex skills follow native discovery.** rtr inherits
  `$HOME/.agents/skills` and repository/admin roots, bridges a distinct legacy
  `~/.codex/skills` or external `skills_source`, excludes source `.system`, and
  preserves the selected home's own bundled-skill cache. Symlink targets are
  canonicalized when copied so relocation cannot change a relative link's
  meaning. Claude retains fresh replacement from `skills_source` or
  `~/.claude/skills`. Missing explicit sources remain configuration errors.
- **No runtime request capture.** The proxy only decides interception and
  applies rewrites; it has no sink for original headers.
- **Default runs are artifact-free.** Per-run directories exist only when the
  user asks for `--log`.
- **Historical import is isolated.** `rtr import --from-capture` can still read
  older compatible JSONL for legacy profiles, but normal runs never generate
  that data.
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
