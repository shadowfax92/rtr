# Architecture

`rtr` is a single Rust binary (library plus thin `main`) on Tokio.

## Modules

| Module | Responsibility |
| --- | --- |
| `cli` | Clap command surface and the `rtr <tool>` alias. |
| `config` | `config.toml` model, starter config, and switch resolution. |
| `tool_specs` | Claude/Codex runtime hosts, native-home env keys, and default skills sources. |
| `profiles` | Redacted profile rendering plus `ls` and `show` commands. |
| `selection` | Enabled-profile validation and round-robin advancement. |
| `usage` | Usage JSONL, local-day filtering, and stats rendering. |
| `state` | Legacy active profiles and round-robin cursors. |
| `rewrite` | Header set/remove validation, host matching, and redaction. |
| `ca` | Local CA generation, loading, and authority construction. |
| `keychain` | macOS trust installation, removal, and detection. |
| `proxy` | Host-scoped hudsucker handler and server lifecycle. |
| `runner` | Profile creation, native-home preparation, child launch, optional tee, proxy lifecycle, and status. |
| `paths` | Config, state, CA, profile-home, and opt-in log paths. |

## First-class subscription flow

`rtr add <tool> --profile <name>` resolves the first-class tool spec and rejects
unsupported tools or duplicate profiles before touching the native home. It
persists an empty enabled profile atomically under a cross-process lock, then
enters the normal forced-profile run path.

`rtr claude` and `rtr codex` select a configured profile. A forced
`--profile/-p` is validated without mutating state; otherwise selection advances
the per-tool round-robin cursor under a lock.

The runner creates the selected native home, refreshes its skills directory,
injects `CODEX_HOME` or Claude's `CLAUDE_CONFIG_DIR` plus
`CLAUDE_SECURESTORAGE_CONFIG_DIR`, and launches the configured command plus user
args. First-class runs use built-in runtime hosts and an empty rewrite set, so
native tool state remains the identity source of truth. A usage event is appended
after the child finishes or launch fails.

For Claude, the native home is the complete user config boundary selected by
`CLAUDE_CONFIG_DIR`, not a skills-only directory. Claude writes user settings,
app state, sessions, and plugin data there. `rtr` seeds only `skills/`; it does
not inherit user commands, agents, plugins, settings, or auth from the default
`~/.claude`. Project `.claude/*` discovery remains rooted in the working tree.
On macOS, Claude's credential secret remains in Keychain even though the config
directory selects the side-by-side account context. In verified Claude Code
2.1.205 behavior, each config directory uses a distinct path-qualified Keychain
service. rtr sets the secure-storage namespace to the selected profile path so
an inherited override cannot collapse profiles onto one Keychain entry; rtr
does not access those entries itself.

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

The first-class path replaces legacy active-profile resolution with forced or
round-robin selection, uses the spec runtime hosts and empty rewrites, prepares
the native home and skills, and pins Claude's secure-storage namespace to the
same home.

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
    codex/<profile>/          # passed as CODEX_HOME
      skills/                 # fresh copy from skills_source or ~/.codex/skills
    claude/<profile>/         # passed as CLAUDE_CONFIG_DIR
      skills/                 # fresh copy from skills_source or ~/.claude/skills
      .claude.json            # Claude-owned app/account state, created on use
      settings.json           # optional, profile-owned user settings
      projects/               # Claude-owned session history and memory
      plugins/                # profile-owned installed plugin state
```

Default launches create no per-run artifact directory. Explicit `--log` adds:

```text
~/.local/state/rtr/runs/<tool>/<timestamp-pid>/
  output.log
  rtr.log
```

## Testing

- Unit tests cover config, profile creation and rendering, selection, rewrites, CA, keychain,
  paths, native-home preparation, Claude/Codex symlink policies, usage, and status.
- `tests/proxy_e2e.rs` sends a real plain-HTTP proxy request and verifies the
  upstream sees the rewritten header.
- `tests/run_smoke.rs` verifies default artifact-free launches, opt-in tee
  output, native-home and Claude secure-storage injection, cross-profile state
  isolation, args, skills refresh, usage, rewrites, and exit propagation.
