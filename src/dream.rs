//! The dream pipeline: gate → snapshot/clone → reduce → workers →
//! aggregator → post-check → diff. State is persisted to SQLite after each
//! stage so the CLI, the API server and the nightly job see the same dreams.
//!
//! Dream dir layout (`<data_dir>/dreams/<id>/`): `in/` (snapshot),
//! `in.sha256` (hash of `in/`), `out/` (the clone the aggregator edits,
//! stored as `Dream::output_dir`), `findings/<session>.json`,
//! `context/` (read-only copies of the project's CLAUDE.md / AGENTS.md for
//! the aggregator), `aggregate.md`, `dream.diff`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use chrono::Utc;

use crate::aggregate::{
    aggregator_prompt, extract_summary, run_aggregator, AGGREGATOR_PERMISSION_MODE,
};
use crate::claude::{Claude, CliClaude};
use crate::config::Config;
use crate::db::Db;
use crate::discover::{candidate_sessions, gate, DiscoveredProject, Gate};
use crate::fsops::{copy_dir, hash_dir, unified_diff};
use crate::lock::{LockError, ProjectLock};
use crate::model::{Dream, DreamStatus, SessionFile, SessionRecord, SessionStatus, Usage};
use crate::notify::notify;
use crate::postcheck::{apply_delete_manifest, check, check_escapes, snapshot_mtimes};
use crate::{transcript, worker};

/// Shared state for the CLI and the API server.
#[derive(Clone)]
pub struct Ctx {
    pub cfg: Config,
    pub db: Arc<Db>,
    pub claude: Arc<dyn Claude>,
}

impl Ctx {
    /// Load config (env overrides included, so `DREAMD_CLAUDE_BIN` wins over
    /// `claude_bin`), open the database, and use the CLI runner.
    pub fn from_env() -> anyhow::Result<Ctx> {
        let cfg = crate::config::load()?;
        Ctx::new(cfg)
    }

    /// `from_env` with an already-loaded config. Refuses a `claude_bin` that
    /// is a wrapper injecting `--dangerously-skip-permissions` unless
    /// `DREAMD_ALLOW_BYPASS_WRAPPER=1`.
    pub fn new(cfg: Config) -> anyhow::Result<Ctx> {
        if std::env::var("DREAMD_ALLOW_BYPASS_WRAPPER").as_deref() != Ok("1") {
            if let Some(p) = crate::claude::detect_bypass_wrapper(&cfg.claude_bin) {
                anyhow::bail!(crate::claude::bypass_wrapper_error(&p));
            }
        }
        let db = Arc::new(Db::open(&cfg.db_path())?);
        let claude: Arc<dyn Claude> = Arc::new(CliClaude {
            bin: cfg.claude_bin.clone(),
        });
        Ok(Ctx { cfg, db, claude })
    }
}

/// Per-dream options (CLI flags / API body).
#[derive(Debug, Clone, Default)]
pub struct DreamOpts {
    pub force: bool,
    /// Aggregator model override.
    pub model: Option<String>,
    pub instructions: Option<String>,
    /// Explicit sessions: bypasses the gate and restricts candidates.
    pub session_ids: Option<Vec<String>>,
    pub dry_run: bool,
    /// Do not record this process's pid on the dream (the API server sets
    /// this so a CLI `cancel` never signals the server).
    pub no_pid: bool,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // one per dream run; plan interface is `Done(Dream)`
pub enum DreamOutcome {
    /// Nothing was run (gate verdict, no sessions, or dry run); the reason.
    Skipped(String),
    /// A dream was created; its final state (completed, failed or canceled).
    Done(Dream),
    /// Another live process is dreaming this project (the lock error text).
    Busy(String),
}

/// A `Pending` dream inserted by [`prepare`], with the project lock that
/// [`execute`] holds until the dream reaches a terminal status.
#[derive(Debug)]
pub struct PreparedDream {
    pub dream: Dream,
    pub lock: ProjectLock,
}

/// What [`prepare`] produced: a dream ready to [`execute`], or the outcome
/// that ends the request here (`Skipped` or `Busy`).
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Prepared {
    Ready(PreparedDream),
    Stop(DreamOutcome),
}

/// The error recorded on a dream whose owning process died.
pub const ORPHANED: &str = "orphaned: owning process died";

/// `<dreams_dir>/<id>`.
pub fn dream_dir(cfg: &Config, id: &str) -> PathBuf {
    cfg.dreams_dir().join(id)
}

/// The dream dir of a stored dream (`output_dir` is `<dream_dir>/out`).
pub fn dir_of(d: &Dream) -> PathBuf {
    d.output_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| d.output_dir.clone())
}

/// Pick the sessions to mine, or the reason to skip.
fn select_sessions(
    ctx: &Ctx,
    d: &DiscoveredProject,
    opts: &DreamOpts,
) -> Result<Vec<SessionFile>, String> {
    let sessions = match &opts.session_ids {
        Some(ids) => {
            let project_dir = d.project.config_dir.join("projects").join(&d.project.slug);
            let all = candidate_sessions(&project_dir, None, Utc::now());
            let missing: Vec<&str> = ids
                .iter()
                .filter(|id| !all.iter().any(|s| &s.id == *id))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "unknown or live session id(s): {}",
                    missing.join(", ")
                ));
            }
            all.into_iter().filter(|s| ids.contains(&s.id)).collect()
        }
        None => {
            if let Gate::Skip(why) = gate(d, &ctx.cfg, opts.force) {
                return Err(why);
            }
            d.candidate_sessions.clone()
        }
    };
    if sessions.is_empty() {
        return Err("no candidate sessions".into());
    }
    Ok(sessions)
}

fn canceled(ctx: &Ctx, id: &str) -> anyhow::Result<bool> {
    Ok(ctx
        .db
        .get_dream(id)?
        .is_some_and(|d| d.status == DreamStatus::Canceled))
}

/// Why the pipeline stopped early without error.
enum Stop {
    Completed,
    Canceled,
}

/// Run one dream end to end: [`prepare`] then [`execute`]. Errors after the
/// dream row exists are recorded as `failed` (output kept) and returned as
/// `Done`; only errors before that (or database failures while recording)
/// are `Err`.
pub async fn run_dream(
    ctx: &Ctx,
    d: DiscoveredProject,
    opts: DreamOpts,
) -> anyhow::Result<DreamOutcome> {
    match prepare(ctx, &d, &opts)? {
        Prepared::Ready(p) => Ok(DreamOutcome::Done(execute(ctx, p, opts).await?)),
        Prepared::Stop(outcome) => Ok(outcome),
    }
}

/// Gate, select sessions, take the project lock and insert the `Pending`
/// dream row. `Skipped` when the gate says no, no sessions qualify, or
/// `opts.dry_run` (which prints the plan); `Busy` when another live process
/// holds the project's lock. Fast and synchronous, so the API can answer 202
/// with the row.
pub fn prepare(ctx: &Ctx, d: &DiscoveredProject, opts: &DreamOpts) -> anyhow::Result<Prepared> {
    let sessions = match select_sessions(ctx, d, opts) {
        Ok(s) => s,
        Err(why) => return Ok(Prepared::Stop(DreamOutcome::Skipped(why))),
    };
    if opts.dry_run {
        println!(
            "dry run: would dream {} ({} sessions)",
            d.project.slug,
            sessions.len()
        );
        for s in &sessions {
            match transcript::reduce(&s.path, ctx.cfg.worker.max_transcript_bytes) {
                Ok(dg) => println!(
                    "  {}  {} → {} bytes{}",
                    s.id,
                    dg.bytes_in,
                    dg.bytes_out,
                    if dg.elided { " (elided)" } else { "" }
                ),
                Err(e) => println!("  {}  unreadable: {e:#}", s.id),
            }
        }
        return Ok(Prepared::Stop(DreamOutcome::Skipped("dry run".into())));
    }

    // Lock before the row exists, so a pending/running row always has a
    // lock (the orphan sweep relies on it).
    let lock = match ProjectLock::acquire(&ctx.cfg, &d.project) {
        Ok(l) => l,
        Err(e @ LockError::Held { .. }) => {
            return Ok(Prepared::Stop(DreamOutcome::Busy(e.to_string())))
        }
        Err(e) => return Err(e.into()),
    };

    let id = Dream::new_id();
    let dir = dream_dir(&ctx.cfg, &id);
    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| ctx.cfg.aggregator.model.clone());
    let dream = Dream {
        id,
        project: d.project.clone(),
        status: DreamStatus::Pending,
        model,
        instructions: opts.instructions.clone(),
        session_ids: sessions.iter().map(|s| s.id.clone()).collect(),
        created_at: Utc::now(),
        ended_at: None,
        usage: Usage::default(),
        summary: None,
        error: None,
        output_dir: dir.join("out"),
        diff_stats: None,
        pid: None,
    };
    ctx.db.upsert_project(&d.project)?;
    ctx.db.insert_dream(&dream)?;
    Ok(Prepared::Ready(PreparedDream { dream, lock }))
}

/// Run the pipeline for a dream inserted by [`prepare`] and return its final
/// state (completed, failed or canceled). Session transcripts are the
/// dream's `session_ids` under `<config_dir>/projects/<slug>/`. The project
/// lock is released when this returns.
pub async fn execute(ctx: &Ctx, prepared: PreparedDream, opts: DreamOpts) -> anyhow::Result<Dream> {
    let PreparedDream { dream, lock: _lock } = prepared;
    let id = dream.id.clone();
    let dir = dream_dir(&ctx.cfg, &id);
    let project_dir = dream
        .project
        .config_dir
        .join("projects")
        .join(&dream.project.slug);
    let sessions: Vec<SessionFile> = dream
        .session_ids
        .iter()
        .map(|sid| {
            let path = project_dir.join(format!("{sid}.jsonl"));
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map(chrono::DateTime::<Utc>::from)
                .unwrap_or_else(|_| Utc::now());
            SessionFile {
                id: sid.clone(),
                path,
                mtime,
            }
        })
        .collect();
    let model = dream.model.clone();

    let mut usage = Usage::default();
    let res = pipeline(ctx, &dream, &dir, &sessions, &opts, &model, &mut usage).await;
    let _ = ctx.db.set_pid(&id, None);
    match res {
        Ok(Stop::Completed) => {
            ctx.db.update_status(&id, DreamStatus::Completed, None)?;
        }
        Ok(Stop::Canceled) => {}
        Err(e) => {
            let msg = format!("{e:#}");
            // Keep the usage spent so far; leave summary/diff unset.
            let _ = ctx.db.set_usage(&id, &usage);
            if !canceled(ctx, &id)? {
                ctx.db.update_status(&id, DreamStatus::Failed, Some(&msg))?;
            }
        }
    }
    let done = ctx
        .db
        .get_dream(&id)?
        .with_context(|| format!("dream vanished: {id}"))?;
    match done.status {
        DreamStatus::Completed => notify(
            "dreamd: dream ready",
            &format!("{} — review with `dreamd diff {id}`", done.project.slug),
        ),
        DreamStatus::Failed => notify(
            "dreamd: dream failed",
            &format!(
                "{}: {}",
                done.project.slug,
                done.error.as_deref().unwrap_or("")
            ),
        ),
        _ => {}
    }
    Ok(done)
}

async fn pipeline(
    ctx: &Ctx,
    dream: &Dream,
    dir: &Path,
    sessions: &[SessionFile],
    opts: &DreamOpts,
    model: &str,
    usage: &mut Usage,
) -> anyhow::Result<Stop> {
    let id = dream.id.as_str();
    let db = &ctx.db;
    let in_dir = dir.join("in");
    let out_dir = dir.join("out");
    let memory_dir = &dream.project.memory_dir;

    // Canceled while still pending (e.g. via the API before the task ran).
    if canceled(ctx, id)? {
        return Ok(Stop::Canceled);
    }

    // 1. Snapshot + clone.
    copy_dir(memory_dir, &in_dir)
        .with_context(|| format!("snapshot {} → in/", memory_dir.display()))?;
    copy_dir(memory_dir, &out_dir)
        .with_context(|| format!("clone {} → out/", memory_dir.display()))?;
    let in_hash = hash_dir(&in_dir).context("hash in/")?;
    std::fs::write(dir.join("in.sha256"), format!("{in_hash}\n")).context("write in.sha256")?;
    db.update_status(id, DreamStatus::Running, None)?;
    // A server-run dream records no pid: a CLI `cancel` must not SIGTERM
    // the API server; it cancels through the status column instead.
    db.set_pid(id, (!opts.no_pid).then(std::process::id))?;

    // 2. Reduce transcripts.
    let mut digests = Vec::new();
    let mut paths = Vec::new();
    let mut unreadable = Vec::new();
    for s in sessions {
        match transcript::reduce(&s.path, ctx.cfg.worker.max_transcript_bytes) {
            Ok(dg) => {
                db.insert_session(id, &pending_record(&s.id, &s.path))?;
                paths.push(s.path.clone());
                digests.push(dg);
            }
            Err(e) => {
                let mut r = pending_record(&s.id, &s.path);
                r.status = SessionStatus::Failed;
                r.error = Some(format!("{e:#}"));
                db.insert_session(id, &r)?;
                unreadable.push(r);
            }
        }
    }
    if canceled(ctx, id)? {
        return Ok(Stop::Canceled);
    }

    // 3. Fan-out workers.
    let memory_index = std::fs::read_to_string(in_dir.join("MEMORY.md")).unwrap_or_default();
    let mut outcomes = worker::mine_sessions(
        Arc::clone(&ctx.claude),
        &ctx.cfg.worker,
        memory_index,
        digests,
        &dir.join("findings"),
    )
    .await;
    for (o, p) in outcomes.iter_mut().zip(paths) {
        o.record.transcript_path = p;
        *usage += o.record.usage;
        db.update_session(id, &o.record)?;
    }
    let ok = outcomes
        .iter()
        .filter(|o| o.record.status == SessionStatus::Ok)
        .count();
    if ok == 0 {
        anyhow::bail!(
            "all workers failed ({} sessions)",
            outcomes.len() + unreadable.len()
        );
    }
    if canceled(ctx, id)? {
        return Ok(Stop::Canceled);
    }

    // 4. Aggregate. Project context is copied into the dream dir first (the
    //    aggregator gets no access to the project tree), then the "before"
    //    snapshot is taken (findings and context exist by then) and the live
    //    memory dir is hashed.
    let cwd = dream.project.cwd.as_deref().filter(|p| p.is_dir());
    let context_files = copy_context(cwd, &dir.join("context")).context("copy project context")?;
    let before = snapshot_mtimes(dir);
    let live_before = hash_dir(memory_dir).context("hash live memory dir")?;
    let findings: Vec<PathBuf> = outcomes
        .iter()
        .filter_map(|o| o.record.findings_path.clone())
        .collect();
    let prompt = aggregator_prompt(
        &dream.project,
        &context_files,
        &findings,
        opts.instructions.as_deref(),
    );
    let mut agg_cfg = ctx.cfg.aggregator.clone();
    agg_cfg.model = model.to_string();
    let result = run_aggregator(ctx.claude.as_ref(), &agg_cfg, &out_dir, prompt)
        .await
        .map_err(|e| anyhow::anyhow!("aggregator: {e}"))?;
    *usage += result.usage;
    if let Some(mode) = result
        .permission_mode
        .as_deref()
        .filter(|m| *m != AGGREGATOR_PERMISSION_MODE)
    {
        anyhow::bail!(
            "aggregator ran with permission mode {mode}, expected {AGGREGATOR_PERMISSION_MODE} (is claude_bin a wrapper that adds --dangerously-skip-permissions?)"
        );
    }
    if canceled(ctx, id)? {
        return Ok(Stop::Canceled);
    }

    // 5. Post-check (fail closed), then write artifacts (after check_escapes,
    //    so they do not register as escapes).
    let live_after = hash_dir(memory_dir).context("re-hash live memory dir")?;
    if live_after != live_before {
        anyhow::bail!(
            "live memory dir changed during aggregation: {}",
            memory_dir.display()
        );
    }
    let after = snapshot_mtimes(dir);
    let mut violations = check_escapes(dir, &before, &after);
    violations.extend(apply_delete_manifest(&out_dir));
    violations.extend(check(&in_dir, &out_dir));
    if !violations.is_empty() {
        let list: Vec<String> = violations.iter().map(|v| v.to_string()).collect();
        anyhow::bail!(
            "post-check failed: {} violation(s): {}",
            violations.len(),
            list.join("; ")
        );
    }
    let (diff, stats) = unified_diff(&in_dir, &out_dir).context("diff in/ → out/")?;
    std::fs::write(dir.join("dream.diff"), &diff).context("write dream.diff")?;
    let summary = extract_summary(&result.text);
    std::fs::write(dir.join("aggregate.md"), format!("{summary}\n"))
        .context("write aggregate.md")?;
    db.set_result(id, usage, &summary, &stats)?;
    if canceled(ctx, id)? {
        return Ok(Stop::Canceled);
    }
    Ok(Stop::Completed)
}

/// Copy the project's CLAUDE.md (`<cwd>/CLAUDE.md`, else
/// `<cwd>/.claude/CLAUDE.md`) and `<cwd>/AGENTS.md` into `context_dir`;
/// returns the names copied (`CLAUDE.md` first). Symlinks are followed only
/// when they resolve to a regular file.
pub fn copy_context(cwd: Option<&Path>, context_dir: &Path) -> std::io::Result<Vec<&'static str>> {
    let Some(cwd) = cwd else {
        return Ok(Vec::new());
    };
    let sources: [(&'static str, Vec<PathBuf>); 2] = [
        (
            "CLAUDE.md",
            vec![cwd.join("CLAUDE.md"), cwd.join(".claude").join("CLAUDE.md")],
        ),
        ("AGENTS.md", vec![cwd.join("AGENTS.md")]),
    ];
    let mut copied = Vec::new();
    for (name, candidates) in sources {
        if let Some(src) = candidates.into_iter().find(|p| p.is_file()) {
            std::fs::create_dir_all(context_dir)?;
            std::fs::copy(&src, context_dir.join(name))?;
            copied.push(name);
        }
    }
    Ok(copied)
}

fn pending_record(sid: &str, path: &Path) -> SessionRecord {
    SessionRecord {
        session_id: sid.to_string(),
        transcript_path: path.to_path_buf(),
        status: SessionStatus::Pending,
        findings_path: None,
        usage: Usage::default(),
        error: None,
        duration_ms: 0,
    }
}

/// Mark every `pending`/`running` dream whose project lock has no live
/// holder `failed` (`orphaned: owning process died`); returns their ids.
pub fn sweep_orphans(ctx: &Ctx) -> anyhow::Result<Vec<String>> {
    sweep_orphans_in(&ctx.cfg, &ctx.db)
}

/// [`sweep_orphans`] without a full `Ctx` (no claude binary needed).
pub fn sweep_orphans_in(cfg: &Config, db: &Db) -> anyhow::Result<Vec<String>> {
    let mut swept = Vec::new();
    for status in [DreamStatus::Pending, DreamStatus::Running] {
        for d in db.list_dreams(Some(status))? {
            if crate::lock::holder(cfg, &d.project).is_none()
                && db.fail_if_active(&d.id, ORPHANED)?
            {
                swept.push(d.id);
            }
        }
    }
    Ok(swept)
}

/// Number of sessions that mined successfully.
pub fn sessions_mined(db: &Db, id: &str) -> anyhow::Result<usize> {
    Ok(db
        .list_sessions(id)?
        .iter()
        .filter(|s| s.status == SessionStatus::Ok)
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_context_prefers_root_claude_md_then_dot_claude() {
        let t = tempfile::TempDir::new().unwrap();
        let cwd = t.path().join("proj");
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        std::fs::write(cwd.join(".claude/CLAUDE.md"), "dot\n").unwrap();
        let ctx = t.path().join("d/context");
        assert_eq!(copy_context(Some(&cwd), &ctx).unwrap(), vec!["CLAUDE.md"]);
        assert_eq!(
            std::fs::read_to_string(ctx.join("CLAUDE.md")).unwrap(),
            "dot\n"
        );
        std::fs::write(cwd.join("CLAUDE.md"), "root\n").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "agents\n").unwrap();
        assert_eq!(
            copy_context(Some(&cwd), &ctx).unwrap(),
            vec!["CLAUDE.md", "AGENTS.md"]
        );
        assert_eq!(
            std::fs::read_to_string(ctx.join("CLAUDE.md")).unwrap(),
            "root\n"
        );
        assert!(ctx.join("AGENTS.md").is_file());
    }

    #[test]
    fn copy_context_without_files_creates_nothing() {
        let t = tempfile::TempDir::new().unwrap();
        let ctx = t.path().join("context");
        assert!(copy_context(Some(t.path()), &ctx).unwrap().is_empty());
        assert!(copy_context(None, &ctx).unwrap().is_empty());
        assert!(!ctx.exists());
    }
}
