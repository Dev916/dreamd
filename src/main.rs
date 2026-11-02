mod claude;
mod cli;
mod config;
mod db;
mod discover;
mod doctor;
mod fsops;
mod model;
mod postcheck;
mod transcript;
mod worker;

use clap::Parser;

use claude::{Claude, ClaudeRequest, CliClaude};
use cli::{Cli, Command, ConfigAction, EXIT_FAILED, EXIT_OK, EXIT_REFUSED};
use db::Db;
use model::Dream;

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
            }
            Ok(EXIT_OK)
        }
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
            "{} in / {} out tokens, ${:.4}",
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
