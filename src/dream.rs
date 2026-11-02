//! The dream pipeline: gate → snapshot/clone → reduce → workers →
//! aggregator → post-check → diff. State is persisted to SQLite after each
//! stage so the CLI, the API server and the nightly job see the same dreams.
//!
//! Dream dir layout (`<data_dir>/dreams/<id>/`): `in/` (snapshot),
//! `in.sha256` (hash of `in/`), `out/` (the clone the aggregator edits,
//! stored as `Dream::output_dir`), `findings/<session>.json`,
//! `aggregate.md`, `dream.diff`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use chrono::Utc;

use crate::aggregate::{aggregator_prompt, extract_summary, run_aggregator};
use crate::claude::{Claude, CliClaude};
use crate::config::Config;
use crate::db::Db;
use crate::discover::{candidate_sessions, gate, DiscoveredProject, Gate};
use crate::fsops::{copy_dir, hash_dir, unified_diff};
use crate::model::{Dream, DreamStatus, SessionFile, SessionRecord, SessionStatus, Usage};
use crate::notify::notify;
use crate::postcheck::{check, check_escapes, snapshot_mtimes};
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

    /// `from_env` with an already-loaded config.
    pub fn new(cfg: Config) -> anyhow::Result<Ctx> {
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
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // one per dream run; plan interface is `Done(Dream)`
pub enum DreamOutcome {
    /// Nothing was run (gate verdict, no sessions, or dry run); the reason.
    Skipped(String),
    /// A dream was created; its final state (completed, failed or canceled).
    Done(Dream),
}

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

/// Run one dream end to end. Errors after the dream row exists are recorded
/// as `failed` (output kept) and returned as `Done`; only errors before that
/// (or database failures while recording) are `Err`.
pub async fn run_dream(
    ctx: &Ctx,
    d: DiscoveredProject,
    opts: DreamOpts,
) -> anyhow::Result<DreamOutcome> {
    let sessions = match select_sessions(ctx, &d, &opts) {
        Ok(s) => s,
        Err(why) => return Ok(DreamOutcome::Skipped(why)),
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
        return Ok(DreamOutcome::Skipped("dry run".into()));
    }

    let id = Dream::new_id();
    let dir = dream_dir(&ctx.cfg, &id);
    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| ctx.cfg.aggregator.model.clone());
    let dream = Dream {
        id: id.clone(),
        project: d.project.clone(),
        status: DreamStatus::Pending,
        model: model.clone(),
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
    Ok(DreamOutcome::Done(done))
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

    // 1. Snapshot + clone.
    copy_dir(memory_dir, &in_dir)
        .with_context(|| format!("snapshot {} → in/", memory_dir.display()))?;
    copy_dir(memory_dir, &out_dir)
        .with_context(|| format!("clone {} → out/", memory_dir.display()))?;
    let in_hash = hash_dir(&in_dir).context("hash in/")?;
    std::fs::write(dir.join("in.sha256"), format!("{in_hash}\n")).context("write in.sha256")?;
    db.update_status(id, DreamStatus::Running, None)?;
    db.set_pid(id, Some(std::process::id()))?;

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

    // 4. Aggregate. The "before" snapshot is taken after findings exist.
    let before = snapshot_mtimes(dir);
    let findings: Vec<PathBuf> = outcomes
        .iter()
        .filter_map(|o| o.record.findings_path.clone())
        .collect();
    let cwd = dream.project.cwd.as_deref().filter(|p| p.is_dir());
    let claude_md = cwd.map(|c| c.join("CLAUDE.md")).filter(|p| p.is_file());
    let prompt = aggregator_prompt(
        &dream.project,
        claude_md.as_deref(),
        &findings,
        opts.instructions.as_deref(),
    );
    let mut agg_cfg = ctx.cfg.aggregator.clone();
    agg_cfg.model = model.to_string();
    let result = run_aggregator(ctx.claude.as_ref(), &agg_cfg, &out_dir, prompt, cwd)
        .await
        .map_err(|e| anyhow::anyhow!("aggregator: {e}"))?;
    *usage += result.usage;
    if canceled(ctx, id)? {
        return Ok(Stop::Canceled);
    }

    // 5. Post-check, then write artifacts (after check_escapes, so they do
    //    not register as escapes).
    let after = snapshot_mtimes(dir);
    let mut violations = check_escapes(dir, &before, &after);
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

/// Number of sessions that mined successfully.
pub fn sessions_mined(db: &Db, id: &str) -> anyhow::Result<usize> {
    Ok(db
        .list_sessions(id)?
        .iter()
        .filter(|s| s.status == SessionStatus::Ok)
        .count())
}
