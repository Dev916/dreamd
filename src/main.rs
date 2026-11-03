use dreamd::{
    api, apply, cli, config, discover, doctor, dream, fsops, model, postcheck, schedule, token,
    transcript, worker,
};

use clap::Parser;

use cli::{
    Cli, Command, ConfigAction, ScheduleAction, EXIT_FAILED, EXIT_OK, EXIT_REFUSED, EXIT_SKIPPED,
};
use dreamd::claude::{Claude, ClaudeRequest, CliClaude};
use dreamd::db::Db;
use dreamd::model::{Dream, DreamStatus};

fn main() {
    // Rust ignores SIGPIPE, so `dreamd diff <id> | head` would panic on EPIPE.
    // Restore the default so a closed pipe just ends the process quietly.
    #[cfg(unix)]
    // SAFETY: resetting a signal disposition before any thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("DREAMD_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("dreamd: tokio runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("dreamd: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

/// Dispatch a parsed command. `Ok(code)` is the process exit code
/// (see `cli::EXIT_*`); `Err` maps to exit 1.
async fn run(cli: Cli) -> anyhow::Result<i32> {
    let cfg = config::load()?;
    match cli.command {
        Command::Doctor => {
            let checks = doctor::run(&cfg);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                print!("{}", doctor::render(&checks));
            }
            Ok(if checks.iter().all(|c| c.ok) {
                EXIT_OK
            } else {
                1
            })
        }
        Command::Config {
            action: ConfigAction::Show,
        } => {
            print!("{}", cfg.to_toml()?);
            Ok(EXIT_OK)
        }
        Command::List { include_archived } => {
            let db = Db::open(&cfg.db_path())?;
            sweep(&cfg, &db)?;
            let dreams = db.list_dreams(None, include_archived)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&dreams)?);
            } else if dreams.is_empty() {
                println!("no dreams");
            } else {
                for d in &dreams {
                    let mut line = format!(
                        "{}  {:<11}  {}  {}  {}",
                        d.id,
                        d.status,
                        d.created_at.format("%Y-%m-%d %H:%M"),
                        d.project.slug,
                        d.model
                    );
                    if include_archived && d.archived_at.is_some() {
                        line.push_str("  archived");
                    }
                    println!("{line}");
                }
            }
            Ok(EXIT_OK)
        }
        Command::Show { id } => {
            let db = Db::open(&cfg.db_path())?;
            let Some(d) = db.get_dream(&id)? else {
                eprintln!("dream not found: {id}");
                return Ok(EXIT_REFUSED);
            };
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                print!("{}", render_dream(&d));
                let sessions = db.list_sessions(&d.id)?;
                if !sessions.is_empty() {
                    println!("\nsessions:");
                    for r in &sessions {
                        match &r.error {
                            Some(e) => println!("  {}  {}  {e}", r.session_id, r.status),
                            None => println!(
                                "  {}  {}  {}ms  cost_usd={:.4}",
                                r.session_id, r.status, r.duration_ms, r.usage.cost_usd
                            ),
                        }
                    }
                }
            }
            Ok(EXIT_OK)
        }
        Command::Dream {
            selector,
            all,
            force,
            model,
            instructions,
            sessions,
            dry_run,
            since_days,
            max_sessions,
        } => {
            let opts = dream::DreamOpts {
                force,
                model,
                instructions,
                session_ids: (!sessions.is_empty()).then_some(sessions),
                max_sessions,
                dry_run,
                no_pid: false,
            };
            dream_cmd(selector.as_deref(), all, opts, since_days).await
        }
        Command::Diff { id } => artifact_cmd(&cfg, &id, "dream.diff"),
        Command::Summary { id } => artifact_cmd(&cfg, &id, "aggregate.md"),
        Command::Cancel { id } => cancel_cmd(&cfg, &id),
        Command::Apply { id, force } => apply_cmd(&id, Some(force)),
        Command::Rollback { id } => apply_cmd(&id, None),
        Command::Archive { id } => archive_cmd(&cfg, &id),
        Command::Scan { all_dirs } => {
            let db = Db::open(&cfg.db_path())?;
            sweep(&cfg, &db)?;
            let found = discover::discover(&cfg, &db, chrono::Utc::now())?;
            let rows: Vec<discover::ScanRow> = found
                .iter()
                .map(|d| discover::ScanRow::new(d, &cfg))
                .collect();
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(EXIT_OK);
            }
            if all_dirs {
                for dir in &cfg.config_dirs {
                    let n = rows.iter().filter(|r| &r.config_dir == dir).count();
                    println!("# {}: {n} dreamable project(s)", dir.display());
                }
            }
            if rows.is_empty() {
                println!("no dreamable projects");
            }
            for r in &rows {
                println!("{}", r.render());
            }
            let plan =
                discover::plan_run(found, &cfg, false, discover::RunCaps::for_all(&cfg, None));
            let l = &cfg.limits;
            println!(
                "next --all run: {} projects, {} worker calls (caps: {}/dream, {}/run, {} projects/run, {}d backfill, min {} bytes)",
                plan.projects(),
                plan.sessions(),
                l.max_sessions_per_dream,
                l.max_sessions_per_run,
                l.max_projects_per_run,
                l.backfill_days,
                l.min_transcript_bytes
            );
            Ok(EXIT_OK)
        }
        Command::Digest { path, max_bytes } => {
            let max = max_bytes.unwrap_or(cfg.worker.max_transcript_bytes);
            let d = transcript::reduce(&path, max)?;
            print!("{}", d.text);
            if d.skipped_lines > 0 {
                let s = if d.skipped_lines == 1 { "" } else { "s" };
                eprintln!("skipped {} malformed line{s}", d.skipped_lines);
            }
            Ok(EXIT_OK)
        }
        Command::ClaudeProbe {
            prompt,
            model,
            timeout,
        } => {
            let claude = CliClaude {
                // config::load already applied DREAMD_CLAUDE_BIN.
                bin: cfg.claude_bin.clone(),
            };
            let req = ClaudeRequest::new(
                prompt,
                model.as_deref().unwrap_or(&cfg.worker.model),
                &cfg.worker.effort,
                std::time::Duration::from_secs(timeout.unwrap_or(cfg.worker.timeout_secs)),
            );
            match claude.run(req).await {
                Ok(r) => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "text": r.text,
                                "structured": r.structured,
                                "usage": r.usage,
                                "duration_ms": r.duration_ms,
                                "session_id": r.session_id,
                            }))?
                        );
                    } else {
                        println!("{}", r.text);
                        println!(
                            "usage: input_tokens={} output_tokens={} cost_usd={:.4} duration_ms={} session_id={}",
                            r.usage.input_tokens,
                            r.usage.output_tokens,
                            r.usage.cost_usd,
                            r.duration_ms,
                            r.session_id
                        );
                    }
                    Ok(EXIT_OK)
                }
                Err(e) => {
                    eprintln!("claude error: {e}");
                    Ok(EXIT_FAILED)
                }
            }
        }
        Command::Mine {
            dream_dir,
            transcripts,
            timeout,
        } => mine(&cfg, &dream_dir, transcripts, timeout).await,
        Command::Postcheck { in_dir, out_dir } => postcheck_cmd(&in_dir, &out_dir),
        Command::Serve { port } => {
            let port = port.unwrap_or(cfg.port);
            let ctx = dream::Ctx::new(cfg)?;
            api::serve(ctx, port).await?;
            Ok(EXIT_OK)
        }
        Command::Token => {
            println!("{}", token::ensure(&cfg.data_dir)?);
            Ok(EXIT_OK)
        }
        Command::Schedule { action } => {
            match action {
                ScheduleAction::Install { dry_run } => schedule::install(&cfg, dry_run)?,
                ScheduleAction::Uninstall => schedule::uninstall()?,
                ScheduleAction::Status => {
                    let st = schedule::status()?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&st)?);
                    } else if st.installed {
                        println!(
                            "installed: daily at {:02}:{:02} ({})",
                            st.hour,
                            st.minute,
                            schedule::plist_path().display()
                        );
                    } else {
                        println!("not installed");
                    }
                }
            }
            Ok(EXIT_OK)
        }
    }
}

/// `mine`: reduce each transcript, fan workers out into `<dream_dir>/findings/`,
/// print one line per session plus `N ok, M failed`. Exit 4 if every worker failed.
async fn mine(
    cfg: &config::Config,
    dream_dir: &std::path::Path,
    transcripts: Vec<std::path::PathBuf>,
    timeout: Option<u64>,
) -> anyhow::Result<i32> {
    let mut wcfg = cfg.worker.clone();
    if let Some(t) = timeout {
        wcfg.timeout_secs = t;
    }
    // The memory index the workers see: the dream's input snapshot, if any.
    let memory_index =
        std::fs::read_to_string(dream_dir.join("in").join("MEMORY.md")).unwrap_or_default();
    let mut digests = Vec::new();
    let mut paths = Vec::new();
    let mut unreadable = Vec::new();
    for t in transcripts {
        match transcript::reduce(&t, wcfg.max_transcript_bytes) {
            Ok(d) => {
                paths.push(t);
                digests.push(d);
            }
            Err(e) => unreadable.push((t, e)),
        }
    }
    let claude: std::sync::Arc<dyn Claude> = std::sync::Arc::new(CliClaude {
        bin: cfg.claude_bin.clone(),
    });
    let mut outcomes = worker::mine_sessions(
        claude,
        &wcfg,
        memory_index,
        digests,
        &dream_dir.join("findings"),
    )
    .await;
    for (o, p) in outcomes.iter_mut().zip(paths) {
        o.record.transcript_path = p;
    }
    for (t, e) in unreadable {
        outcomes.push(worker::SessionOutcome {
            record: model::SessionRecord {
                session_id: t
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                transcript_path: t,
                status: model::SessionStatus::Failed,
                findings_path: None,
                usage: model::Usage::default(),
                error: Some(format!("{e:#}")),
                duration_ms: 0,
            },
            findings: None,
        });
    }
    let ok = outcomes
        .iter()
        .filter(|o| o.record.status == model::SessionStatus::Ok)
        .count();
    for o in &outcomes {
        let r = &o.record;
        match &r.error {
            Some(e) => println!("{}  {}  {}ms  {e}", r.session_id, r.status, r.duration_ms),
            None => println!("{}  {}  {}ms", r.session_id, r.status, r.duration_ms),
        }
    }
    println!("{ok} ok, {} failed", outcomes.len() - ok);
    if worker::all_failed(&outcomes) {
        eprintln!("all workers failed");
        return Ok(EXIT_FAILED);
    }
    Ok(EXIT_OK)
}

/// `postcheck`: print `violation: …` per new violation (exit 4), else
/// `ok: N files, +a -r ~c` for the in → out diff.
fn postcheck_cmd(in_dir: &std::path::Path, out_dir: &std::path::Path) -> anyhow::Result<i32> {
    use anyhow::Context;
    for d in [in_dir, out_dir] {
        anyhow::ensure!(d.is_dir(), "not a directory: {}", d.display());
    }
    let violations = postcheck::check(in_dir, out_dir);
    if !violations.is_empty() {
        for v in &violations {
            println!("violation: {v}");
        }
        return Ok(EXIT_FAILED);
    }
    let (_, s) = fsops::unified_diff(in_dir, out_dir).context("diff in/ → out/")?;
    let n = walkdir::WalkDir::new(out_dir)
        .min_depth(1)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .count();
    println!(
        "ok: {n} files, +{} -{} ~{}",
        s.files_added, s.files_removed, s.files_changed
    );
    Ok(EXIT_OK)
}

/// `dream <selector>|--all`: plan the run (gate, order, caps), then run the
/// plan one project at a time, printing one line per project. With
/// `--dry-run`, print the plan and stop.
async fn dream_cmd(
    selector: Option<&str>,
    all: bool,
    mut opts: dream::DreamOpts,
    since_days: Option<u64>,
) -> anyhow::Result<i32> {
    let ctx = dream::Ctx::from_env()?;
    sweep(&ctx.cfg, &ctx.db)?;
    let found = discover::discover_with(
        &ctx.cfg,
        &ctx.db,
        chrono::Utc::now(),
        since_days,
        opts.force,
    )?;
    let targets = discover::select_projects(found, if all { None } else { selector });
    if targets.is_empty() {
        if all {
            println!("no dreamable projects");
            return Ok(EXIT_SKIPPED);
        }
        eprintln!("project not found: {}", selector.unwrap_or_default());
        return Ok(EXIT_REFUSED);
    }
    let explicit = opts.session_ids.clone();
    let plan = match &explicit {
        // Explicit sessions bypass the gate and every cap.
        Some(ids) => discover::RunPlan {
            items: targets
                .into_iter()
                .map(|project| discover::PlanItem::Run {
                    project,
                    take: ids.len(),
                })
                .collect(),
        },
        None => {
            let caps = if all {
                discover::RunCaps::for_all(&ctx.cfg, opts.max_sessions)
            } else {
                discover::RunCaps::for_one(&ctx.cfg, opts.max_sessions)
            };
            discover::plan_run(targets, &ctx.cfg, opts.force, caps)
        }
    };
    if opts.dry_run {
        return Ok(print_plan(&ctx, &plan, explicit.as_deref()));
    }

    let auto_apply = all && ctx.cfg.schedule.auto_apply;
    let (mut completed, mut failed, mut busy) = (0usize, 0usize, 0usize);
    for item in plan.items {
        let (d, take) = match item {
            discover::PlanItem::Run { project, take } => (project, take),
            discover::PlanItem::Deferred(d) => {
                println!(
                    "deferred {}: run budget exhausted ({} candidates)",
                    d.project.slug,
                    d.candidate_sessions.len()
                );
                continue;
            }
            discover::PlanItem::Skipped(d, why) => {
                println!("skipped {}: {why}", d.project.slug);
                continue;
            }
        };
        let slug = d.project.slug.clone();
        opts.max_sessions = Some(take);
        let outcome = match dream::run_dream(&ctx, d, opts.clone()).await {
            Ok(o) => o,
            // --all keeps going past one broken project (nightly job).
            Err(e) if all => {
                failed += 1;
                eprintln!("dream {slug} error: {e:#}");
                continue;
            }
            Err(e) => return Err(e),
        };
        match outcome {
            dream::DreamOutcome::Skipped(why) => println!("skipped {slug}: {why}"),
            dream::DreamOutcome::Busy(why) => {
                busy += 1;
                eprintln!("busy {slug}: {why}");
            }
            dream::DreamOutcome::Done(dr) => match dr.status {
                DreamStatus::Completed => {
                    completed += 1;
                    let s = dr.diff_stats.unwrap_or_default();
                    println!(
                        "dream {} completed: {} sessions mined, +{} -{} ~{} files",
                        dr.id,
                        dream::sessions_mined(&ctx.db, &dr.id)?,
                        s.files_added,
                        s.files_removed,
                        s.files_changed
                    );
                    println!("{}", dr.output_dir.display());
                    if auto_apply {
                        match apply::apply(&ctx, &dr.id, false) {
                            Ok(r) => println!(
                                "applied {}: backup at {}",
                                r.dream_id,
                                r.backup_path.display()
                            ),
                            Err(e @ apply::ApplyError::HashMismatch) => {
                                eprintln!("warning: not applying {}: {e}", dr.id)
                            }
                            Err(e) => {
                                failed += 1;
                                eprintln!("apply {} failed: {e}", dr.id);
                            }
                        }
                    }
                }
                status => {
                    failed += 1;
                    eprintln!(
                        "dream {} {status}: {}",
                        dr.id,
                        dr.error.as_deref().unwrap_or("-")
                    );
                    eprintln!("{}", dr.output_dir.display());
                }
            },
        }
    }
    Ok(if failed > 0 {
        EXIT_FAILED
    } else if busy > 0 && !all {
        EXIT_REFUSED
    } else if completed > 0 {
        EXIT_OK
    } else {
        EXIT_SKIPPED
    })
}

/// `dream --dry-run`: one line per project (`plan`, `deferred`, `skipped`),
/// each planned session's reduced size, then the totals. Exit 2 when
/// nothing would run.
fn print_plan(ctx: &dream::Ctx, plan: &discover::RunPlan, explicit: Option<&[String]>) -> i32 {
    for item in &plan.items {
        match item {
            discover::PlanItem::Run { project, take } => {
                let sessions: Vec<std::path::PathBuf> = match explicit {
                    Some(ids) => {
                        let dir = project
                            .project
                            .config_dir
                            .join("projects")
                            .join(&project.project.slug);
                        ids.iter()
                            .map(|id| dir.join(format!("{id}.jsonl")))
                            .collect()
                    }
                    None => project
                        .candidate_sessions
                        .iter()
                        .take(*take)
                        .map(|s| s.path.clone())
                        .collect(),
                };
                let n = if explicit.is_some() {
                    *take
                } else {
                    project.candidate_sessions.len()
                };
                println!("plan {}: {take} of {n} sessions", project.project.slug);
                for p in sessions {
                    let id = p.file_stem().unwrap_or_default().to_string_lossy();
                    match transcript::reduce(&p, ctx.cfg.worker.max_transcript_bytes) {
                        Ok(dg) => println!(
                            "  {id}  {} → {} bytes{}",
                            dg.bytes_in,
                            dg.bytes_out,
                            if dg.elided { " (elided)" } else { "" }
                        ),
                        Err(e) => println!("  {id}  unreadable: {e:#}"),
                    }
                }
            }
            discover::PlanItem::Deferred(d) => println!(
                "deferred {}: run budget exhausted ({} candidates)",
                d.project.slug,
                d.candidate_sessions.len()
            ),
            discover::PlanItem::Skipped(d, why) => println!("skipped {}: {why}", d.project.slug),
        }
    }
    let (p, s) = (plan.projects(), plan.sessions());
    println!("plan: {p} projects, {s} worker calls, {p} aggregator calls");
    if p == 0 {
        EXIT_SKIPPED
    } else {
        EXIT_OK
    }
}

/// `diff` / `summary`: print a dream dir artifact verbatim.
fn artifact_cmd(cfg: &config::Config, id: &str, file: &str) -> anyhow::Result<i32> {
    let db = Db::open(&cfg.db_path())?;
    let Some(d) = db.get_dream(id)? else {
        eprintln!("dream not found: {id}");
        return Ok(EXIT_REFUSED);
    };
    let path = dream::dir_of(&d).join(file);
    match std::fs::read(&path) {
        Ok(bytes) => {
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
            Ok(EXIT_OK)
        }
        Err(_) => {
            eprintln!("no {file} for {id} (dream is {})", d.status);
            Ok(EXIT_REFUSED)
        }
    }
}

/// `apply` (`force` = `Some`) / `rollback` (`None`): print the paths touched;
/// refused (not found, bad status, hash mismatch) → exit 3, I/O failure → 4.
fn apply_cmd(id: &str, force: Option<bool>) -> anyhow::Result<i32> {
    use apply::ApplyError;
    let ctx = dream::Ctx::from_env()?;
    let res = match force {
        Some(f) => apply::apply(&ctx, id, f),
        None => apply::rollback(&ctx, id),
    };
    match res {
        Ok(r) => {
            if force.is_some() {
                println!(
                    "applied {}: backup at {}",
                    r.dream_id,
                    r.backup_path.display()
                );
                println!("memory_dir {}", r.memory_dir.display());
            } else {
                println!(
                    "rolled back {}: restored {} from {}",
                    r.dream_id,
                    r.memory_dir.display(),
                    r.backup_path.display()
                );
            }
            Ok(EXIT_OK)
        }
        Err(ApplyError::NotFound) => {
            eprintln!("dream not found: {id}");
            Ok(EXIT_REFUSED)
        }
        Err(e @ (ApplyError::BadStatus(_) | ApplyError::HashMismatch)) => {
            eprintln!("refused: {e}");
            Ok(EXIT_REFUSED)
        }
        Err(ApplyError::Io(e)) => {
            let what = if force.is_some() { "apply" } else { "rollback" };
            eprintln!("{what} failed: {e}");
            Ok(EXIT_FAILED)
        }
    }
}

/// `archive`: set a terminal dream aside; pending/running → exit 3.
fn archive_cmd(cfg: &config::Config, id: &str) -> anyhow::Result<i32> {
    let db = Db::open(&cfg.db_path())?;
    sweep(cfg, &db)?;
    match dream::archive(&db, id) {
        Ok(d) => {
            let at = d
                .archived_at
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_default();
            println!("archived {id} ({}, at {at})", d.status);
            Ok(EXIT_OK)
        }
        Err(dream::ArchiveError::NotFound) => {
            eprintln!("dream not found: {id}");
            Ok(EXIT_REFUSED)
        }
        Err(e @ dream::ArchiveError::Active(_)) => {
            eprintln!("refused: {e}");
            Ok(EXIT_REFUSED)
        }
        Err(dream::ArchiveError::Db(e)) => Err(e),
    }
}

/// `cancel`: mark a pending/running dream canceled and SIGTERM its process.
fn cancel_cmd(cfg: &config::Config, id: &str) -> anyhow::Result<i32> {
    let db = Db::open(&cfg.db_path())?;
    sweep(cfg, &db)?;
    let Some(d) = db.get_dream(id)? else {
        eprintln!("dream not found: {id}");
        return Ok(EXIT_REFUSED);
    };
    if !matches!(d.status, DreamStatus::Pending | DreamStatus::Running) {
        eprintln!("refused: dream is {}", d.status);
        return Ok(EXIT_REFUSED);
    }
    db.update_status(id, DreamStatus::Canceled, None)?;
    if let Some(pid) = d.pid.filter(|p| *p != std::process::id()) {
        if dreamd::lock::pid_alive(pid) {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }
    println!("canceled {id}");
    Ok(EXIT_OK)
}

/// Fail dreams orphaned by a dead process; one stderr line per dream.
fn sweep(cfg: &config::Config, db: &Db) -> anyhow::Result<()> {
    for id in dream::sweep_orphans_in(cfg, db)? {
        eprintln!("swept orphaned dream {id}");
    }
    Ok(())
}

/// Human-readable `show` output.
fn render_dream(d: &Dream) -> String {
    let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    let mut out = String::new();
    let mut line = |k: &str, v: String| out.push_str(&format!("{k:<13}{v}\n"));
    line("id", d.id.clone());
    line("status", d.status.to_string());
    line("project", d.project.key());
    line(
        "cwd",
        opt(d.project.cwd.as_ref().map(|p| p.display().to_string())),
    );
    line("memory_dir", d.project.memory_dir.display().to_string());
    line("model", d.model.clone());
    line("created_at", d.created_at.to_rfc3339());
    line("ended_at", opt(d.ended_at.map(|t| t.to_rfc3339())));
    line("sessions", d.session_ids.len().to_string());
    line(
        "usage",
        format!(
            "{} in / {} out tokens, cost_usd={:.4}",
            d.usage.input_tokens, d.usage.output_tokens, d.usage.cost_usd
        ),
    );
    line(
        "diff",
        opt(d.diff_stats.map(|s| {
            format!(
                "+{} -{} ~{} files",
                s.files_added, s.files_removed, s.files_changed
            )
        })),
    );
    line("output_dir", d.output_dir.display().to_string());
    line("instructions", opt(d.instructions.clone()));
    line("error", opt(d.error.clone()));
    line("archived_at", opt(d.archived_at.map(|t| t.to_rfc3339())));
    if let Some(s) = &d.summary {
        out.push_str(&format!("\n{s}\n"));
    }
    out
}
