# Usage

## Initialize

```bash
rtr init
```

This creates `~/.config/rtr/config.toml` with Claude and Codex tool entries.
`--force` replaces an existing file.

Create and sign in to native profiles:

```bash
rtr add claude work
rtr add codex personal
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
rtr fix codex personal
```

`fix` validates the existing profile, removes only recognized stale credential
locks in that profile home, runs startup synchronization, and launches the
configured tool there. It preserves auth data, settings, sessions, other
profile homes, and the automatic rotation cursor. Repair also works while a
profile is disabled. If the profile is bypassed, `fix` reports that fact but still repairs
and launches its isolated native home; normal launches remain bypassed.

Delete a profile and all tool-owned state in its native home with:

```bash
rtr rm codex personal
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
| `profiles.<name>.enabled` | Whether selection may use the profile; managed by `rtr enable <tool> <name>` / `rtr disable <tool> <name>` |
| `profiles.<name>.bypass` | Whether runs use the tool's default home instead of the isolated native home; managed by `rtr bypass <tool> <name>` / `rtr unbypass <tool> <name>` |

For `copy` sources, relative paths resolve from the rtr config directory and
`~` / `~/...` resolve from the user's home. Destinations must stay inside the
selected isolated profile home: relative paths and `~/...` both resolve from
that profile home, while absolute paths and `..` are rejected. A tool cannot
set both `copy` and `skills_source`. Configuration is strict: unsupported fields
are rejected instead of ignored.

## Launch a Profile Again

```bash
rtr claude -p work
rtr codex -p personal
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

Without `-p`, rtr shares launches equally among enabled profiles by
default, choosing them in lexicographic order and advancing a per-tool cursor:

```bash
rtr codex
rtr codex
rtr codex
```

A forced profile does not move that cursor. Profile preparation completes
before a cursor update is saved, so an invalid skills source does not consume a
turn.

To give a low-quota profile a smaller share, set its percentage:

```bash
rtr weight
rtr weight codex
rtr weight codex -p nik 25
rtr weight codex --reset
```

`weight` without a tool shows every tool; with a tool it shows just that tool.
Each edit prints the resulting allocation. Enter a whole percentage from 0 to
100; an optional `%` suffix is accepted. Profiles without an override split the
remainder equally, and percentages already set stay fixed:

| Action | nik | eng | work |
|---|---:|---:|---:|
| Default | 33.33% | 33.33% | 33.33% |
| Set nik to 25% | 25% | 37.5% | 37.5% |
| Set eng to 50% | 25% | 50% | 25% |

The table identifies settings as `fixed` or `remainder`. Computed shares are
rounded to two decimal places for display; scheduling uses exact ratios.
Automatic launches and fork destinations share a persistent weighted schedule,
so separate and concurrent RTR processes honor the same distribution. These
percentages do not cap token usage, session duration, or subscription quota.
Explicit `--profile`, explicit fork destinations, and resumes do not consume an
automatic slot. Existing sessions keep running in their original profiles.

An override is stored as `share_percent = 25` in the profile's TOML table.
`--reset` removes every override for the tool, including disabled profiles, and
restores equal shares. There is no `auto` argument. A 0% profile gets no automatic
launches but remains usable with explicit `--profile`; disabling it also blocks
explicit launches. Disabled profiles keep their override but take no share.

Enabled fixed shares may not exceed 100%. When all enabled profiles have fixed
shares, they must total exactly 100%. For example, a lone enabled profile fixed
at 25% cannot absorb the missing 75%. Automatic selection reports an error;
reset the tool, adjust the percentages, or enable a profile without an override.
Disabling or removing profiles is still allowed if it leaves an incomplete
allocation. `weight` and `ls` display the issue and explicit launches still work.
Re-enabling validates the resulting allocation before changing configuration.

## Startup, Exit Summary, and Resume

Before starting the child, rtr prints one line identifying the tool, selected
profile, and requested model on stderr. Effort appears when explicitly supplied:

```text
rtr: starting claude in profile 'work' · model claude-opus-5-5 · effort xhigh
rtr: starting codex in profile 'personal' · model gpt-5.5 · effort xhigh
```

The line uses merged tool `args` and command-line overrides, including Codex
`-c model=...` and `-c model_reasoning_effort=...`. It does not inspect layered
native settings or restored session settings; without a model argument it says
`model native default`. Normal launches, resumes, and forks use this summary.

After the child exits and returns the terminal, rtr prints the selected profile
and a profile-bound picker command to stderr:

```text
rtr: codex ran in profile 'personal' — resume: rtr codex -p personal resume
rtr: claude ran in profile 'work' — resume: rtr claude -p work -r
```

Codex uses its `resume` subcommand; Claude uses its `--resume` flag. Both open
the tool's session picker inside the same native profile home that just ran. A
non-zero child exit appends ` (exit N)` and rtr still returns that exit code.

Interactive stderr uses color. Redirected or otherwise non-TTY stderr emits the
same text without ANSI escapes, as does setting `NO_COLOR` to a non-empty value.
The child keeps stdout, so pipelines such as `rtr codex exec ... | jq` remain
clean.

## Search, Resume, and Fork Conversations

Codex exec runs and subagent threads are hidden from lists and the picker by
default. Add `--include-non-conversations` to `sessions`, `fork`, or `resume` to
show them; exact IDs resolve regardless of this flag. Multi-file Codex threads
appear once, with previews and search following their live history.

`rtr sessions` opens the built-in terminal picker over Claude Code and Codex
history from every configured profile:

```bash
rtr sessions
rtr sessions -t codex -p personal
rtr sessions --here --query "release"
```

The interactive picker searches every user/assistant message in each native
transcript plus its title, project directory, native session ID, and first prompt. Tool payloads, reasoning records, and
system/developer instructions are excluded from the dialogue index. The title
is presentation; selection uses an opaque `(tool, profile, native ID)` key.

`Enter` forks in `rtr sessions` and `rtr fork`; it resumes in `rtr resume`.
`Ctrl-F` and `Ctrl-R` choose fork and resume explicitly. With `--to-profile`,
Ctrl-R is unavailable so it cannot discard the destination choice. `Esc` / `Ctrl-C`
cancel without launching. The picker restores normal terminal mode before
handing control to the native CLI.

Rows show the title, relative age, agent/profile, and project as space allows.
The preview sits beside the list on wide terminals and below it in narrow panes. `Tab` /
`Shift-Tab` switch between **Conversation** and **Details**.
Conversation initially reads a bounded tail, then uses the persistent dialogue
index. Without a query, the latest message sits at the bottom. With a query,
it lands on the newest message containing any positive term, with older context
above and all matching terms highlighted. A dim `Match 1/7 · newest first` line
shows your position. Ctrl-N moves to an older match; Ctrl-B returns toward newer
ones. In a long message, the last hit stays in view. Terms still AND across the
whole conversation, so separate messages can satisfy separate terms.

Ctrl-U scrolls up; Ctrl-D continues down through later messages, including those
after the match. Changing selection lands on that conversation's newest match;
clearing the query returns to its latest message. Metadata-only hits show the
latest messages and name the matching title, project, ID, or first-prompt field.
Claude messages typed while the agent is working are included as human dialogue.
Slash-command prompts display and search as `/name args`; local command output
and machine-generated attachments are excluded.
The previous preview stays visible until the replacement arrives. Details
contains full identity, paths, and requested launch
arguments. The action line uses the runner's merged model/effort arguments;
settings not explicitly passed to the native CLI are labeled as native defaults.

| Key | Action |
| --- | --- |
| Up / Down | Select a conversation |
| Ctrl-U / Ctrl-D | Scroll the preview |
| Ctrl-B / Ctrl-N | Newer / older matching message |
| Ctrl-O | Toggle current-directory / all-project scope |
| Ctrl-T / Ctrl-G | Cycle agent / profile filters |
| Ctrl-P | Toggle preview visibility |
| Ctrl-L | Refresh the catalog and transcripts |
| Ctrl-Y | Copy the current action's exact RTR command |
| F1 | Show controls |

These controls work without Alt, which terminal multiplexers often reserve.
The original `Alt-B/N`, `Alt-H`, `Alt-T/A`, `Alt-P`, `Alt-R`, and `Alt-Y`
remain aliases; `Alt-1` selects Conversation, `Alt-2` selects Details, and
`Alt-3` remains an alias for Details. `Ctrl-A` and
`Ctrl-E` retain query start/end editing, and `Ctrl-R` still resumes.

CLI `--tool`, `--profile`, and `--here` initialize these picker filters;
they can be changed inside the picker. An empty query puts current-directory
sessions first, then uses recency. Typed terms AND together, including terms
found in different messages or fields. Dialogue uses case-insensitive substrings,
so `cache timeout` requires both words rather than their scattered letters.
All fields use literal matching, including titles, project paths, IDs, and first
prompts. A hyphen inside a term, such as `session-owner`, is ordinary text.
Complete title hits rank first, other metadata hits next, then dialogue-only
hits; each group uses recency. A leading apostrophe (`'exact`) is accepted as
a no-op for compatibility. `^prefix` and `suffix$`
anchor a short field or an individual message. `!exclude` excludes literal,
case-insensitive occurrences in any searchable field or dialogue message.

Warm opens show the cached list with complete cached dialogue already searchable.
The filesystem is reconciled in the background after the first frame. On a cold
cache, metadata becomes selectable before background indexing finishes; the
indexing counter indicates that dialogue results are still incomplete.
Indexing, matching, and preview reads run outside the terminal event loop.
Selection and preview navigation do not re-run search. The terminal redraws only
for input, results, and resize, and formats only visible rows. Refresh invalidates
preview excerpts and checks the persistent index for changed files. Unreadable transcripts remain selectable
by metadata and are reported in Details. Copy uses `pbcopy` on macOS, or
`wl-copy` / `xclip` on other Unix systems.

The persistent cache lives at `$RTR_STATE_DIR/session-index/`, outside native
homes. Its catalog, incremental per-transcript JSON checkpoints, and mapped
picker snapshot contain metadata and human dialogue, with `0700` directories
and `0600` files. Warm opens use the mapped snapshot in place rather than
deserializing the corpus. Unchanged path/size/mtime/inode
keys reuse projections; growing files resume after their last complete record.
Replaced or truncated files rebuild. Completed shards and large-file checkpoints
survive an interrupted first build. Concurrent pickers publish through advisory
locks and atomic private writes. Missing, unreadable, corrupt, or incompatible
cache entries rebuild automatically. Version 5 removes old shards and rebuilds
the cache once to prepare the mapped snapshot. The first physical Codex header
still owns identity and visibility; replayed ancestor headers do not replace it. You can delete this disposable directory
to rebuild it; it contains conversation text. `--list` and `--json` use the same
catalog and wait for fresh metadata without loading unchanged dialogue. Exact
ID opens retain their direct filename lookup. Native homes are never written by
indexing.

The picker no longer shells out to fzf; `RTR_FZF` is no longer used.

Use the non-interactive forms for scripts and direct links:

```bash
rtr sessions -l
rtr sessions -j
rtr resume <session-id-or-exact-name>
rtr fork <session-id-or-exact-name>
rtr fork <session-id> -t claude -p work -- --model opus
rtr fork <session-id> -t codex -p work -d personal
```

An exact ID is resolved by filename without building the catalog. Exact native
names still use the catalog. An ambiguous match opens the picker. A missing UUID
prints the ID and searched homes, then opens an empty current-directory picker;
without a terminal it exits nonzero. If the ID exists in `~/.claude` or `~/.codex`,
the message explains that it was started outside rtr. Importing those sessions is
not supported. `--tool`, `--profile`, and `--here`
filter discovery; arguments after `--` are appended to the native resume/fork
invocation.

Forks select the next enabled destination other than the source, honoring the
relative shares among eligible destinations. `--profile` filters the **source**; optional `--to-profile`
chooses an enabled **destination** without advancing that cursor. With only the
source usable, the fork stays there and prints an explanation. If all other
enabled profiles have zero share, forks rotate equally among those alternatives.
The existing Herdr menu commands inherit this behavior without a destination prompt.

RTR opens an existing session with its tool's native resume command.

| Tool | Resume |
| --- | --- |
| Codex | `codex resume <id>` |
| Claude Code | `claude --resume <id>` |

For every fork, RTR copies the native conversation under a fresh ID and resumes
it in the selected profile, including when the source profile is selected.
Codex copies legacy history or materializes paginated inherited prefixes and
the active segment into one independent rollout. A `history_base` references a
physical rollout ID, which may belong to the same thread or another thread.
Segments without a base replace all earlier history. Written rollout paths use
local time, matching Codex. The Claude adapter copies the project transcript and
session companions, including tool results and subagent history. RTR relocates
asset references in supported native stores.
Missing dependencies, symlinks, malformed records, and unsupported Codex history
modes fail the transfer. Claude subagent parent context must resolve within the
copied main history; external parent context fails before publication. An
unterminated final JSONL record is treated as incomplete and omitted.
An active Codex turn is marked interrupted in the copy.

Claude titles follow native precedence: `agent-name`, `custom-title` (written by
`/rename`), then `ai-title`. Copies keep titles but drop `session-owner` account
and organization attribution so the destination tool records its own account.

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
rtr sessions --here -l
rtr sessions --here -j
```

RTR reads Claude's top-level project JSONL transcripts (excluding nested
subagent logs), Claude native `agent-name` / `custom-title` / `ai-title` records, active and
archived Codex rollout metadata, `history.jsonl`, and `session_index.jsonl`. It
does not depend on `usage.jsonl`. Malformed or incomplete sessions are isolated
as diagnostics so one damaged transcript cannot hide other profiles.

Catalogs share the persistent index. Unchanged metadata is reused without loading
dialogue for `--list`, `--json`, and native-name opens; missing or changed files
refresh the shared projection. Exact IDs keep their direct filename lookup.

Rename the active conversation with the native `/rename` command in Claude Code
or Codex. The next catalog scan reflects that native name; RTR deliberately has
no parallel rename store to drift out of sync.

## Disable and Re-enable a Profile

```bash
rtr disable codex personal
rtr enable codex personal
```

Disabling flips only `enabled = false` in config.toml, preserving hand-written
comments. The profile's native home, sign-in, skills, usage history, and the
stored share stay untouched. The next automatic selection reconciles any change
to the allocation; re-enabling validates the profile's retained share.
Disabled profiles are skipped by rotation and rejected by `--profile`.

Both commands are idempotent: repeating one reports the current state and
succeeds. Disabling the last enabled profile is allowed — launches fail with
"no enabled profiles" until one is re-enabled. Toggles serialize under the
same config lock as `rtr add`, so concurrent updates are never lost.

## Bypass a Profile

When an isolated profile home is corrupt or cannot authenticate, keep that lane
available while launching the real CLI with its default home:

```bash
rtr bypass codex personal
rtr codex -p personal
rtr unbypass codex personal
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
rtr ls -a
rtr show codex personal
rtr status
rtr status codex
```

`ls` shows agent, profile, enabled/disabled state, isolated/bypassed home policy,
automatic `SHARE`, recorded runs, and failed exits. Wide terminal tables show
successful and failed exits together as `RUNS` (`ok/failed`) and show `HOME`
when a profile is bypassed. Shares describe current policy. Run counts include
explicit launches and resumes and cover the current local day by default. `--all` includes all-time usage. The default view ends with a tip
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

`ls`, `weight`, `paths`, and `config` share `--color=auto|always|never`. Automatic color is
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
rtr paths -j
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
rtr codex -p personal
```

The defaults are:

- Config: `~/.config/rtr`
- State, native homes, and usage: `~/.local/state/rtr`

## Files

| Path | Purpose |
|---|---|
| `~/.config/rtr/config.toml` | Tool and profile configuration |
| `~/.local/state/rtr/homes/<tool>/<profile>/` | Isolated native tool home |
| `~/.local/state/rtr/state.toml` | Equal-rotation cursors and weighted scheduling progress |
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


## Usage limits and watchdogs

`rtr session ID [-t claude|codex] [-p NAME] -j` never opens a
picker. It exits 0 with the following v1 object; unknown, ambiguous, and
outside-RTR IDs exit 1 with one stderr line. Nullable fields remain present.

```json
{"schema_version":1,"tool":"claude","profile":"work","id":"example-id",
 "cwd":"/project","model":"claude-opus-5-5","effort":"high",
 "limit":{"kind":"weekly","at":"2026-11-11T17:00:00Z",
          "resets_at":"2026-11-14T20:00:00Z","active":true,"message":"display text"}}
```

`limit` is null unless the newest turn ended on a usage limit written in this
profile. `model` and `effort` are null if unavailable. `resets_at` can be null;
unknown resets keep the session signal active, while profile avoidance expires
after 15 minutes from the error timestamp. Known resets become inactive when
they pass. `kind` and `message` are human labels,
not branching keys. Claude uses `apiError=usage_limit_reached` regardless of its
native window name. Codex uses a terminal `usage_limit_exceeded`, a saturated
`codex` rate-limit window from that same turn, or the error's local-time date /
time as a fallback. Older turns' windows never supply a reset.

`rtr fork ID --no-picker -n -j` exits 0 with:

```json
{"schema_version":1,"destination":{"tool":"claude","profile":"personal"},
 "in_place":false,"model":"claude-opus-5-5","effort":"high"}
```

When every routable profile is blocked for the configured model policy, it exits 75.

```json
{"schema_version":1,"destination":null,
 "earliest_reset":"2026-11-14T20:00:00Z",
 "limited":[{"profile":"work","until":"2026-11-14T20:00:00Z"}]}
```

An empty pool returns 75 with a null earliest reset. For each profile, rtr takes
the latest release among simultaneous blockers and then the earliest time a
permitted model can use that profile. The report uses the earliest such time
across the pool. Zero-share profiles are outside the automatic launch pool.
Dry runs leave quota, rotation, transfer, routing log, and usage state intact.

Automatic forks preserve relative shares among eligible alternatives. If only
the source admits the requested model, the fork stays there. If every candidate
blocks Fable, automatic Claude launches and forks try Opus by default. Explicit
destinations and exact-profile resumes keep the selected home and receive
advisory quota warnings. Resume warnings leave rotation and holds intact.

```toml
[routing.claude]
auto_drop = true
fallback_model = "claude-opus-5-5"
```

Set `auto_drop = false` to require the requested Fable model on automatic routes.
Set `fallback_model` to a recognized Opus model ID or alias.

Run the real move without pinning the dry run's destination:

```sh
rtr fork ID -t claude --no-picker --report /tmp/move.json -- --effort high
```

`--report PATH` first removes any previous report at PATH. After preparation
and copying, before child startup, it atomically publishes a private JSON file:

```json
{"schema_version":1,"source":{"tool":"claude","profile":"work","id":"old-id"},
 "destination":{"tool":"claude","profile":"personal"},"in_place":false,
 "id":"fresh-id","model":"claude-opus-5-5","effort":"high"}
```

Every prepared fork reports its new native `id`. In-place forks set
`in_place` to `true` and report the source profile as the destination.
Exit 75 writes the same failure object as a dry run. Other preparation failures
leave no report. The report survives a child spawn or execution failure.
`--no-picker` fails wherever interactive selection would otherwise be needed.

Fork/resume model and effort precedence is explicit native argv after `--`,
then the transcript's last real settings, then `[tools.TOOL] args`. Synthetic
Claude errors cannot replace model settings. A readable nonempty model cache
in the selected Codex home can reject an inherited model and fall back to the
configured model. That check also applies to exact resume. Explicit overrides
always win. Service tier is not restored.

Session lookup and real launch or fork preflight learn holds from owned transcript
tails. Routing evaluates the four most recently written top-level histories per
candidate along with its native usage snapshot. An applicable full window or
hold blocks that model. Missing, expired, or stale partial readings leave
headroom unknown, which remains eligible. A newer owned success can clear
applicable holds. Completed Codex turns leave reported full meters in force
until a new sample, reset, or manual clear. Use `rtr clear-limit TOOL NAME` to suppress older blockers
for the named profile.

Use `rtr log -j -t claude` to inspect admissions, blocked pools, model drops,
and prepared fork IDs. `rtr log -p NAME` filters on source or destination.
Human-readable reset times use local time. JSON timestamps use UTC.

`quota.json` and `fork-origins.json` under `$RTR_STATE_DIR` use independent
advisory locks and atomic 0600 writes. Legacy `state.toml` is unchanged. New
transfers record the exact inherited byte count before publishing the copied
transcript. Older copies use filesystem birth time: errors predating the file
are inherited. This fallback depends on birth-time support and preserved file
creation history; without it, RTR conservatively avoids attributing undated
legacy records. Session inspection uses 64 KiB reverse blocks, reading older blocks only
until the newest turn and its latest settings are resolved, plus a bounded
head for cwd. Native homes are read-only during inspection.

### Short command forms

Profile management accepts `rtr enable codex personal` (likewise `add`, `rm`,
`fix`, `disable`, `bypass`, `unbypass`, `show`, and `clear-limit`). `-p personal`
and `--profile personal` remain equivalent alternatives. Conflicting positional
and flagged names fail before any action.

| Short | Long | Commands |
| --- | --- | --- |
| `-p` | `--profile` | Every command with a profile option |
| `-t` | `--tool` | `sessions`, `session`, `fork`, `resume` |
| `-j` | `--json` | `sessions`, `session`, `paths`, `fork` |
| `-l` | `--list` | `sessions` |
| `-a` | `--all` | `ls` |
| `-y` | `--yes` | `rm` |
| `-f` | `--force` | `init` |
| `-n` | `--dry-run` | `fork` (requires `-j`) |
| `-d` | `--to-profile`, `--to` | `fork` |

`--here`, `--no-picker`, `--report`, `--include-non-conversations`, `--reset`,
and `--color` keep their long forms. Existing long options keep working.
Native launch parsing is unchanged: rtr owns `-p` / `--profile` before `--`;
put native `-p` after `--`. On `fork` and `resume`, all native arguments must
follow `--`, including letters shared with rtr.

### Subscription usage in `ls`

`rtr ls` shows independent meters for Claude's 5-hour, all-model weekly, and
Fable weekly windows and the reported Codex windows. Bars show percentage
used. Expired readings keep their measured value with a `was` marker.
The listing evaluates usage samples, holds, and recent owned transcript
evidence with the same rules as routing. A full Claude reading superseded
by a newer owned success shows `superseded` and the success time. Active
holds show as `HELD` and add `!` to the affected meter when its scope is known.

At 100 columns or more, the terminal compares profiles in a table when the
columns fit. Resets use local weekday and time. Extra Codex scopes appear
beneath the profile's row. Narrower layouts preserve the evidence in profile
blocks. A pipe or redirect without `COLUMNS` keeps each profile on one line.
A bypassed ordinary launch uses the native default home with unknown headroom.
