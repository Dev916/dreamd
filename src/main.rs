mod aggregate;
mod apply;
mod claude;
mod cli;
mod config;
mod db;
mod discover;
mod doctor;
mod dream;
mod fsops;
mod model;
mod notify;
mod postcheck;
mod transcript;
mod worker;

use clap::Parser;

use claude::{Claude, ClaudeRequest, CliClaude};
use cli::{Cli, Command, ConfigAction, EXIT_FAILED, EXIT_OK, EXIT_REFUSED, EXIT_SKIPPED};
use db::Db;
use model::{Dream, DreamStatus};

fn main() {
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
        Command::List => {
            let db = Db::open(&cfg.db_path())?;
            let dreams = db.list_dreams(None)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&dreams)?);
            } else if dreams.is_empty() {
                println!("no dreams");
            } else {
                for d in &dreams {
                    println!(
                        "{}  {:<11}  {}  {}  {}",
                        d.id,
                        d.status,
                        d.created_at.format("%Y-%m-%d %H:%M"),
                        d.project.slug,
                        d.model
                    );
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
        } => {
            let opts = dream::DreamOpts {
                force,
                model,
                instructions,
                session_ids: (!sessions.is_empty()).then_some(sessions),
                dry_run,
            };
            dream_cmd(selector.as_deref(), all, opts).await
        }
        Command::Diff { id } => artifact_cmd(&cfg, &id, "dream.diff"),
        Command::Summary { id } => artifact_cmd(&cfg, &id, "aggregate.md"),
        Command::Cancel { id } => cancel_cmd(&cfg, &id),
        Command::Apply { id, force } => apply_cmd(&id, Some(force)),
        Command::Rollback { id } => apply_cmd(&id, None),
        Command::Scan { all_dirs } => {
            let db = Db::open(&cfg.db_path())?;
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
        _ => anyhow::bail!("not implemented"),
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

/// Find the projects a `dream` selector names: a slug, or a path compared
/// with each project's cwd. `None` selector = every project (`--all`).
fn select_projects(
    found: Vec<discover::DiscoveredProject>,
    selector: Option<&str>,
) -> Vec<discover::DiscoveredProject> {
    let Some(sel) = selector else {
        return found;
    };
    let as_path = std::path::Path::new(sel);
    let canon = std::fs::canonicalize(as_path).ok();
    let by_slug: Vec<_> = found
        .iter()
        .filter(|d| d.project.slug == sel)
        .cloned()
        .collect();
    if !by_slug.is_empty() {
        return by_slug.into_iter().take(1).collect();
    }
    found
        .into_iter()
        .filter(|d| {
            d.project
                .cwd
                .as_deref()
                .is_some_and(|c| c == as_path || canon.as_deref().is_some_and(|k| k == c))
        })
        .take(1)
        .collect()
}

/// `dream <selector>|--all`: run the pipeline, print one line per project.
async fn dream_cmd(
    selector: Option<&str>,
    all: bool,
    opts: dream::DreamOpts,
) -> anyhow::Result<i32> {
    let ctx = dream::Ctx::from_env()?;
    let found = discover::discover(&ctx.cfg, &ctx.db, chrono::Utc::now())?;
    let targets = select_projects(found, if all { None } else { selector });
    if targets.is_empty() {
        if all {
            println!("no dreamable projects");
            return Ok(EXIT_SKIPPED);
        }
        eprintln!("project not found: {}", selector.unwrap_or_default());
        return Ok(EXIT_REFUSED);
    }
    let (mut completed, mut failed) = (0usize, 0usize);
    for d in targets {
        let slug = d.project.slug.clone();
        match dream::run_dream(&ctx, d, opts.clone()).await? {
            dream::DreamOutcome::Skipped(_) if opts.dry_run => completed += 1,
            dream::DreamOutcome::Skipped(why) => println!("skipped {slug}: {why}"),
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
    } else if completed > 0 {
        EXIT_OK
    } else {
        EXIT_SKIPPED
    })
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

/// `cancel`: mark a pending/running dream canceled and SIGTERM its process.
fn cancel_cmd(cfg: &config::Config, id: &str) -> anyhow::Result<i32> {
    let db = Db::open(&cfg.db_path())?;
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
        if pid_alive(pid) {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }
    println!("canceled {id}");
    Ok(EXIT_OK)
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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
    if let Some(s) = &d.summary {
        out.push_str(&format!("\n{s}\n"));
    }
    out
}
