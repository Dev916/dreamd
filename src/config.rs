//! Configuration: defaults from the spec, optional TOML file, env overrides.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Effective dreamd configuration. Every field has a spec default, so a
/// partial (or missing) config file is fine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub config_dirs: Vec<PathBuf>,
    pub claude_bin: String,
    pub min_hours: u64,
    pub min_sessions: usize,
    pub exclude_slugs: Vec<String>,
    pub port: u16,
    pub data_dir: PathBuf,
    pub worker: WorkerCfg,
    pub aggregator: AggCfg,
    pub schedule: ScheduleCfg,
}

/// Per-session miner settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerCfg {
    pub model: String,
    pub effort: String,
    pub concurrency: usize,
    pub timeout_secs: u64,
    pub max_transcript_bytes: usize,
}

/// Aggregator settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AggCfg {
    pub model: String,
    pub effort: String,
    pub timeout_secs: u64,
}

/// Nightly launchd schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleCfg {
    pub hour: u8,
    pub minute: u8,
    pub auto_apply: bool,
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

impl Default for Config {
    fn default() -> Self {
        let h = home();
        Config {
            config_dirs: vec![
                h.join(".claude"),
                h.join(".claude-revenium"),
                h.join(".claude-clever"),
            ],
            claude_bin: "claude".into(),
            min_hours: 24,
            min_sessions: 3,
            exclude_slugs: Vec::new(),
            port: 7720,
            data_dir: default_data_dir(),
            worker: WorkerCfg::default(),
            aggregator: AggCfg::default(),
            schedule: ScheduleCfg::default(),
        }
    }
}

impl Default for WorkerCfg {
    fn default() -> Self {
        WorkerCfg {
            model: "sonnet".into(),
            effort: "low".into(),
            concurrency: 3,
            timeout_secs: 600,
            max_transcript_bytes: 200_000,
        }
    }
}

impl Default for AggCfg {
    fn default() -> Self {
        AggCfg {
            model: "opus".into(),
            effort: "high".into(),
            timeout_secs: 1800,
        }
    }
}

impl Default for ScheduleCfg {
    fn default() -> Self {
        ScheduleCfg {
            hour: 3,
            minute: 30,
            auto_apply: false,
        }
    }
}

impl Config {
    /// `<data_dir>/dreams`, one subdirectory per dream.
    #[allow(dead_code)] // used by the dream pipeline
    pub fn dreams_dir(&self) -> PathBuf {
        self.data_dir.join("dreams")
    }

    /// `<data_dir>/dreamd.db`.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("dreamd.db")
    }

    /// The effective config rendered as TOML.
    pub fn to_toml(&self) -> anyhow::Result<String> {
        toml::to_string_pretty(self).context("serialize config")
    }
}

/// `~/.local/share/dreamd` (spec location, also on macOS).
pub fn default_data_dir() -> PathBuf {
    home().join(".local/share/dreamd")
}

/// `$DREAMD_CONFIG` or `~/.config/dreamd/config.toml`.
pub fn config_path() -> PathBuf {
    match std::env::var_os("DREAMD_CONFIG") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home().join(".config/dreamd/config.toml"),
    }
}

/// Expand a leading `~` to the home directory.
pub fn expand_tilde(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => p.to_path_buf(),
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Load config: file (missing = defaults), then env overrides
/// (`DREAMD_CONFIG_DIRS` colon-separated, `DREAMD_CLAUDE_BIN`, `DREAMD_DATA_DIR`).
pub fn load() -> anyhow::Result<Config> {
    let path = config_path();
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<Config>(&text)
            .with_context(|| format!("parse config {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(e).with_context(|| format!("read config {}", path.display())),
    };
    if let Some(dirs) = env_nonempty("DREAMD_CONFIG_DIRS") {
        cfg.config_dirs = dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    if let Some(bin) = env_nonempty("DREAMD_CLAUDE_BIN") {
        cfg.claude_bin = bin;
    }
    if let Some(d) = env_nonempty("DREAMD_DATA_DIR") {
        cfg.data_dir = PathBuf::from(d);
    }
    cfg.config_dirs = cfg.config_dirs.iter().map(|p| expand_tilde(p)).collect();
    cfg.data_dir = expand_tilde(&cfg.data_dir);
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        for k in [
            "DREAMD_CONFIG",
            "DREAMD_DATA_DIR",
            "DREAMD_CONFIG_DIRS",
            "DREAMD_CLAUDE_BIN",
        ] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.min_hours, 24);
        assert_eq!(c.min_sessions, 3);
        assert_eq!(c.port, 7720);
        assert_eq!(c.claude_bin, "claude");
        assert_eq!(c.worker.model, "sonnet");
        assert_eq!(c.worker.effort, "low");
        assert_eq!(c.worker.concurrency, 3);
        assert_eq!(c.worker.timeout_secs, 600);
        assert_eq!(c.worker.max_transcript_bytes, 200_000);
        assert_eq!(c.aggregator.model, "opus");
        assert_eq!(c.aggregator.effort, "high");
        assert_eq!(c.aggregator.timeout_secs, 1800);
        assert_eq!(c.schedule.hour, 3);
        assert_eq!(c.schedule.minute, 30);
        assert!(!c.schedule.auto_apply);
        assert_eq!(c.config_dirs.len(), 3);
        assert!(c.config_dirs[0].ends_with(".claude"));
        assert_eq!(c.dreams_dir(), c.data_dir.join("dreams"));
        assert_eq!(c.db_path(), c.data_dir.join("dreamd.db"));
    }

    #[test]
    fn env_overrides_config_dirs_and_bin() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("DREAMD_CONFIG", tmp.path().join("missing.toml"));
        std::env::set_var("DREAMD_CONFIG_DIRS", "/tmp/a:/tmp/b");
        std::env::set_var("DREAMD_CLAUDE_BIN", "/x/claude");
        std::env::set_var("DREAMD_DATA_DIR", tmp.path().join("data"));
        let c = load().unwrap();
        clear_env();
        assert_eq!(
            c.config_dirs,
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
        assert_eq!(c.claude_bin, "/x/claude");
        assert_eq!(c.data_dir, tmp.path().join("data"));
        assert_eq!(c.min_hours, 24);
    }

    #[test]
    fn partial_toml_merges_with_defaults() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "min_sessions = 5\n[worker]\nmodel = 'haiku'\n").unwrap();
        std::env::set_var("DREAMD_CONFIG", &file);
        let c = load().unwrap();
        clear_env();
        assert_eq!(c.worker.model, "haiku");
        assert_eq!(c.worker.concurrency, 3);
        assert_eq!(c.min_sessions, 5);
        assert_eq!(c.min_hours, 24);
    }

    #[test]
    fn tilde_paths_expand() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "config_dirs = ['~/.claude-x']\n").unwrap();
        std::env::set_var("DREAMD_CONFIG", &file);
        let c = load().unwrap();
        clear_env();
        let home = dirs::home_dir().unwrap();
        assert_eq!(c.config_dirs, vec![home.join(".claude-x")]);
    }

    #[test]
    fn malformed_toml_is_an_error() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "min_hours = \"lots\"\n").unwrap();
        std::env::set_var("DREAMD_CONFIG", &file);
        let r = load();
        clear_env();
        assert!(r.is_err());
    }
}
