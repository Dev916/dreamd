mod cli;
mod config;
mod db;
mod doctor;
mod model;

use clap::Parser;

use cli::{Cli, Command, ConfigAction, EXIT_OK, EXIT_REFUSED};
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
        _ => anyhow::bail!("not implemented"),
    }
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
