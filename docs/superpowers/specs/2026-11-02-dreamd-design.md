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
- Giving the aggregator a shell or the project tree. It edits `out/` only (acceptEdits + cwd-scoped Write/Edit, no Bash, no `--add-dir`); deletions go through a Rust-validated manifest and project context arrives as copies in `context/`.
- Running under a `claude` wrapper that injects `--dangerously-skip-permissions` (such as the account-rotating `~/.local/bin/claude`). dreamd refuses such a `claude_bin`.

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
- `dreamd.db` (SQLite): `projects` (incl. `last_dreamed_at`, set by apply, and `last_mined_at`, set when a dream reaches `completed`), `dreams` (incl. `archived_at`), `dream_sessions`, `applies`. Columns added after 0.1.0 are added by a guarded `ALTER TABLE` on open, so older databases upgrade in place.
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
[limits]   # quota caps: each mined session is one `claude -p` call (~$0.40 of quota)
max_sessions_per_dream = 10; max_sessions_per_run = 30; max_projects_per_run = 5
backfill_days = 14; min_transcript_bytes = 8192
```

### Dream object (mirrors the hosted API)

`{ id: "drm_<ulid>", project: {config_dir, slug, cwd, memory_dir}, status: pending|running|completed|failed|canceled|applied|rolled_back, model, instructions?, session_ids[], created_at, ended_at?, usage {input_tokens, output_tokens, cost_usd}, summary?, error?, output_dir, diff_stats {files_added, files_removed, files_changed}, archived_at? }`. `archived_at` is set by `dreamd archive` / `POST /v1/dreams/{id}/archive` (terminal statuses only; idempotent): the dream is hidden from `list` / `GET /v1/dreams` unless `--include-archived` / `?include_archived=true`, and no longer parks its project.

### Pipeline (`dreamd dream <selector>`)

0. **Discover and plan.** Walk each config dir's `projects/*/`; a project is dreamable if it has a memory dir with ≥1 `.md`. Candidate sessions = `*.jsonl` of ≥ `min_transcript_bytes` whose mtime > the project's mined-through cutoff `max(last_dreamed_at, last_mined_at)` (never dreamed: within the last `backfill_days`, 0 = all; `--since-days N` replaces both for one run; `--force` uses `last_dreamed_at` alone, so a forced re-dream can re-mine an unapplied dream's sessions) and mtime older than 10 min (skip live sessions), newest first, capped at 100 (matches hosted limit). Gate: `--force` always runs; otherwise a project with a `completed`, unarchived dream is skipped first with `awaiting review: <id> (apply, rollback or archive it)` (a new dream would start from the live memory dir and lose that dream's insights); then skip unless hours since the cutoff ≥ min_hours and candidate count ≥ min_sessions. Explicit `--sessions` bypasses the gate. When a dream reaches `completed` (not failed/canceled), `last_mined_at = now` for its project. A dream mines the newest `max_sessions_per_dream` (`--max-sessions N`) candidates; explicit `--sessions` bypasses every cap. `--all` plans first: due projects ordered by newest candidate mtime, at most `max_projects_per_run`, sessions allocated newest-first until `max_sessions_per_run` is spent (a project may get fewer than the per-dream cap); a due project left with nothing prints `deferred <slug>: run budget exhausted (N candidates)` and counts as skipped. `--dry-run` prints the plan (`plan <slug>: k of n sessions`, deferred/skipped lines, `plan: P projects, S worker calls, P aggregator calls`) and runs nothing (exit 2 if empty). Measured motivation: an uncapped first `--all` on the author's machine would mine 381 sessions across 13 projects (91 in one).
1. **Snapshot + clone.** Copy memory dir → `in/` and `out/`. Record a content hash of `in/`.
2. **Reduce transcripts (Rust, no model).** For each session JSONL: keep user text, assistant text, tool_use `name` + a 200-char input preview, tool_result error text; drop images/base64, progress, system noise. Emit a compact text digest. If it exceeds `max_transcript_bytes`, keep head 40% + tail 40% with a `[... elided N bytes ...]` marker.
3. **Fan-out workers.** For each digest, run one `claude -p` with `--json-schema` (findings schema below), `--permission-mode plan` (no tools needed; digest is inlined in the prompt), model/effort from `[worker]`, bounded by a tokio semaphore (`concurrency`) and `tokio::time::timeout`. Prompt gives: the project's `MEMORY.md` index (so it knows what exists), the digest, and the four memory types. A failed or timed-out worker is recorded per session and does not fail the dream unless *all* workers fail.
   Findings schema: `{ insights: [{ kind: user|feedback|project|reference, statement, why, evidence: {session_id, quote}, confidence: 0..1, relates_to: [memory file names] }], stale: [{ file, reason, evidence }], corrections: [{ file, wrong, right, evidence }] }`.
4. **Aggregate.** First copy the project's CLAUDE.md (`<cwd>/CLAUDE.md`, else `<cwd>/.claude/CLAUDE.md`) and `<cwd>/AGENTS.md` when present into `<dream_dir>/context/`; then take the escape snapshot of the dream dir and a content hash of the live memory dir. Run one `claude -p` with `cwd = out/`, `--permission-mode acceptEdits`, `--allowedTools "Read,Glob,Grep,Write(./**),Edit(./**)"`, no Bash and no `--add-dir`. The project tree is never mounted: the prompt references `../context/CLAUDE.md` (and `../context/AGENTS.md`) relative to `out/`, and findings by their paths under `findings/`; when the project has no CLAUDE.md the prompt says so and the reconcile phase skips it. The aggregator cannot delete files: it lists them, one bare filename per line, in `out/.dreamd-delete` with the Write tool. Prompt = Anthropic's four phases adapted: (1) read `MEMORY.md` and every `.md`; (2) fold in `findings/*.json`: add a memory only when backed by evidence, prefer updating an existing file; (3) merge near-duplicates, delete (via the manifest) contradicted or stale files, keep the one-fact-per-file + frontmatter convention; (4) reconcile against the CLAUDE.md copy (never edit it, annotate and report conflicts); rewrite `MEMORY.md` so every line points at an existing file; finish with a ≤300-word summary. Operator `instructions` are appended verbatim. If the run's `system/init` event reports a permission mode other than `acceptEdits`, the dream fails (see Security model).
5. **Post-check (Rust, fail closed).** The live memory dir must hash the same as before the aggregator, else the dream fails with `live memory dir changed during aggregation`. Then apply `out/.dreamd-delete`: each line must be a plain `*.md` filename (no path separators, not `..`, not `MEMORY.md`) that exists in `out/`; valid entries are removed, any invalid entry is a `BadDeleteEntry` violation, and the manifest is always removed before the diff. Then: Every `.md` in `out/` except `MEMORY.md` has frontmatter with `name` and `description`; every `MEMORY.md` link resolves; nothing outside `out/` under the dream dir changed; compute unified diff `in/`→`out/` (`similar`), store `dream.diff` and stats. Fail the dream (status `failed`, output kept) if checks fail.
6. **Notify.** `osascript -e 'display notification …'` when a dream completes (best-effort, macOS only).

### Security model

The aggregator reads model-generated findings mined from transcripts, so it is treated as prompt-injectable. Containment, in layers:

- **acceptEdits with a scoped allowlist.** `--permission-mode acceptEdits`, `--allowedTools "Read,Glob,Grep,Write(./**),Edit(./**)"`. A bare `Write`/`Edit` allow rule permits writes to *any* absolute path even under acceptEdits (live-probed 2026-11-02: a `/tmp` write outside cwd succeeded), so write rules are scoped to `./**` (the cwd, `out/`). Probed: reads of `../context/` and `../findings/` succeed; a Write to `../in/evil.md` is denied (`permission_denials`). Workers run in `plan` mode with no tools.
- **No Bash, no extra dirs.** No `Bash(...)` rule at all and no `--add-dir`; the project tree is not reachable for writes.
- **Manifest deletions.** The aggregator lists deletions in `out/.dreamd-delete`; Rust validates each entry (bare `*.md`, no separators, not `..`, not `MEMORY.md`, exists in `out/`) and removes it. Invalid entries fail the dream.
- **Context copy.** CLAUDE.md / AGENTS.md are copied into `<dream_dir>/context/` before the escape snapshot and referenced relatively; they are read-only context.
- **Escape and live-dir checks (fail closed).** Anything outside `out/` under the dream dir that changes fails the dream, and so does any change to the live memory dir's content hash during aggregation (defence in depth; acceptEdits already denies those writes).
- **Permission-mode assertion.** dreamd parses `permissionMode` from each run's `system/init` event. The aggregator must report `acceptEdits` and each worker `plan`; a mismatch fails the dream (or that worker's session) with a hint that `claude_bin` may be a bypass wrapper.
- **Wrapper detection.** At startup (`Ctx::new`) and in `doctor` (`claude permissions`), dreamd resolves `claude_bin` via `PATH`; if it is a text script containing `--dangerously-skip-permissions` dreamd refuses to run (override: `DREAMD_ALLOW_BYPASS_WRAPPER=1`, for tests only). **The account-rotating wrapper at `~/.local/bin/claude` cannot be used as `claude_bin`**: it always prepends `--dangerously-skip-permissions`, which turns every run into `bypassPermissions` (observed in the init event) and voids all of the above. Point `claude_bin` at the real binary (e.g. `~/.local/bin/_claude`).
- **Loopback API guards.** Binding to 127.0.0.1 does not stop a web page in the user's browser from reaching the API (DNS rebinding: a hostname re-pointed at 127.0.0.1; cross-site `POST` from any origin). Every request passes one axum middleware layer, in order: (1) `Host` must be `127.0.0.1`, `127.0.0.1:<port>`, `localhost` or `localhost:<port>` (the bound port), else 403 `forbidden` — a rebound request carries the attacker's host name; (2) an `Origin` header other than exactly `null` gets 403 `forbidden` — browsers send Origin on cross-site requests, curl and SDK clients never do; (3) every route except `GET /healthz` requires `Authorization: Bearer <token>`, compared in constant time with `<data_dir>/api-token`, else 401 `unauthorized`; (4) a `POST` whose `Content-Type` is present must be `application/json` (parameters allowed), and `POST /v1/dreams` must send it, else 415 `unsupported media type` — this rules out the "simple" content types an HTML form can send without a CORS preflight. The token is 32 random bytes (hex), created with mode 0600 on first `serve` (or `dreamd token`) and reused after; `serve` prints the file's path, never the token. `doctor` reports `api token` ok (absent, or 0600) or FAIL when group/other can access it.

### Apply / rollback

`dreamd apply <id>`: refuse unless status `completed`; recompute the live memory dir's hash and refuse if it differs from the `in/` hash (someone wrote memory since; rerun the dream) unless `--force`; copy live dir → `backup/`; stage `out/` to a sibling temp dir on the same filesystem; swap via rename (`memory` → `memory.dreamd-old`, temp → `memory`, remove old); set status `applied`, update `last_dreamed_at`. `dreamd rollback <id>`: reverse swap from `backup/`, status `rolled_back`. Both print the paths touched.

### Locking

One dream per project at a time. `prepare` takes `<data_dir>/locks/<sha256(project key) hex, first 16>.lock` (`create_new`, content = the owner's pid) *before* inserting the `pending` row, and the lock travels with the dream until `execute` returns (any terminal status); drop removes the file only while it still holds our pid. A held lock whose pid is alive (`kill(pid, 0)`) refuses the second dream: CLI `busy <slug>: project is already being dreamed by pid <pid>`, exit 3 (`--all` counts it as skipped); API 409 `busy`. A dead or unparsable pid is stale: the file is removed and the take retried once. Because every `pending`/`running` row has a lock, a row whose lock has no live holder is orphaned (its process crashed or was killed, e.g. `serve` stopped mid-dream): `sweep_orphans` marks it `failed` with `orphaned: owning process died` (a conditional update, so a dream finishing concurrently is never overwritten). The sweep runs at `serve` start and before `dream`, `list`, `scan`, `cancel` and `doctor` (`orphaned dreams` line).

### CLI

```
dreamd scan [--all-dirs]                 # projects, sessions + selected, gate verdict, next --all plan
dreamd dream <slug|cwd|--all> [--force] [--model M] [--instructions "…"] [--sessions id,id] [--since-days N] [--max-sessions N] [--dry-run]
dreamd list [--include-archived] | show <id> | diff <id> | summary <id> | cancel <id>
dreamd apply <id> [--force] | rollback <id> | archive <id>
dreamd serve [--port 7720]
dreamd token                              # print the API bearer token (creates it if absent)
dreamd schedule install|uninstall|status   # launchd dev.dreamd.nightly
dreamd doctor                             # claude on PATH, config dirs, db, disk
```
Exit codes: 0 ok, 2 gate-skipped/nothing to do, 3 refused (hash mismatch, bad status, project busy), 4 dream failed. `--json` on read commands.

### HTTP API (`dreamd serve`, 127.0.0.1 only)

`GET /v1/projects`, `POST /v1/dreams {project, instructions?, model?, session_ids?, force?}` → 202 dream, `GET /v1/dreams[?status=]`, `GET /v1/dreams/{id}`, `GET /v1/dreams/{id}/diff` (text/plain), `GET /v1/dreams/{id}/sessions`, `POST /v1/dreams/{id}/cancel|apply|rollback`, `POST /v1/dreams/{id}/archive` (200 dream / 409 refused while pending or running), `GET /v1/dreams?include_archived=true`, `GET /healthz`. Dreams started over HTTP run as background tasks in the server process and persist state in SQLite so the CLI sees them. Errors are RFC 7807 problem JSON.

Every route except `/healthz` requires `Authorization: Bearer <token>`, the token being the content of `<data_dir>/api-token` (created mode 0600 on first `serve`; `dreamd token` prints it, e.g. `curl -H "Authorization: Bearer $(dreamd token)" http://127.0.0.1:7720/v1/projects`); missing or wrong → 401 `unauthorized`. Every route, `/healthz` included, answers 403 `forbidden` to a `Host` other than `127.0.0.1`/`localhost` (bare or with the bound port) and to any `Origin` other than `null`. `POST` bodies must be `Content-Type: application/json` (`POST /v1/dreams` must send the header; bodyless `cancel|apply|rollback|archive` may omit it), else 415 `unsupported media type`. `serve` prints `listening on http://127.0.0.1:<port>` then `token file: <path>`. See Security model.

### Scheduling

`dreamd schedule install` writes `~/Library/LaunchAgents/dev.dreamd.nightly.plist` running `dreamd dream --all` at `[schedule]` time with stdout/stderr to `~/.local/share/dreamd/logs/`. `--all` applies the gate per project, so a quiet project is skipped at exit 2. With `auto_apply = true` it applies dreams whose post-check passed; default off.

### Testing strategy

- Unit: transcript reducer (fixtures of real-shaped JSONL), discovery + gate math, frontmatter/index post-check, diff stats, apply/rollback on tempdirs including the hash-mismatch refusal and a crash between backup and swap.
- Integration: `CLAUDE_BIN`/`claude_bin` pointed at `tests/stub-claude.sh`, which returns canned event arrays (worker findings for a given digest, an aggregator that edits `out/`), so `dreamd dream --force` runs end to end offline; HTTP tests with `axum::test`-style in-process server.
- One live smoke (manual, documented): a fixture project with 3 tiny transcripts dreamed with `[worker] model = "haiku"` and aggregator `sonnet`, cost printed.

## Left to the builder

Exact prompt wording (keep Anthropic's phase structure and the evidence rule), SQLite schema details and migrations, the digest format, log formatting, how `--sessions` interacts with the gate (it bypasses it), and the `doctor` checklist. Repo conventions: beads (`bd`) if initialised, `cargo fmt`/`clippy` clean, tests green before each commit.
