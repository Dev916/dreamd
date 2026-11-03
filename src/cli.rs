//! Command-line surface: every subcommand from the spec.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

pub const EXIT_OK: i32 = 0;
/// Gate-skipped / nothing to do.
pub const EXIT_SKIPPED: i32 = 2;
/// Refused (hash mismatch, bad status, unknown dream).
pub const EXIT_REFUSED: i32 = 3;
/// Dream failed (also: a `claude-probe` call failed).
pub const EXIT_FAILED: i32 = 4;

#[derive(Debug, Parser)]
#[command(
    name = "dreamd",
    version,
    about = "Local dream machine for Claude Code memory"
)]
pub struct Cli {
    /// Emit JSON instead of human-readable text (read commands).
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List projects, sessions since last dream, and the gate verdict.
    Scan {
        /// Include every configured config dir (not just ones with dreamable projects).
        #[arg(long)]
        all_dirs: bool,
    },
    /// Dream a project (by slug or cwd) or every project with --all.
    Dream {
        /// Project slug or cwd.
        #[arg(
            required_unless_present = "all",
            conflicts_with = "all",
            allow_hyphen_values = true
        )]
        selector: Option<String>,
        /// Dream every dreamable project (gate applied per project).
        #[arg(long)]
        all: bool,
        /// Bypass the gate (awaiting review, min_hours, min_sessions).
        #[arg(long)]
        force: bool,
        /// Override the aggregator model.
        #[arg(long)]
        model: Option<String>,
        /// Operator instructions appended to the aggregator prompt.
        #[arg(long)]
        instructions: Option<String>,
        /// Explicit session ids (comma-separated); bypasses the gate.
        #[arg(long, value_delimiter = ',')]
        sessions: Vec<String>,
        /// Print the run plan (projects, sessions, model calls); run nothing.
        #[arg(long)]
        dry_run: bool,
        /// Only mine transcripts modified in the last N days, overriding
        /// `limits.backfill_days` and the last-dream cutoff (0 = no window).
        #[arg(long, value_name = "N")]
        since_days: Option<u64>,
        /// Mine at most N sessions per dream (overrides
        /// `limits.max_sessions_per_dream`).
        #[arg(long, value_name = "N")]
        max_sessions: Option<usize>,
    },
    /// List dreams (archived ones only with --include-archived).
    List {
        /// Also show archived dreams (marked `archived`).
        #[arg(long)]
        include_archived: bool,
    },
    /// Show one dream.
    Show { id: String },
    /// Print a dream's unified diff.
    Diff { id: String },
    /// Print a dream's aggregator summary.
    Summary { id: String },
    /// Cancel a pending or running dream.
    Cancel { id: String },
    /// Apply a completed dream to the live memory dir.
    Apply {
        id: String,
        /// Apply even if the live memory dir changed since the snapshot.
        #[arg(long)]
        force: bool,
    },
    /// Restore the pre-apply backup of an applied dream.
    Rollback { id: String },
    /// Set a finished dream aside without applying it (hides it from
    /// `list` and unparks its project).
    Archive { id: String },
    /// Run the loopback HTTP API.
    Serve {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Print the API bearer token (creating `<data_dir>/api-token` if absent).
    Token,
    /// Manage the nightly launchd job (dev.dreamd.nightly).
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    /// Check claude on PATH, config dirs, data dir, database.
    Doctor,
    /// Configuration commands.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// (debug) Reduce one transcript JSONL to a digest.
    #[command(hide = true)]
    Digest {
        path: PathBuf,
        #[arg(long)]
        max_bytes: Option<usize>,
    },
    /// (debug) Send one prompt through the Claude runner.
    #[command(hide = true)]
    ClaudeProbe {
        prompt: String,
        #[arg(long)]
        model: Option<String>,
        /// Kill claude and fail after this many seconds (default: worker timeout).
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// (debug) Run the worker fan-out over transcripts into a dream dir.
    #[command(hide = true)]
    Mine {
        #[arg(long)]
        dream_dir: PathBuf,
        #[arg(required = true)]
        transcripts: Vec<PathBuf>,
        /// Per-worker timeout in seconds (default: worker timeout_secs).
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// (debug) Post-check an out/ dir against its in/ snapshot.
    #[command(hide = true)]
    Postcheck { in_dir: PathBuf, out_dir: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum ScheduleAction {
    /// Write ~/Library/LaunchAgents/dev.dreamd.nightly.plist and load it.
    Install {
        /// Print the plist that would be written; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Unload and remove the plist.
    Uninstall,
    /// Print whether the job is installed and when it runs.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print the effective config as TOML.
    Show,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn dream_requires_selector_or_all() {
        assert!(Cli::try_parse_from(["dreamd", "dream"]).is_err());
        assert!(Cli::try_parse_from(["dreamd", "dream", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["dreamd", "dream", "x", "--all"]).is_err());
        let c = Cli::try_parse_from(["dreamd", "dream", "x", "--sessions", "a,b"]).unwrap();
        match c.command {
            Command::Dream { sessions, .. } => assert_eq!(sessions, vec!["a", "b"]),
            _ => unreachable!(),
        }
        let c = Cli::try_parse_from(["dreamd", "dream", "-tmp-proj", "--force"]).unwrap();
        match c.command {
            Command::Dream {
                selector, force, ..
            } => {
                assert_eq!(selector.as_deref(), Some("-tmp-proj"));
                assert!(force);
            }
            _ => unreachable!(),
        }
    }
}
