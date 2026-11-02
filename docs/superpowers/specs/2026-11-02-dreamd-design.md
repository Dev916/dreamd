# dreamd — a local dream machine for Claude Code memory

Date: 2026-11-02. Author: Claude (design calls marked as mine; Mike opted out of the brainstorm questions and asked to just build it).

## Intent

Claude Code ships "auto-dream" (background memory consolidation) but it is gated behind a server-side flag (`tengu_onyx_plover`) that is off for all of Mike's accounts. Anthropic's hosted Dreams API (`POST /v1/dreams`) does the same job for Managed Agents memory stores but needs research-preview access. We build the local equivalent: an out-of-band service that, on a schedule or on demand, reads the session transcripts that touched a project since its last dream, spawns one sub-agent per session to mine insights, then runs one aggregator pass that rewrites a **clone** of the project's auto-memory directory. The human reviews the diff and applies or discards it. Input memory is never modified in place.

Why this shape: it mirrors both things we verified. Anthropic's local auto-dream recipe (fork a subagent, read-only shell plus `rm` of memory `.md` files, phases: skim → merge duplicates → prune stale → reconcile against CLAUDE.md → summarize, gate on ≥24 h and ≥3 sessions, lock file) and the hosted Dreams API (inputs = one memory store + 1..100 sessions, `model`, `instructions`; output = a new store; statuses pending/running/completed/failed/canceled; archive/cancel).

## Goals

1. One binary, `dreamd`, that is both the CLI and the local HTTP API server.
2. Dream any Claude Code project's auto-memory directory across all of Mike's config dirs (`~/.claude`, `~/.claude-revenium`, `~/.claude-clever`).
3. Per-session sub-agents (fan-out) feeding one aggregator (fan-in), all via `claude -p`, so it rides the existing OAuth accounts and the `claude-acct` shim (no API key).
4. Output is a cloned memory store plus a unified diff and a summary. Apply is explicit, atomic, backed up, and reversible.
5. Nightly launchd schedule with the same gating as Anthropic's (min hours, min sessions), notification when a dream is ready for review.
6. Verifiable without spending tokens: every pipeline stage runs against a stub `claude` in tests.

## Non-goals (v1)

- Team/shared memory (`team/` subdir) semantics, multi-machine, cloud sync.
- Editing CLAUDE.md. The aggregator may *flag* a CLAUDE.md conflict in its summary; it never writes to it.
- Auto-apply by default. There is an opt-in flag, off.
- A web UI or SSE event stream. The API is JSON over HTTP on loopback.
- Dreaming Managed Agents memory stores. (Could later be a second backend.)

## Anti-goals (would make it a failure)

- Losing or corrupting a memory directory. Hence: never write to the input; apply only after an unchanged-since-snapshot check; backup before swap; rollback command.
- A dream silently inventing facts. Every worker insight must cite evidence (session id + quoted snippet), and the aggregator prompt forbids adding memories without a cited insight.
- Burning a day's quota. Concurrency and per-worker byte/time caps are hard limits; transcripts are reduced before they reach a model.

## Constraints and givens

- Rust 1.93, tokio, clap 4, axum 0.8, rusqlite (bundled), serde/serde_json, ulid, chrono, similar (diff), walkdir, tempfile, toml, dirs, anyhow/thiserror, tracing. Mirrors the stack already used in `~/dev/a2a` and `hq`.
- `claude -p <prompt> --output-format json --no-session-persistence --model <m> --effort <e> [--json-schema <schema>] [--allowedTools ...] [--permission-mode ...] [--add-dir ...] < /dev/null` emits a JSON **array** of events; the last element has `type: "result"`, with `result` (text), `is_error`, `subtype`, `total_cost_usd`, `usage`, `duration_ms`, `session_id`. stdin must be redirected from `/dev/null`.
- Transcripts live at `<config_dir>/projects/<slug>/<session-uuid>.jsonl` (one file per session, JSONL of typed records: `user`, `assistant`, `system`, `progress`, tool results, etc.). Memory dir defaults to `<config_dir>/projects/<slug>/memory/` with `MEMORY.md` index plus one fact per `.md` file with YAML frontmatter (`name`, `description`, `metadata.type`). A project may override via `autoMemoryDirectory` in settings; v1 reads that key from `<config_dir>/settings.json` only.
- The `claude` on PATH is Mike's account-selecting shim; `-p` runs rotate accounts. `dreamd` must call `claude` by PATH name (configurable `claude_bin`).

## The parts of the how (decided)

### Storage

`~/.local/share/dreamd/`:
- `dreamd.db` (SQLite): `projects`, `dreams`, `dream_sessions`, `applies`.
- `dreams/<dream_id>/in/` snapshot of the input memory dir at dream start; `out/` the clone the aggregator edits; `findings/<session_id>.json` worker outputs; `aggregate.md` summary; `dream.diff` unified diff in→out; `backup/` the pre-apply copy of the live memory dir (written at apply time).

`~/.config/dreamd/config.toml` (all optional, with defaults):
```toml
config_dirs = ["~/.claude", "~/.claude-revenium", "~/.claude-clever"]
claude_bin = "claude"
min_hours = 24
min_sessions = 3
exclude_slugs = []
port = 7720
[worker]   # per-session miner
model = "sonnet"; effort = "low"; concurrency = 3; timeout_secs = 600; max_transcript_bytes = 200000
[aggregator]
model = "opus"; effort = "high"; timeout_secs = 1800
[schedule]
hour = 3; minute = 30; auto_apply = false
```

### Dream object (mirrors the hosted API)

`{ id: "drm_<ulid>", project: {config_dir, slug, cwd, memory_dir}, status: pending|running|completed|failed|canceled|applied|rolled_back, model, instructions?, session_ids[], created_at, ended_at?, usage {input_tokens, output_tokens, cost_usd}, summary?, error?, output_dir, diff_stats {files_added, files_removed, files_changed} }`.

### Pipeline (`dreamd dream <selector>`)

0. **Discover.** Walk each config dir's `projects/*/`; a project is dreamable if it has a memory dir with ≥1 `.md`. Candidate sessions = `*.jsonl` whose mtime > project's `last_dreamed_at` (or all if never) and mtime older than 10 min (skip live sessions). Gate: skip unless `--force` or (hours since last ≥ min_hours and candidate count ≥ min_sessions). Cap at 100 sessions, newest first (matches hosted limit).
1. **Snapshot + clone.** Copy memory dir → `in/` and `out/`. Record a content hash of `in/`.
2. **Reduce transcripts (Rust, no model).** For each session JSONL: keep user text, assistant text, tool_use `name` + a 200-char input preview, tool_result error text; drop images/base64, progress, system noise. Emit a compact text digest. If it exceeds `max_transcript_bytes`, keep head 40% + tail 40% with a `[... elided N bytes ...]` marker.
3. **Fan-out workers.** For each digest, run one `claude -p` with `--json-schema` (findings schema below), `--permission-mode plan` (no tools needed; digest is inlined in the prompt), model/effort from `[worker]`, bounded by a tokio semaphore (`concurrency`) and `tokio::time::timeout`. Prompt gives: the project's `MEMORY.md` index (so it knows what exists), the digest, and the four memory types. A failed or timed-out worker is recorded per session and does not fail the dream unless *all* workers fail.
   Findings schema: `{ insights: [{ kind: user|feedback|project|reference, statement, why, evidence: {session_id, quote}, confidence: 0..1, relates_to: [memory file names] }], stale: [{ file, reason, evidence }], corrections: [{ file, wrong, right, evidence }] }`.
4. **Aggregate.** One `claude -p` with `cwd = out/`, `--permission-mode bypassPermissions`, `--allowedTools "Read,Write,Edit,Glob,Grep,Bash(ls:*),Bash(rm:*)"`, `--add-dir <project cwd>` read-only context for the project's CLAUDE.md (path passed in the prompt; writes outside `out/` are forbidden by prompt and caught by the post-check). Prompt = Anthropic's four phases adapted: (1) read `MEMORY.md` and every `.md`; (2) fold in `findings/*.json`: add a memory only when backed by evidence, prefer updating an existing file; (3) merge near-duplicates, delete contradicted or stale files, keep the one-fact-per-file + frontmatter convention; (4) reconcile against CLAUDE.md (never edit it, annotate and report conflicts); rewrite `MEMORY.md` so every line points at an existing file; finish with a ≤300-word summary. Operator `instructions` are appended verbatim.
5. **Post-check (Rust).** Every `.md` in `out/` except `MEMORY.md` has frontmatter with `name` and `description`; every `MEMORY.md` link resolves; nothing outside `out/` under the dream dir changed; compute unified diff `in/`→`out/` (`similar`), store `dream.diff` and stats. Fail the dream (status `failed`, output kept) if checks fail.
6. **Notify.** `osascript -e 'display notification …'` when a dream completes (best-effort, macOS only).

### Apply / rollback

`dreamd apply <id>`: refuse unless status `completed`; recompute the live memory dir's hash and refuse if it differs from the `in/` hash (someone wrote memory since; rerun the dream) unless `--force`; copy live dir → `backup/`; stage `out/` to a sibling temp dir on the same filesystem; swap via rename (`memory` → `memory.dreamd-old`, temp → `memory`, remove old); set status `applied`, update `last_dreamed_at`. `dreamd rollback <id>`: reverse swap from `backup/`, status `rolled_back`. Both print the paths touched.

### CLI

```
dreamd scan [--all-dirs]                 # projects, sessions since last dream, gate verdict
dreamd dream <slug|cwd|--all> [--force] [--model M] [--instructions "…"] [--sessions id,id] [--dry-run]
dreamd list | show <id> | diff <id> | summary <id> | cancel <id>
dreamd apply <id> [--force] | rollback <id>
dreamd serve [--port 7720]
dreamd schedule install|uninstall|status   # launchd dev.dreamd.nightly
dreamd doctor                             # claude on PATH, config dirs, db, disk
```
Exit codes: 0 ok, 2 gate-skipped/nothing to do, 3 refused (hash mismatch, bad status), 4 dream failed. `--json` on read commands.

### HTTP API (`dreamd serve`, 127.0.0.1 only)

`GET /v1/projects`, `POST /v1/dreams {project, instructions?, model?, session_ids?, force?}` → 202 dream, `GET /v1/dreams[?status=]`, `GET /v1/dreams/{id}`, `GET /v1/dreams/{id}/diff` (text/plain), `GET /v1/dreams/{id}/sessions`, `POST /v1/dreams/{id}/cancel|apply|rollback`, `GET /healthz`. Dreams started over HTTP run as background tasks in the server process and persist state in SQLite so the CLI sees them. Errors are RFC 7807 problem JSON.

### Scheduling

`dreamd schedule install` writes `~/Library/LaunchAgents/dev.dreamd.nightly.plist` running `dreamd dream --all` at `[schedule]` time with stdout/stderr to `~/.local/share/dreamd/logs/`. `--all` applies the gate per project, so a quiet project is skipped at exit 2. With `auto_apply = true` it applies dreams whose post-check passed; default off.

### Testing strategy

- Unit: transcript reducer (fixtures of real-shaped JSONL), discovery + gate math, frontmatter/index post-check, diff stats, apply/rollback on tempdirs including the hash-mismatch refusal and a crash between backup and swap.
- Integration: `CLAUDE_BIN`/`claude_bin` pointed at `tests/stub-claude.sh`, which returns canned event arrays (worker findings for a given digest, an aggregator that edits `out/`), so `dreamd dream --force` runs end to end offline; HTTP tests with `axum::test`-style in-process server.
- One live smoke (manual, documented): a fixture project with 3 tiny transcripts dreamed with `[worker] model = "haiku"` and aggregator `sonnet`, cost printed.

## Left to the builder

Exact prompt wording (keep Anthropic's phase structure and the evidence rule), SQLite schema details and migrations, the digest format, log formatting, how `--sessions` interacts with the gate (it bypasses it), and the `doctor` checklist. Repo conventions: beads (`bd`) if initialised, `cargo fmt`/`clippy` clean, tests green before each commit.
