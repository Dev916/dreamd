mod cli;
mod config;
mod doctor;

use clap::Parser;

use cli::{Cli, Command, ConfigAction, EXIT_OK};

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
        _ => anyhow::bail!("not implemented"),
    }
}
