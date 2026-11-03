# dreamd

A local dream machine for Claude Code memory. `dreamd` consolidates a
project's auto-memory directory (`<config_dir>/projects/<slug>/memory/`) out
of band: it reads the session transcripts that touched the project since its
last dream, runs one `claude -p` **worker** per session to mine evidence-backed
insights, then runs one **aggregator** that rewrites a *clone* of the memory
dir. You review the diff and apply it (atomically, with a backup) or discard
it. The live memory dir is never written except by `dreamd apply`.

One binary is both the CLI and a loopback HTTP API, plus an optional nightly
launchd job.

## Why it looks like this

It mirrors the two things Anthropic ships:

- **Claude Code auto-dream** (background memory consolidation, gated behind a
  server-side flag that is off on our accounts). Same gate (at least 24 h and
  3 sessions since the last dream), same aggregator phases: read the index and
  every memory file, fold in new insights, merge duplicates and prune stale or
  contradicted files, reconcile against CLAUDE.md, rewrite `MEMORY.md`, and
  summarize.
- **The hosted Dreams API** (`POST /v1/dreams` for Managed Agents memory
  stores). Same object shape: a dream takes one memory store plus 1..100
  sessions, a `model` and optional `instructions`, produces a *new* store, and
  moves through `pending → running → completed | failed | canceled`
  (dreamd adds `applied` and `rolled_back`).

Everything goes through `claude -p`, so it rides your existing Claude Code
login. No API key.

For diagrams of the pipeline, the review-loop state machine, the write
boundaries and the nightly gate, and for where each idea came from, see
[docs/inspiration-and-design.md](docs/inspiration-and-design.md).

## Install

```sh
cargo install --path . --locked     # → ~/.cargo/bin/dreamd
dreamd --version                    # dreamd 0.1.0
```

Then write `~/.config/dreamd/config.toml` with at least `claude_bin` (next
section) and run `dreamd doctor`.

## Configuration

`~/.config/dreamd/config.toml` (or the path in `DREAMD_CONFIG`). Every key is
optional; a missing file means all defaults. `dreamd config show` prints the
effective config.

```toml
config_dirs   = ["~/.claude", "~/.claude-revenium", "~/.claude-clever"]
claude_bin    = "claude"            # SET THIS: absolute path to the real Claude Code binary
min_hours     = 24                  # gate: hours since the mined-through cutoff
min_sessions  = 3                   # gate: candidate sessions since that cutoff
exclude_slugs = []                  # project slugs never dreamed
port          = 7720                # dreamd serve
data_dir      = "~/.local/share/dreamd"

[worker]                            # one per session transcript
model                = "sonnet"
effort               = "low"
concurrency          = 3
timeout_secs         = 600
max_transcript_bytes = 200000       # reduced digest cap (head 40% + tail 40% beyond it)

[aggregator]                        # one per dream
model        = "opus"
effort       = "high"
timeout_secs = 1800

[schedule]                          # dreamd schedule install
hour       = 3
minute     = 30
auto_apply = false

[limits]                            # quota caps (see "Quota and caps")
max_sessions_per_dream = 10         # newest candidate sessions one dream mines
max_sessions_per_run   = 30         # worker calls one `dream --all` may spend
max_projects_per_run   = 5          # projects one `dream --all` may dream
backfill_days          = 14         # never-dreamed project: only transcripts this recent (0 = all)
min_transcript_bytes   = 8192       # smaller transcripts are never candidates
```

`config_dirs` and `data_dir` expand a leading `~`; **`claude_bin` does not**,
so give it an absolute path (or a bare name resolved on `PATH`).

### `claude_bin` must be the real binary

The account-rotating wrapper at `~/.local/bin/claude` always injects
`--dangerously-skip-permissions`, which turns every run into
`bypassPermissions` and voids the containment below. dreamd detects such a
wrapper (a text script containing that flag) and refuses to start:

```
dreamd: claude_bin /Users/you/.local/bin/claude is a wrapper that injects --dangerously-skip-permissions; set claude_bin in ~/.config/dreamd/config.toml (or DREAMD_CLAUDE_BIN) to the real binary
```

Point it at the real binary instead. On Mike's machine:

```toml
claude_bin = "/Users/webmech/.local/bin/_claude"
```

(`DREAMD_ALLOW_BYPASS_WRAPPER=1` disables the check. Don't.)

### Environment overrides

| Variable | Effect |
| --- | --- |
| `DREAMD_CONFIG` | config file path |
| `DREAMD_DATA_DIR` | overrides `data_dir` |
| `DREAMD_CONFIG_DIRS` | colon-separated, overrides `config_dirs` |
| `DREAMD_CLAUDE_BIN` | overrides `claude_bin` |
| `DREAMD_NO_NOTIFY=1` | no macOS notification when a dream completes |
| `DREAMD_LOG` | tracing filter for stderr (default `warn`, e.g. `info`) |

## Doctor

```sh
dreamd doctor          # exit 0 when every line is ok, else 1
dreamd doctor --json
```

```
ok   claude binary      /Users/webmech/.local/bin/_claude
ok   claude permissions permission modes are honoured (no bypass wrapper)
ok   config dirs        3 of 3 present: /Users/webmech/.claude, ...
ok   data dir           /Users/webmech/.local/share/dreamd (writable)
ok   database           /Users/webmech/.local/share/dreamd/dreamd.db
ok   api token          /Users/webmech/.local/share/dreamd/api-token (absent; created on first serve)
ok   launchd schedule   not installed (`dreamd schedule install` to run nightly)
ok   orphaned dreams    none
ok   last dream         none yet
```

| Check | FAIL when |
| --- | --- |
| `claude binary` | `claude_bin` is not found or not executable |
| `claude permissions` | `claude_bin` is a bypass wrapper (see above) |
| `config dirs` | none of `config_dirs` exists |
| `data dir` | `data_dir` cannot be created or written |
| `database` | `dreamd.db` cannot be opened |
| `api token` | `<data_dir>/api-token` is readable by group/other (must be 0600) |
| `launchd schedule` | the installed plist runs a `dreamd` binary that no longer exists |
| `orphaned dreams` | it just swept dreams left `pending`/`running` by a dead process (see Locking); the sweep repairs them, so the next run is ok |
| `last dream` | the database cannot be read (otherwise informational: newest dream's id, status, project, error) |

## The loop: scan → dream → diff → apply / rollback

```sh
dreamd scan                                   # dreamable projects, candidate sessions, gate verdict
dreamd dream -Users-webmech-dev-Tinkering     # by slug, or by the project's cwd
dreamd dream ~/dev/Tinkering --dry-run        # the plan: sessions to mine, no model calls
dreamd dream --all --dry-run                  # tonight's plan across every project
dreamd list                                   # dreams, newest first (--include-archived for all)
dreamd show drm_...                           # status, usage/cost, per-session results, summary
dreamd summary drm_...                        # the aggregator's summary (aggregate.md)
dreamd diff drm_... | less                    # unified diff memory → dreamed memory
dreamd apply drm_...                          # swap it in (backup kept)
dreamd rollback drm_...                       # restore the backup
dreamd archive drm_...                        # or set it aside without applying
```

`dream` options: `--force` (bypass the gate, including awaiting review, and
the mined-through cutoff: candidates are windowed by the last apply only),
`--model M` (aggregator model),
`--instructions "..."` (appended to the aggregator prompt verbatim),
`--sessions id,id` (explicit sessions; bypasses the gate and every cap),
`--max-sessions N` (overrides `max_sessions_per_dream`), `--since-days N`
(only transcripts modified in the last N days, replacing both the backfill
window and the mined-through cutoff; `0` = no window), `--dry-run` (print the
plan and run nothing), `--all` (every dreamable project, gate and run caps
applied; this is what the nightly job runs). `cancel <id>` cancels a pending or running dream.
`--json` works on the read commands (`scan`, `list`, `show`, `doctor`,
`schedule status`).

What a dream does (state is saved to SQLite after each stage):

0. **Discover.** A project is dreamable if its memory dir holds at least one
   `.md`. Candidate sessions are `*.jsonl` of at least `min_transcript_bytes`,
   newer than the project's mined-through cutoff (see The review loop; a
   never-dreamed project: within the last `backfill_days`) and older than 10 minutes (a file touched in the last 10
   minutes is a live session and is skipped), newest first, capped at 100.
   The gate counts these. A dream mines the newest `max_sessions_per_dream`
   of them; `--all` first plans the whole run (see Quota and caps).
1. **Snapshot + clone** the memory dir into `in/` and `out/`; hash `in/`.
2. **Reduce** each transcript in Rust (user/assistant text, tool names with a
   short input preview, tool errors; images and noise dropped).
3. **Workers** (fan-out, `concurrency` at a time): one `claude -p` per digest
   returning JSON findings: insights (each citing session id + a quote),
   stale files, corrections. A failed worker fails only its session; the
   dream fails if every worker fails.
4. **Aggregator** edits `out/` (see Security model).
5. **Post-check** (fail closed): the live memory dir is unchanged, deletion
   manifest entries are valid, every new or edited `.md` has `name` and
   `description` frontmatter, every `MEMORY.md` link resolves, nothing outside
   `out/` changed. Violations that already existed in your memory dir do not
   fail the dream; only new ones do. Then `dream.diff` and diff stats.
6. **Notify** via a macOS notification.

`apply` refuses (exit 3) unless the dream is `completed` and the live memory
dir still hashes the same as the snapshot. If you or a session wrote memory
since, rerun the dream, or pass `--force` to overwrite those writes. It copies
the live dir to `backup/`, stages `out/` next to the live dir, swaps by
rename, and records `last_dreamed_at`. `rollback` restores `backup/`.

### The review loop

A completed dream changes nothing until you review it, and dreamd waits for
you instead of piling up more dreams:

- **Mined-through cutoff.** When a dream reaches `completed`, dreamd records
  `last_mined_at` for its project (not for `failed` or `canceled` dreams).
  The project's cutoff is the later of `last_mined_at` and `last_dreamed_at`
  (set by `apply`): later dreams only mine transcripts modified after it, and
  `min_hours` counts from it. So an unapplied dream's sessions are never
  mined twice, and with `auto_apply = false` the nightly `--all` does not
  re-mine the same newest sessions every night. `scan` shows the cutoff as
  `last=<timestamp>`; `scan --json` has both `last_dreamed_at` and
  `last_mined_at`.
- **Awaiting review parks the project.** While a project has a `completed`
  dream that is not archived, the gate skips it — before the hours and
  sessions checks — with `awaiting review: <id> (apply, rollback or archive
  it)` (`scan` verdict, `dream` exit 2, `POST /v1/dreams` 409 `skipped`). A
  new dream would start from the live memory dir and lose that dream's
  insights. `--force` and `--sessions` bypass this, as they bypass the gate.
- **Unparking.** `apply` (the dream becomes `applied`), `rollback` of an
  applied dream, or `archive`. `dreamd archive <id>` sets a finished dream
  (completed, failed, canceled, applied or rolled_back) aside: it records
  `archived_at`, hides the dream from `list` (`list --include-archived`
  shows it with an `archived` marker; `show` always works) and stops it
  parking its project. It refuses a pending or running dream (`refused:
  dream is running`, exit 3) and is idempotent. Archiving does not move the
  cutoff back: the archived dream's sessions stay mined.

Upgrading from 0.1.0 adds the two columns on first open. A `completed` dream
made by 0.1.0 parks its project until you apply or archive it.

Data lives in `~/.local/share/dreamd/`: `dreamd.db`, and per dream
`dreams/<id>/{in/, in.sha256, out/, findings/<session>.json, context/,
aggregate.md, dream.diff, backup/}`.

### Locking

One dream per project at a time. Before a dream's row is created, dreamd
takes `<data_dir>/locks/<first 16 hex of sha256(project key)>.lock`
(created exclusively, holding the owner's pid) and keeps it until the dream
ends, whatever its status. A second dream of the same project, from the CLI
or the API, is refused while that pid is alive: `dream` prints
`busy <slug>: project is already being dreamed by pid <pid>` and exits 3
(`--all` counts it as skipped), `POST /v1/dreams` answers 409 `busy`. A lock
whose pid is dead or unreadable is stale and is reclaimed. A dream left
`pending`/`running` whose lock has no live holder (its process crashed or was
killed, e.g. a `serve` stopped mid-dream) is marked `failed` with
`orphaned: owning process died`; this sweep runs when `serve` starts and
before `dream`, `list`, `scan`, `cancel` and `doctor` (stderr:
`swept orphaned dream <id>`).

## Quota and caps

Every mined session is one `claude -p` worker call, and every dream adds one
aggregator call. Measured on a real machine (13 projects, a few months of
history), an uncapped first `dreamd dream --all` would have mined **381
sessions**, 91 of them in one project; at roughly **$0.40 of subscription
quota per worker call** that is about $150 of quota in one night. The
`[limits]` caps keep each run bounded:

- `min_transcript_bytes` (8192) drops tiny transcripts (aborted or one-line
  sessions) before anything else, so they neither count toward the gate nor
  get mined.
- `backfill_days` (14): a project that has never been dreamed only considers
  the last two weeks; after its first completed dream the cutoff is the
  mined-through time (the later of the last completed dream and the last
  apply), so nothing is mined twice whether or not you apply.
- A project with a completed dream awaiting review is skipped entirely
  (`awaiting review: <id>`), so an unattended `--all` spends nothing on it
  until you apply, roll back or `archive` that dream (see The review loop).
- `max_sessions_per_dream` (10): a dream mines the newest 10 candidates.
- `max_projects_per_run` (5) and `max_sessions_per_run` (30): `dream --all`
  orders due projects by their newest candidate (most recently active first)
  and hands out sessions newest-first until either budget is spent. A project
  that gets nothing prints `deferred <slug>: run budget exhausted (N
  candidates)`; it is first in line on a later night if it stays active.

So a nightly run is at most 30 worker calls plus 5 aggregator calls. Check
the plan before it runs:

```sh
dreamd scan                     # per project: sessions=<candidates>  selected=<a dream would mine>
                                # footer: next --all run: P projects, S worker calls (caps: …)
dreamd dream --all --dry-run    # plan <slug>: k of n sessions / deferred / skipped, then
                                # plan: P projects, S worker calls, P aggregator calls
```

**Backfilling deliberately.** To mine older history, do it one project at a
time, when you choose to spend the quota:

```sh
dreamd dream <slug> --since-days 0 --max-sessions 100 --dry-run   # see what it would cost
dreamd dream <slug> --since-days 0 --max-sessions 100
```

`--since-days 0` removes the window (and the mined-through cutoff) for that run;
100 is the hosted-API ceiling, the most one dream can take. `--sessions
id,id` picks exact transcripts and bypasses every cap.

## HTTP API

```sh
dreamd serve                 # listening on http://127.0.0.1:7720
                             # token file: ~/.local/share/dreamd/api-token
dreamd token                 # print the bearer token (creates it if absent)
```

Every route except `/healthz` needs `Authorization: Bearer $(dreamd token)`.
POST bodies must be `Content-Type: application/json`, which `curl --json`
sets for you.

```sh
TOK=$(dreamd token)
curl -s http://127.0.0.1:7720/healthz
curl -s -H "Authorization: Bearer $TOK" http://127.0.0.1:7720/v1/projects
curl -s -H "Authorization: Bearer $TOK" \
     --json '{"project":"-Users-webmech-dev-Tinkering","force":true}' \
     http://127.0.0.1:7720/v1/dreams                       # 202 + dream
curl -s -H "Authorization: Bearer $TOK" http://127.0.0.1:7720/v1/dreams/drm_.../diff
curl -s -H "Authorization: Bearer $TOK" -X POST http://127.0.0.1:7720/v1/dreams/drm_.../apply
```

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/healthz` | no token |
| GET | `/v1/projects` | dreamable projects + gate verdict |
| POST | `/v1/dreams` | `{project, instructions?, model?, session_ids?, force?}` → 202 with the pending dream; 409 `skipped` when gated, 409 `busy` when the project is already being dreamed |
| GET | `/v1/dreams[?status=&include_archived=true]` | list (archived dreams only with `include_archived=true`) |
| GET | `/v1/dreams/{id}` | one dream |
| GET | `/v1/dreams/{id}/diff` | `text/plain` |
| GET | `/v1/dreams/{id}/sessions` | per-session worker results |
| POST | `/v1/dreams/{id}/cancel` | |
| POST | `/v1/dreams/{id}/apply` | 409 on bad status / hash mismatch |
| POST | `/v1/dreams/{id}/rollback` | |
| POST | `/v1/dreams/{id}/archive` | 200 with `archived_at` set; 409 `refused` while pending/running |

Errors are RFC 7807 `application/problem+json`. Guards, in order: a `Host`
other than `127.0.0.1` / `localhost` (bare or with the bound port) → 403; any
`Origin` header other than `null` → 403 (browsers send one, curl does not);
missing or wrong bearer token → 401; a POST with a non-JSON `Content-Type`
(or `POST /v1/dreams` without one) → 415. To rotate the token, delete
`<data_dir>/api-token` and restart `serve`.

## Nightly schedule

Install the schedule **after** `cargo install`, running the installed binary:
the plist bakes in the path of the executable that installs it and your
current `PATH`.

```sh
~/.cargo/bin/dreamd schedule install --dry-run   # print the plist, change nothing
~/.cargo/bin/dreamd schedule install             # ~/Library/LaunchAgents/dev.dreamd.nightly.plist
dreamd schedule status
dreamd schedule uninstall
```

The job runs `dreamd dream --all` daily at `[schedule] hour:minute`, logging
to `<data_dir>/logs/nightly.out.log` and `nightly.err.log`. Quiet projects
are gate-skipped. With `[schedule] auto_apply = true` completed dreams are
applied straight away; a hash mismatch (memory changed during the dream) is
only a warning and that dream waits for you. `--all` exits 4 if any project
failed and 2 if all were skipped. Reinstall after changing the schedule time
or moving the binary (`doctor` flags a plist whose binary is gone).

## Security model

The aggregator reads model-generated findings mined from transcripts, so it
is treated as prompt-injectable. Containment, in layers:

- **Workers have no tools.** `--permission-mode plan`; the digest is inlined
  in the prompt.
- **The aggregator can only edit `out/`.** It runs with cwd = `out/`,
  `--permission-mode acceptEdits` and
  `--allowedTools "Read,Glob,Grep,Write(./**),Edit(./**)"`: no Bash, no
  `--add-dir`. (A bare `Write` rule would allow writes to any absolute path;
  the `./**` scope is what keeps it in `out/`.)
- **Deletions go through a manifest.** The aggregator lists files to delete in
  `out/.dreamd-delete`; Rust validates each entry (a bare `*.md` that exists
  in `out/`, no path separators, not `MEMORY.md`) and removes it. A bad entry
  fails the dream.
- **Project context is a copy.** The project's `CLAUDE.md` (or
  `.claude/CLAUDE.md`) and `AGENTS.md` are copied into `<dream_dir>/context/`
  and read from there. dreamd never edits CLAUDE.md; conflicts are reported
  in the summary.
- **Fail-closed checks.** Any change under the dream dir outside `out/` fails
  the dream, and so does any change to the live memory dir's hash while the
  aggregator ran.
- **Permission-mode assertion.** dreamd reads `permissionMode` from each run's
  init event: workers must report `plan`, the aggregator `acceptEdits`, or the
  session/dream fails.
- **Wrapper refusal.** A `claude_bin` that injects
  `--dangerously-skip-permissions` is refused at startup and flagged by
  `doctor` (see Configuration).
- **Loopback API.** Bound to 127.0.0.1 with Host/Origin guards (against DNS
  rebinding and cross-site requests from your browser), a 0600 bearer token,
  and JSON-only POSTs.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | ok |
| 1 | error (bad config, I/O, `doctor` found a FAIL) |
| 2 | gate-skipped (incl. awaiting review) / nothing to do |
| 3 | refused: hash mismatch, bad status (incl. archiving a pending/running dream), unknown dream or project, project busy |
| 4 | dream failed (with `--all`: any project failed) |

## Troubleshooting

- **`claude_bin ... is a wrapper that injects --dangerously-skip-permissions`**:
  set `claude_bin` to the real binary's absolute path (on Mike's machine
  `/Users/webmech/.local/bin/_claude`). `doctor` shows it under
  `claude permissions`.
- **`claude binary FAIL`**: `claude_bin` is not tilde-expanded; use an
  absolute path.
- **`skipped <slug>: ...`, exit 2**: the gate. Use `--force`, or
  `--sessions`.
- **A dream fails with `live memory dir changed during aggregation`**: a live
  Claude Code session wrote that project's memory while the dream ran. This is
  deliberate (fail closed). Rerun when the session is idle.
- **A dream fails with a permission-mode mismatch**: `claude_bin` is running
  in a mode other than the one requested, almost always a wrapper.
- **`apply` refused, hash mismatch**: memory changed since the snapshot.
  Rerun the dream (or `apply --force` to drop those changes).
- **A dream stuck in `running`**: if its process is gone, the next `dreamd
  list` (or `doctor`) sweeps it to `failed` (see Locking); if it is still
  running, `dreamd cancel <id>`.
- **`busy <slug>: project is already being dreamed by pid <pid>`**: wait for
  that dream, or `dreamd cancel` it.
- **Nightly job did nothing**: check `<data_dir>/logs/nightly.err.log` and
  `dreamd doctor` (`launchd schedule` line).
- More logging: `DREAMD_LOG=info dreamd dream ...`. Every artifact (findings,
  summary, diff, `out/`) stays in the dream dir, including on failure.

## Live smoke

Run once against real `claude`, never against the real memory dir:

```sh
SRC=~/.claude/projects/-Users-webmech-dev-Tinkering
T=$(mktemp -d); P=$T/claude/projects/-Users-webmech-dev-Tinkering; mkdir -p $P
(cd $SRC && ls -S -r *.jsonl | head -3 | xargs -I{} cp {} $P/)   # three smallest sessions
cp -R $SRC/memory $P/memory                                      # a COPY of the memory dir
touch -t $(date -v-2H +%Y%m%d%H%M) $P/*.jsonl                    # not "live"
cat > $T/config.toml <<'EOF'
claude_bin = "/Users/webmech/.local/bin/_claude"
[worker]
model = "haiku"
effort = "low"
[aggregator]
model = "sonnet"
effort = "medium"
timeout_secs = 900
EOF
export DREAMD_CONFIG_DIRS=$T/claude DREAMD_DATA_DIR=$T/data DREAMD_CONFIG=$T/config.toml DREAMD_NO_NOTIFY=1
dreamd doctor && dreamd dream -Users-webmech-dev-Tinkering --force
dreamd show <id>; dreamd diff <id> | head -60; dreamd summary <id>
```

Result (2026-11-02, v0.1.0): `dream drm_01M6F2FMMG24RNSZNWZTTS93V4 completed:
3 sessions mined, +3 -0 ~4 files` in 2 min 04 s, **cost_usd = 2.0611**
(three haiku workers at about $0.43 each, the rest the sonnet aggregator).
What the dream changed in the copied memory dir:

- Added three evidence-cited memories: the `slack-clean-dm` tool,
  Slack's eventual consistency after bulk deletes, and a feedback rule to
  verify research-subagent claims against primary sources.
- Updated `a2a-project.md` (a dated 2026-11-02 block: `pow` scope = session
  directives only, push authorized when green), `precompact-herdr-project.md`
  and `claude-remote-managed-settings.md` (managed `claudeMd` is additive).
- Rewrote `MEMORY.md` to index the new files; deleted nothing (it rejected a
  worker's "stale" flag on `depot-multi-account.md` as not backed by the
  quote) and found no CLAUDE.md conflicts.

Every `claude -p` run carries a fixed overhead (Claude Code's system prompt
plus your user-level CLAUDE.md and tool definitions): a trivial
`dreamd claude-probe "reply with exactly: hi" --model haiku` cost $0.10 on
the same machine. Budget about $0.40 per mined session plus the aggregator.

## Known limitations

- The lock is keyed by pid: if a crashed dream's pid is reused by an
  unrelated process before the sweep runs, the project stays busy until that
  process exits (or the lock file is removed by hand).
- A live Claude Code session writing that project's memory during a dream
  fails the dream (by design).
- `autoMemoryDirectory` is read from `<config_dir>/settings.json` only, not
  from project-level settings.
- macOS only for the schedule (launchd) and notifications.
- Team memory (`team/`), CLAUDE.md edits and Managed Agents memory stores are
  out of scope.

## Development

```sh
cargo test                       # unit + integration, offline
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

The integration tests never call a model: they point `DREAMD_CLAUDE_BIN` at
`tests/stub-claude.sh`, a fake `claude -p` that prints the same JSON event
array. `DREAMD_STUB_MODE` picks its behaviour: `worker` (canned findings),
`aggregator` (edits `out/`), `auto` (worker when `--json-schema` is passed,
else aggregator), `fail`, `sleep`, `badjson`; see the script header for the
escape/delete/live-dir-touch knobs. `tests/common/mod.rs` builds a temp
config dir with a project, a memory dir and three transcripts with controlled
mtimes.

Hidden debug subcommands exercise one stage at a time:

```sh
dreamd digest <session.jsonl> [--max-bytes N]          # reduced transcript
dreamd claude-probe "reply with exactly: hi" [--model M] [--timeout S]
dreamd mine --dream-dir <dir> <session.jsonl>...       # worker fan-out → <dir>/findings/
dreamd postcheck <in_dir> <out_dir>                    # post-check + diff stats
```

Design: `docs/superpowers/specs/2026-11-02-dreamd-design.md`. Plan:
`docs/superpowers/plans/2026-11-02-dreamd.md`.

## License

MIT
