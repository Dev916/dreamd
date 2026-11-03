# dreamd: where it came from and how it works

dreamd is a local, out-of-band memory consolidator for Claude Code. It was built on 2026-11-02 after
tracing three sources: the dormant auto-dream code inside the Claude Code binary, Anthropic's hosted
Dreams API for Managed Agents, and an Anthropic talk on memory systems. This document frames the design
against those sources and explains each moving part with a diagram.

## 1. Three sources, one shape

```mermaid
flowchart LR
    subgraph sources["What we traced"]
        A["Claude Code auto-dream<br/>(found in the 2.1.3xx binary,<br/>gated off by a server flag)"]
        B["Managed Agents Dreams API<br/>POST /v1/dreams<br/>(research preview)"]
        C["AI DevCon talk, Anthropic applied AI:<br/>'memory' in-band, 'dreaming' out-of-band"]
    end
    D["dreamd"]
    A -- "gate: 24 h and ≥3 sessions,<br/>pid lock file, four-phase prompt,<br/>read-only tools + delete" --> D
    B -- "inputs = one memory store + sessions,<br/>output = a NEW store, never the input;<br/>statuses, cancel, archive" --> D
    C -- "per-session sub-agents feeding an<br/>orchestrator that rewrites a clone;<br/>versioning, hashing, review before accept" --> D
```

**From the binary.** Claude Code already ships a `DreamTask`. It fires after a turn when at least
24 hours and a few sessions have passed, takes a `.consolidate-lock` whose mtime is the last run and whose
content is the owning pid, forks a subagent with a read-only shell plus permission to delete memory
`.md` files, and walks four phases: skim, merge duplicates, prune stale, reconcile against CLAUDE.md
without editing it. It is switched off for this account by a feature flag, which is the whole reason
dreamd exists. dreamd keeps the gate, the lock semantics and the four phases almost verbatim.

**From the hosted API.** A hosted dream takes one memory store and 1 to 100 session transcripts and
writes a *new* store; the input is never modified, the caller reviews and then attaches or discards.
dreamd copies that contract exactly: `in/` is a snapshot, `out/` is the clone the aggregator edits,
and `apply` is a separate, explicit, reversible step. The status set (pending, running, completed,
failed, canceled) and the cancel and archive verbs are the same words.

**From the talk.** The speaker split memory into an in-band process (the agent writes notes while
working) and an out-of-band "dreaming" pass with its own token budget and fleet-wide visibility, run as
an orchestrator spawning one sub-agent per transcript, with the production guardrails of versioning,
content hashing before writes, and human acceptance of proposed changes. That is dreamd's fan-out and
fan-in, the hash guard on apply, and the review loop.

## 2. The pipeline

```mermaid
flowchart TD
    S0["0. Discover<br/>config dirs → projects with memory/<br/>candidate sessions: ≥ 8 KB not yet mined<br/>(grown past their mark, or new since the cutoff),<br/>older than 10 min"]
    S1["1. Snapshot + clone<br/>memory/ → in/ (hashed) and out/<br/>CLAUDE.md → context/"]
    S2["2. Reduce (Rust, no tokens)<br/>JSONL → digest: user/assistant text,<br/>tool names, tool errors; images dropped;<br/>a marked session: only records past its mark,<br/>after 6 records of earlier context"]
    S3["3. Fan-out workers<br/>one claude -p per session,<br/>plan mode, JSON-schema findings<br/>with evidence quotes"]
    S4["4. Aggregate<br/>one claude -p in out/,<br/>acceptEdits, scoped Write/Edit,<br/>four phases + findings"]
    S5["5. Post-check<br/>frontmatter, index links,<br/>no writes outside out/,<br/>live dir hash unchanged,<br/>permission mode asserted"]
    S6["6. Report<br/>dream.diff, aggregate.md,<br/>notification"]
    S0 --> S1 --> S2 --> S3 --> S4 --> S5 --> S6
    S3 -. "no findings at all:<br/>aggregator skipped, empty diff" .-> S6
    S5 -. "any violation" .-> F["status: failed<br/>(out/ kept for inspection)"]
```

The only steps that spend tokens are 3 and 4. Everything else is Rust, which is what keeps a nightly
run cheap and deterministic. Reading is incremental: a completed dream records, per session whose worker
succeeded, the byte offset and record count its digest consumed (`session_marks`), so a long-lived
session that keeps growing is read from that mark (the new records, plus up to 6 earlier user/assistant
text records marked "already consolidated") instead of from byte 0, and a session that did not grow is
not read at all. When no worker found anything, step 4 is skipped and the dream completes with an empty
diff. Workers see a digest, not a raw transcript, and must cite a session id and
a quote for every insight; the aggregator is told to add a memory only when a findings file backs it.

## 3. The review loop and dream lifecycle

```mermaid
stateDiagram-v2
    [*] --> pending: prepare (lock taken, row written)
    pending --> running: execute
    running --> completed: post-check passes
    running --> failed: worker/aggregator/post-check error
    running --> canceled: cancel
    pending --> canceled: cancel
    completed --> applied: apply (hash guard, backup, rename swap)
    applied --> rolled_back: rollback (from backup)
    completed --> completed: archive (archived_at set, project unparked)
    failed --> failed: archive
    canceled --> canceled: archive
    applied --> applied: archive
    rolled_back --> rolled_back: archive
```

A `completed` dream parks its project as *awaiting review* until it is applied, rolled back or
archived. Completion also advances the project's mining cutoff, so a nightly run never re-mines the
same sessions, and `--force` is the deliberate way around both.

## 4. Who may write where

```mermaid
flowchart LR
    subgraph host["Your machine"]
        LIVE["live memory dir<br/>(read once, hashed,<br/>never written by a model)"]
        subgraph dream["<data_dir>/dreams/&lt;id&gt;/"]
            IN["in/ (snapshot)"]
            CTX["context/CLAUDE.md"]
            FND["findings/*.json"]
            OUT["out/ (the clone)"]
        end
        PROJ["project cwd<br/>(not mounted)"]
    end
    W["workers<br/>(plan mode, no tools)"]
    AG["aggregator<br/>(acceptEdits,<br/>Write(./**) Edit(./**),<br/>cwd = out/)"]
    LIVE -- copy --> IN
    LIVE -- copy --> OUT
    W -- "read digest in prompt" --> FND
    FND -. read .-> AG
    CTX -. read .-> AG
    AG == "write / edit / manifest delete" ==> OUT
    AG x-. "denied: outside cwd" .-x IN
    AG x-. "denied: not mounted" .-x PROJ
    OUT -- "apply: backup, then rename swap" --> LIVE
```

The first version used `bypassPermissions` and mounted the project directory. A live probe showed the
aggregator could then write anywhere, so containment moved to the front: `acceptEdits` with a scoped
allowlist, no Bash, deletions through a validated manifest, CLAUDE.md copied in rather than mounted,
and a runtime assertion that the session really ran in that mode. The account-rotating `claude`
wrapper injects `--dangerously-skip-permissions`, so dreamd refuses it and runs the real binary.

## 5. The nightly gate and caps

```mermaid
flowchart TD
    N["03:30 launchd: dreamd dream --all"]
    N --> SW["sweep orphans<br/>(running rows whose lock pid is dead → failed)"]
    SW --> P["per project: candidates with ≥ 8 KB unmined:<br/>marked sessions grown past their mark,<br/>unmarked ones newer than the cutoff<br/>(14-day backfill if never dreamed)"]
    P --> G1{"awaiting review?"}
    G1 -- yes --> SK1["skip"]
    G1 -- no --> G2{"≥ 24 h since cutoff<br/>and ≥ 3 sessions?"}
    G2 -- no --> SK2["skip"]
    G2 -- yes --> PLAN["plan: most active projects first,<br/>≤ 5 projects, ≤ 10 sessions each,<br/>≤ 30 sessions per run"]
    PLAN --> RUN["run plan, one project at a time,<br/>under its lock"]
    PLAN --> DEF["deferred: run budget exhausted<br/>(picked up another night)"]
    RUN --> NOTE["notify: dream ready for review"]
```

Measured on this machine, an uncapped first run would have mined 381 sessions. Under the defaults it
mines 30 and touches the 3 or 4 most active projects, and because completion advances the cutoff and
parks the project, the following nights only see new work: a session is a candidate again only when it
grew by at least `min_transcript_bytes` past its mark, and then only that growth is read.

## 6. What was deliberately left out

Team memory, editing CLAUDE.md, auto-apply by default, a web UI, and dreaming hosted memory stores.
Each is a real feature in one of the sources; none is needed to close the loop locally, and the first
three carry risk that the review step is there to prevent.
