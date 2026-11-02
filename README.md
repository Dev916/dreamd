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
min_hours     = 24                  # gate: hours since the last dream
min_sessions  = 3                   # gate: candidate sessions since the last dream
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
| `last dream` | the database cannot be read (otherwise informational: newest dream's id, status, project, error) |

## The loop: scan → dream → diff → apply / rollback

```sh
dreamd scan                                   # dreamable projects, candidate sessions, gate verdict
dreamd dream -Users-webmech-dev-Tinkering     # by slug, or by the project's cwd
dreamd dream ~/dev/Tinkering --dry-run        # what would be mined, no model calls
dreamd list                                   # all dreams, newest first
dreamd show drm_...                           # status, usage/cost, per-session results, summary
dreamd summary drm_...                        # the aggregator's summary (aggregate.md)
dreamd diff drm_... | less                    # unified diff memory → dreamed memory
dreamd apply drm_...                          # swap it in (backup kept)
dreamd rollback drm_...                       # restore the backup
```

`dream` options: `--force` (bypass the gate), `--model M` (aggregator model),
`--instructions "..."` (appended to the aggregator prompt verbatim),
`--sessions id,id` (explicit sessions; bypasses the gate), `--dry-run`,
`--all` (every dreamable project, gate applied per project; this is what the
nightly job runs). `cancel <id>` cancels a pending or running dream.
`--json` works on the read commands (`scan`, `list`, `show`, `doctor`,
`schedule status`).

What a dream does (state is saved to SQLite after each stage):

0. **Discover.** A project is dreamable if its memory dir holds at least one
   `.md`. Candidate sessions are `*.jsonl` newer than the last dream and older
   than 10 minutes (a file touched in the last 10 minutes is a live session
   and is skipped), newest first, capped at 100.
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

Data lives in `~/.local/share/dreamd/`: `dreamd.db`, and per dream
`dreams/<id>/{in/, in.sha256, out/, findings/<session>.json, context/,
aggregate.md, dream.diff, backup/}`.

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
| POST | `/v1/dreams` | `{project, instructions?, model?, session_ids?, force?}` → 202 with the pending dream; 409 `skipped` when gated |
| GET | `/v1/dreams[?status=]` | list |
| GET | `/v1/dreams/{id}` | one dream |
| GET | `/v1/dreams/{id}/diff` | `text/plain` |
| GET | `/v1/dreams/{id}/sessions` | per-session worker results |
| POST | `/v1/dreams/{id}/cancel` | |
| POST | `/v1/dreams/{id}/apply` | 409 on bad status / hash mismatch |
| POST | `/v1/dreams/{id}/rollback` | |

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
| 2 | gate-skipped / nothing to do |
| 3 | refused: hash mismatch, bad status, unknown dream or project |
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
- **A dream stuck in `running`**: see Known limitations; `dreamd cancel <id>`.
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

- A dream started by `serve` runs as a detached task in the server process.
  Killing the server mid-dream leaves the row `running`; there is no startup
  sweep yet. `dreamd cancel <id>` clears it.
- There is no per-project lock: don't start two dreams of the same project at
  once (the second one's apply would be refused by the hash check, but both
  spend tokens).
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
