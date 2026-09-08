# Usage

## Initialize

```bash
rtr init
```

This creates `~/.config/rtr/config.toml` with Claude and Codex tool entries.
`--force` replaces an existing file.

Create and sign in to native profiles:

```bash
rtr add claude --profile work
rtr add codex --profile personal
```

`rtr add` atomically adds the corresponding empty table, prepares the native
home and skills, and launches the tool. Adding an existing profile fails before
changing its config, home, skills, or launching a child.

The resulting config entries look like:

```toml
[tools.claude.profiles.work]

[tools.codex.profiles.personal]
```

Profiles are enabled by default. Use `rtr disable` / `rtr enable` (below) to
exclude one from selection and bring it back.

## Repair or Remove a Profile

If a profile's credentials become locked or otherwise stop working, relaunch
its normal sign-in flow inside the existing native home:

```bash
rtr fix codex --profile personal
```

`fix` validates the existing profile, removes only recognized stale credential
locks in that profile home, runs startup synchronization, and launches the
configured tool there. It preserves auth data, settings, sessions, other
profile homes, and the automatic rotation cursor. Repair also works while a
profile is disabled. If the profile is bypassed, `fix` reports that fact but still repairs
and launches its isolated native home; normal launches remain bypassed.

Delete a profile and all tool-owned state in its native home with:

```bash
rtr rm codex --profile personal
```

rtr prints the exact directory it will recursively delete and requires `y` or
`yes`. `--yes` skips the prompt for deliberate automation. The profile table is
removed without reformatting the rest of `config.toml`; remaining profiles
continue rotating even when the old cursor is larger than their new count.

## Configuration

Print the resolved config path without extra text:

```bash
rtr config
```

Open an existing config using `$VISUAL`, falling back to `$EDITOR`:

```bash
rtr config edit
```

`config edit` does not create a missing file; run `rtr init` first. The default
path is `~/.config/rtr/config.toml`.

```toml
[tools.claude]
command = ["claude"]
args = ["--effort", "max", "--model", "claude-opus-5"]
copy = [
  { source = "~/.skills", destination = "skills" },
  { source = "shared/CLAUDE.md", destination = "CLAUDE.md" },
]

[tools.claude.profiles.work]

[tools.claude.profiles.personal]
enabled = false

[tools.codex]
command = ["codex"]
args = ["-m", "gpt-5.6-sol", "-c", "model_reasoning_effort=max"]
skills_source = "shared/codex-skills"

[tools.codex.profiles.work]

[tools.codex.profiles.personal]
bypass = true
```

| Field | Meaning |
|---|---|
| `command` | Executable and immutable leading arguments |
| `args` | Native defaults for every launch; explicit runtime model/effort/config options override matching defaults |
| `copy` | Optional tool-level list of `{ source, destination }` startup mappings; `[]` disables startup copying |
| `skills_source` | Backwards-compatible skills source used only when `copy` is omitted |
| `profiles.<name>.enabled` | Whether selection may use the profile; managed by `rtr enable <tool> --profile <name>` / `rtr disable <tool> --profile <name>` |
| `profiles.<name>.bypass` | Whether runs use the tool's default home instead of the isolated native home; managed by `rtr bypass <tool> --profile <name>` / `rtr unbypass <tool> --profile <name>` |

For `copy` sources, relative paths resolve from the rtr config directory and
`~` / `~/...` resolve from the user's home. Destinations must stay inside the
selected isolated profile home: relative paths and `~/...` both resolve from
that profile home, while absolute paths and `..` are rejected. A tool cannot
set both `copy` and `skills_source`. Configuration is strict: unsupported fields
are rejected instead of ignored.

## Launch a Profile Again

```bash
rtr claude --profile work
rtr codex --profile personal
```

All tool-owned state remains isolated under rtr's state directory.

For Claude, rtr sets `CLAUDE_CONFIG_DIR` and
`CLAUDE_SECURESTORAGE_CONFIG_DIR` to the same profile home. Claude-owned user
settings, app state, sessions, plugins, and account context therefore remain
profile-specific. On macOS, Claude keeps credential secrets in Keychain and
uses the config path to qualify that service; rtr never accesses the secret.
Project `.claude/*` discovery still comes from the working tree.

Pass tool arguments after the rtr arguments:

```bash
rtr claude -p work --model claude-opus-4-6
rtr codex -p personal -m gpt-5.5 -c model_reasoning_effort=xhigh
```

When the same option is present in tool-level `args`, the explicit invocation
wins. Codex `-c` entries merge by configuration key, so overriding
`model_reasoning_effort` does not discard an unrelated configured `-c` value.
The same rightmost-wins rule removes repeated known options within one
invocation, allowing a caller to override native defaults supplied by a shell
alias. `-p` / `--profile` remains rtr-owned anywhere before `--`, even after
those alias arguments.

Use `--` to force the rest of the command line to the child:

```bash
rtr codex -p personal -- --profile child-profile
```

## Automatic Rotation

Without `--profile`, rtr chooses enabled profiles in lexicographic order and
advances a per-tool cursor:

```bash
rtr codex
rtr codex
rtr codex
```

A forced profile does not move that cursor. Profile preparation completes
before a cursor update is saved, so an invalid skills source does not consume a
turn.

## Exit Summary and Resume

After the child exits and returns the terminal, rtr prints the selected profile
and a profile-bound picker command to stderr:

```text
rtr: codex ran in profile 'personal' — resume: rtr codex -p personal resume
rtr: claude ran in profile 'work' — resume: rtr claude -p work --resume
```

Codex uses its `resume` subcommand; Claude uses its `--resume` flag. Both open
the tool's session picker inside the same native profile home that just ran. A
non-zero child exit appends ` (exit N)` and rtr still returns that exit code.

Interactive stderr uses color. Redirected or otherwise non-TTY stderr emits the
same text without ANSI escapes, as does setting `NO_COLOR` to a non-empty value.
The child keeps stdout, so pipelines such as `rtr codex exec ... | jq` remain
clean.

## Search, Resume, and Fork Conversations

`rtr sessions` opens the built-in terminal picker over Claude Code and Codex
history from every configured profile:

```bash
rtr sessions
rtr sessions --tool codex --profile personal
rtr sessions --here --query "release"
```

The interactive picker searches every user/assistant message in each native
transcript as well as its title, first prompt, working directory, tool, profile,
update time, and native session ID. Tool payloads, reasoning records, and
system/developer instructions are excluded from the dialogue index. The title
is presentation; selection uses an opaque `(tool, profile, native ID)` key.

`Enter` forks in `rtr sessions` and `rtr fork`; it resumes in `rtr resume`.
`Ctrl-F` and `Ctrl-R` choose fork and resume explicitly. With `--to-profile`,
Ctrl-R is unavailable so it cannot discard the destination choice. `Esc` / `Ctrl-C`
cancel without launching. The picker restores normal terminal mode before
handing control to the native CLI.

Rows show the title, relative age, agent/profile, and project as space allows.
The preview sits beside the list on wide terminals and below it in narrow panes. `Tab` /
`Shift-Tab` cycle **Conversation**, **Matches**, and **Details**; `Alt-1/2/3`
jump directly. Conversation reads a bounded tail with paragraph boundaries.
Matches highlights passages from the full dialogue, including old exchanges
outside that tail. Details contains full identity, paths, and requested launch
arguments. The action line uses the runner's merged model/effort arguments;
settings not explicitly passed to the native CLI are labeled as native defaults.

| Key | Action |
| --- | --- |
| Up / Down | Select a conversation |
| Ctrl-U / Ctrl-D | Scroll the preview |
| Alt-B / Alt-N | Previous / next matching passage |
| Alt-H | Toggle current-directory / all-project scope |
| Alt-T / Alt-A | Cycle agent / profile filters |
| Alt-P | Toggle preview visibility |
| Alt-R | Refresh the catalog and transcripts |
| Alt-Y | Copy the current action's exact RTR command |
| F1 | Show controls |

CLI `--tool`, `--profile`, and `--here` initialize these picker filters;
they can be changed inside the picker. An empty query puts current-directory
sessions first, then uses recency. Typed queries prioritize metadata matches
over dialogue-only matches. Search supports space-separated fuzzy terms,
`'exact`, `^prefix`, `suffix$`, and `!exclude`; case matching is smart.

Metadata becomes selectable before background transcript indexing finishes.
The indexing counter indicates that dialogue results are still incomplete.
Indexing, matching, and preview reads run outside the terminal event loop.
Recent previews and matching excerpts are cached for the open picker;
refresh invalidates those caches. Unreadable transcripts remain selectable
by metadata and are reported in Details. Copy uses `pbcopy` on macOS, or
`wl-copy` / `xclip` on other Unix systems.

The picker no longer shells out to fzf; `RTR_FZF` is no longer used.

Use the non-interactive forms for scripts and direct links:

```bash
rtr sessions --list
rtr sessions --json
rtr resume <session-id-or-exact-name>
rtr fork <session-id-or-exact-name>
rtr fork <session-id> --tool claude --profile work -- --model opus
rtr fork <session-id> --tool codex --profile work --to-profile personal
```

An exact match launches immediately. A missing or ambiguous selector opens the
picker with that text as its initial query. `--tool`, `--profile`, and `--here`
filter discovery; arguments after `--` are appended to the native resume/fork
invocation.

Forks select the next enabled destination with the same round-robin cursor as
normal launches. `--profile` filters the **source**; optional `--to-profile`
chooses an enabled **destination** without advancing that cursor. Rotation may
select the source again, especially when only one profile is enabled. The
existing Herdr menu commands inherit this behavior without a destination prompt.

RTR maps resumes and forks within the same profile to the native protocol:

| Tool | Resume | Fork |
| --- | --- | --- |
| Codex | `codex resume <id>` | `codex fork <id>` |
| Claude Code | `claude --resume <id>` | `claude --resume <id> --fork-session` |

For another destination profile, RTR copies the native conversation under a fresh
ID and runs the destination's native resume command. Codex copies legacy history
or materializes paginated parent prefixes into an independent rollout. Claude
copies the project transcript and session companions, including tool results and
subagent history. Referenced assets in supported native stores are relocated.
Missing dependencies, symlinks, malformed records, and unsupported Codex history
modes fail the transfer. Claude subagent parent context must resolve within the
copied main history; external parent context fails before publication. An
unfinished final JSONL record is omitted; an active
Codex turn is marked interrupted in the copy.

Copies preserve conversation history, including tool results and compaction
records. They do not move project files, credentials, settings, running processes,
queued work, or Claude file-rewind checkpoints. Destination startup synchronization
and configured launch defaults still apply; native arguments after `--` can
override model/effort, but cannot redirect the chosen session or remote server.

Resume always uses the source's isolated home, even when disabled or configured
for bypass, without changing rotation. Disabled profiles can supply fork history,
but destinations must be enabled. Forks always use the selected isolated home,
including normally bypassed destinations. Neither operation edits those flags.

Source-picker cancellation and failures before destination preparation consume no
rotation slot. Once the destination is prepared, a copy or launch failure consumes
that slot. A published copy is retained even if authentication or launch fails;
RTR prints its exact destination-bound resume command before launching it.

Use `rtr sessions --here` to browse all conversations for the exact current
directory. The picker offers the same search, previews, and fork/resume actions
as the all-directory view. Add `--list` or `--json` for noninteractive output:

```bash
rtr sessions --here
rtr sessions --here --list
rtr sessions --here --json
```

RTR reads Claude's top-level project JSONL transcripts (excluding nested
subagent logs), Claude native `ai-title` / `agent-name` records, active and
archived Codex rollout metadata, `history.jsonl`, and `session_index.jsonl`. It
does not depend on `usage.jsonl`. Malformed or incomplete sessions are isolated
as diagnostics so one damaged transcript cannot hide other profiles.

Catalog discovery remains bounded for Codex. Full transcript bodies are scanned
only after RTR needs the interactive picker; `--list`, `--json`, and an exact ID
or native-name open do not pay the full-text indexing cost.

Rename the active conversation with the native `/rename` command in Claude Code
or Codex. The next catalog scan reflects that native name; RTR deliberately has
no parallel rename store to drift out of sync.

## Disable and Re-enable a Profile

```bash
rtr disable codex --profile personal
rtr enable codex --profile personal
```

Disabling flips only `enabled = false` in config.toml, preserving hand-written
comments. The profile's native home, sign-in, skills, usage history, and the
rotation cursor stay untouched, so re-enabling restores it exactly as it was.
Disabled profiles are skipped by rotation and rejected by `--profile`.

Both commands are idempotent: repeating one reports the current state and
succeeds. Disabling the last enabled profile is allowed — launches fail with
"no enabled profiles" until one is re-enabled. Toggles serialize under the
same config lock as `rtr add`, so concurrent updates are never lost.

## Bypass a Profile

When an isolated profile home is corrupt or cannot authenticate, keep that lane
available while launching the real CLI with its default home:

```bash
rtr bypass codex --profile personal
rtr codex --profile personal
rtr unbypass codex --profile personal
```

Bypass persists `bypass = true` on that profile. The profile remains eligible
for forced selection and automatic rotation; `enabled` still controls whether
it may be selected. A disabled, bypassed profile takes effect after it is
enabled again.

On a bypassed run, rtr removes inherited `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, and
`CLAUDE_SECURESTORAGE_CONFIG_DIR` values owned by the selected tool. It does not
create the isolated profile home or run startup synchronization in either the
isolated or default home. The real CLI then owns its normal behavior in
`~/.codex` or `~/.claude`. Every bypassed launch prints a stderr banner with the
profile, effect, and `rtr unbypass` command; `ls`, `show`, and `status` also
mark it.

Both commands are idempotent and use the same locked, comment-preserving config
edit path as enable and disable. `rtr fix` intentionally ignores bypass because
it repairs the isolated home, and tells you when bypass remains enabled.

## Startup synchronization

Configure any number of file or directory mappings at the tool level:

```toml
[tools.codex]
command = ["codex"]
copy = [
  { source = "~/.skills", destination = "skills" },
  { source = "shared/AGENTS.md", destination = "AGENTS.md" },
]
```

Each non-bypassed isolated launch applies the same mappings to the selected
Codex or Claude profile home before starting the child. A source directory's
contents replace the destination directory; a source file or symlink replaces
the destination path. Existing destination entries not present in a source
directory are removed. Relative source paths use `RTR_CONFIG_DIR`; relative
destinations use the selected profile home. `~/...` means the real user home on
the source side and the isolated profile home on the destination side.

rtr rejects missing or unsupported sources, destinations outside the profile
home, and overlapping sources or destinations before it copies anything or
launches the child. It stages every mapping first, then atomically replaces each
destination under one per-profile lock. A staging failure leaves all existing
destinations unchanged; an install failure rolls the whole mapping set back.
Bypassed launches skip synchronization entirely.

Set `copy = []` to opt out. When `copy` is omitted, rtr retains its original
skills-only behavior for existing configs:

Each isolated launch refreshes `<native-home>/skills` from the tool's source.
Bypassed launches skip this refresh.

Defaults:

| Tool | Source | Destination |
|---|---|---|
| Claude | `~/.claude/skills` | `$CLAUDE_CONFIG_DIR/skills` |
| Codex | `~/.codex/skills` | `$CODEX_HOME/skills` |

Override the legacy skills source per tool:

```toml
[tools.codex]
command = ["codex"]
skills_source = "~/shared/codex-skills"
```

Relative paths resolve from `RTR_CONFIG_DIR`. The refresh is locked and uses a
temporary sibling directory, so concurrent launches cannot expose a partial
skills tree. `skills_source` and `copy` cannot be configured together.

Codex keeps the real `HOME`, so `$HOME/.agents/skills`, repository
`.agents/skills`, and admin roots remain natively discoverable. rtr skips a
configured source already inside the canonical user root. Otherwise it bridges
the configured source or a distinct legacy `~/.codex/skills`, excluding source
`.system` and preserving the profile's Codex-owned `.system` cache.

External relative skill symlinks are rebased so they stay usable after copying
into a profile home. Internal and dangling relative links stay verbatim.

## Inspect Profiles and Usage

```bash
rtr ls
rtr ls --all
rtr show codex --profile personal
rtr status
rtr status codex
```

`ls` shows agent, profile, enabled/disabled state, isolated/bypassed home policy,
recorded runs, and failed exits in one table. Counts cover the current local day
by default; `--all` includes all-time usage. The default view ends with a tip
about `--all`. Configured profiles without usage show zero. Usage for removed
profiles appears in a separate section, including when the config file no longer
exists. `FAILED` means a non-zero or unavailable child exit; it does not measure
tokens, subscription quota, or authentication health. An unreadable usage log
leaves profile state visible with unknown counts (`-`) and an explanation on
stderr.

`ls` and `status` mark bypassed profiles, while `show` includes the bypass flag,
its effect, and the isolated native-home environment variable and resolved path.
`status` prints every configured profile beside its resolved isolated native-home
directory, including disabled profiles, without creating missing homes.

`ls`, `paths`, and `config` share `--color=auto|always|never`. Automatic color is
enabled only when stdout is a terminal, `TERM` is not `dumb`, and `NO_COLOR` is
unset or empty. An explicit `always` or `never` overrides that automatic policy.
These options belong to the inspection commands; native agent arguments keep
their existing passthrough behavior.

```bash
rtr ls --color=always
rtr paths --color=never
rtr config --color=auto
```

Piped or redirected output is plain by default. `paths --json` is always plain,
even with `--color=always`. Config output remains only the exact path, including
its original Unix filename bytes; color adds styling without labels or shortening.

## Discover Profile Homes

Inspect the rtr-managed isolated homes for every configured profile:

```bash
rtr paths
```

The human output groups profiles by agent and names the native-home environment
variable once per group. Each row shows the profile and full isolated-home path;
disabled, bypassed, and missing homes are annotated. Path prefixes are dimmed and
the final directory name is cyan. These are the managed isolated homes, including
for profiles whose ordinary launches are bypassed. Human output is presentation
text and should not be parsed by scripts.

Use the versioned JSON contract for local integrations such as `tokens`:

```bash
rtr paths --json
```

Version 1 has this shape:

```json
{
  "version": 1,
  "profiles": [
    {
      "tool": "codex",
      "profile": "example",
      "home_env": "CODEX_HOME",
      "home": "/path/to/rtr/state/homes/codex/example",
      "enabled": true,
      "bypass": false,
      "exists": true
    }
  ]
}
```

The array includes all configured Claude and Codex profiles in deterministic
tool/profile order, including profiles that are disabled, bypassed, or whose
isolated home does not exist yet. `home` always names the isolated historical
data location. When `bypass` is true, current launches use the tool's default
home instead, while older usage may remain in the reported isolated home.

Discovery is read-only: it does not create a missing home, run startup
synchronization, or inspect credentials, commands, sessions, or their contents.
The JSON v1 field names are the compatibility contract; consumers should reject
versions they do not understand rather than fall back to parsing human output.

## Environment Overrides

```bash
RTR_CONFIG_DIR=/tmp/rtr-config \
RTR_STATE_DIR=/tmp/rtr-state \
rtr codex --profile personal
```

The defaults are:

- Config: `~/.config/rtr`
- State, native homes, and usage: `~/.local/state/rtr`

## Files

| Path | Purpose |
|---|---|
| `~/.config/rtr/config.toml` | Tool and profile configuration |
| `~/.local/state/rtr/homes/<tool>/<profile>/` | Isolated native tool home |
| `~/.local/state/rtr/state.toml` | Round-robin cursors |
| `~/.local/state/rtr/usage.jsonl` | Per-launch tool, profile, timestamp, exit code, and bypass marker when active |

## Errors

- Missing config points to `rtr init`.
- Unknown, missing, or disabled profiles fail before child launch.
- `fix` rejects unknown profiles with the matching `rtr add` command.
- `rm` rejects unknown profiles before prompting or deleting anything.
- `config edit` requires `$VISUAL` or `$EDITOR` and returns the editor's status.
- Missing configured skills sources fail without replacing existing skills.
- An unknown config or state field fails parsing.
- Child spawn failures name the configured executable and record an event with
  no exit code.
